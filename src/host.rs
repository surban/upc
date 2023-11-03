//! Host-side USB packet channel (UPC).
//!
//! To open a channel, use [`rusb`] to find the target device and then pass it to [`connect`].
//!

use std::{
    fmt,
    future::Future,
    mem::take,
    sync::{Arc, Mutex},
    thread::{self, JoinHandle},
    time::Duration,
};

use rusb::{
    request_type, Device, DeviceHandle, Direction, Error, PrimaryLanguage, Recipient, RequestType, Result,
    SubLanguage, UsbContext,
};
use tokio::{
    sync::{mpsc, oneshot, oneshot::error::TryRecvError},
    task::spawn_blocking,
};

use crate::{Class, CTRL_REQ_CLOSE, CTRL_REQ_INFO, CTRL_REQ_OPEN, INFO_SIZE, MAX_SIZE};

const IN_REQUEST: u8 = request_type(Direction::In, RequestType::Vendor, Recipient::Interface);
const OUT_REQUEST: u8 = request_type(Direction::Out, RequestType::Vendor, Recipient::Interface);

const TIMEOUT: Duration = Duration::from_secs(1);

/// Finds the interface by class and optional interface name.
///
/// Returns the interface number.
pub fn find_interface<C: UsbContext>(dev: &Device<C>, class: Class, name: Option<impl AsRef<str>>) -> Result<u8> {
    let name = name.as_ref().map(|s| s.as_ref());
    let cfg = dev.active_config_descriptor()?;
    for iface in cfg.interfaces() {
        for desc in iface.descriptors() {
            if desc.class_code() == class.class
                && desc.sub_class_code() == class.sub_class
                && desc.protocol_code() == class.protocol
            {
                if let Some(name) = name {
                    let Some(idx) = desc.description_string_index() else { continue };
                    let Ok(if_name) = read_string(dev, idx) else { continue };
                    if if_name != name {
                        continue;
                    }
                }

                return Ok(desc.interface_number());
            }
        }
    }
    Err(Error::NotFound)
}

fn read_string<C: UsbContext>(dev: &Device<C>, idx: u8) -> Result<String> {
    let hnd = dev.open()?;
    let langs = hnd.read_languages(TIMEOUT)?;
    match langs.into_iter().find(|lang| {
        lang.primary_language() == PrimaryLanguage::English && lang.sub_language() == SubLanguage::UnitedStates
    }) {
        Some(lang) => hnd.read_string_descriptor(lang, idx, TIMEOUT),
        None => Err(Error::NotFound),
    }
}

/// Read the device-provided information on the specified device and interface.
///
/// The maximum size of the data is [`INFO_SIZE`].
pub fn info<C: UsbContext + 'static>(dev: &Device<C>, interface: u8) -> Result<Vec<u8>> {
    // Open device and claim interface.
    let mut hnd = dev.open()?;
    hnd.claim_interface(interface)?;

    // Read info.
    tracing::debug!("reading info");
    let mut info = vec![0; INFO_SIZE];
    let n = hnd.read_control(IN_REQUEST, CTRL_REQ_INFO, 0, interface.into(), &mut info, TIMEOUT)?;
    info.truncate(n);

    Ok(info)
}

/// Sends data into a USB packet channel.
pub struct UpcSender {
    tx: mpsc::Sender<Vec<u8>>,
    shared: Arc<UpcShared>,
}

impl fmt::Debug for UpcSender {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_tuple("UpcSender").field(&self.shared.name).finish()
    }
}

impl UpcSender {
    /// Send packet.
    ///
    /// ## Cancel safety
    /// If canceled, no data will have been sent.
    pub async fn send(&self, data: Vec<u8>) -> Result<()> {
        match self.tx.send(data).await {
            Ok(()) => Ok(()),
            Err(_) => Err(*self.shared.error.lock().unwrap()),
        }
    }

    /// Wait until connection is closed.
    pub fn closed(&self) -> impl Future<Output = ()> {
        let tx = self.tx.clone();
        async move { tx.closed().await }
    }
}

/// Receives data from a USB packet channel.
pub struct UpcReceiver {
    rx: mpsc::Receiver<Vec<u8>>,
    shared: Arc<UpcShared>,
    buffer: Vec<u8>,
    max_size: usize,
    max_transfer_size: usize,
}

impl fmt::Debug for UpcReceiver {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_tuple("UpcReceiver").field(&self.shared.name).finish()
    }
}

impl UpcReceiver {
    /// Receive packet.
    ///
    /// ## Cancel safety
    /// If canceled, no data will have been removed from the receive queue.
    pub async fn recv(&mut self) -> Result<Vec<u8>> {
        loop {
            let packet = self.rx.recv().await.ok_or(Error::Pipe)?;
            self.buffer.extend_from_slice(&packet);

            if self.buffer.len() > self.max_size {
                self.buffer.clear();
                return Err(Error::Overflow);
            }

            if packet.len() < self.max_transfer_size {
                return Ok(take(&mut self.buffer));
            }
        }
    }

    /// Sets the maximum packet size.
    pub fn set_max_size(&mut self, max_size: usize) {
        self.max_size = max_size;
    }
}

struct UpcShared {
    name: String,
    error: Arc<Mutex<Error>>,
    in_thread: Option<JoinHandle<()>>,
    out_thread: Option<JoinHandle<()>>,
    stop_tx: Option<oneshot::Sender<()>>,
}

impl Drop for UpcShared {
    fn drop(&mut self) {
        tracing::debug!("waiting for IO threads");
        let _ = self.stop_tx.take().unwrap().send(());
        self.in_thread.take().unwrap().join().expect("in thread panicked");
        self.out_thread.take().unwrap().join().expect("out thread panicked");
        tracing::debug!("IO threads finished");
    }
}

/// Connect to the specified device and interface.
///
/// Use [`find_interface`] to determine the `interface` number.
/// The `topic` is provided to the device and may contain user-defined data.
///
/// # Panics
/// Panics if the size of `topic` is larger than [`INFO_SIZE`].
pub async fn connect<C: UsbContext + 'static>(
    dev: &Device<C>, interface: u8, topic: &[u8],
) -> Result<(UpcSender, UpcReceiver)> {
    assert!(topic.len() <= INFO_SIZE, "topic too big");

    // Get endpoints.
    let mut ep_in = None;
    let mut ep_out = None;
    let mut max_packet_size = usize::MAX;
    {
        let cfg = dev.active_config_descriptor()?;
        let iface_desc = cfg.interfaces().find(|i| i.number() == interface).ok_or(Error::NotFound)?;
        for desc in iface_desc.descriptors() {
            for ep in desc.endpoint_descriptors() {
                max_packet_size = max_packet_size.min(ep.max_packet_size().into());
                match ep.direction() {
                    Direction::In => ep_in = Some(ep.address()),
                    Direction::Out => ep_out = Some(ep.address()),
                }
            }
        }
    }
    let (Some(ep_in), Some(ep_out)) = (ep_in, ep_out) else { return Err(Error::NotFound) };
    let max_transfer_size = max_packet_size * 128;

    // Open device and claim interface.
    let mut hnd = dev.open()?;
    hnd.claim_interface(interface)?;
    hnd.clear_halt(ep_in)?;
    hnd.clear_halt(ep_out)?;
    let hnd = Arc::new(hnd);

    // Open connection.
    tracing::debug!("opening connection");
    let hnd_task = hnd.clone();
    let topic = topic.to_vec();
    spawn_blocking(move || {
        // Close previous connection and flush buffer.
        let mut buf = vec![0; max_transfer_size];
        hnd_task.write_control(OUT_REQUEST, CTRL_REQ_CLOSE, 0, interface.into(), &[], TIMEOUT)?;
        while let Ok(n) = hnd_task.read_bulk(ep_in, &mut buf, Duration::from_millis(10)) {
            if n == 0 {
                break;
            }
        }

        hnd_task.write_control(OUT_REQUEST, CTRL_REQ_OPEN, 0, interface.into(), &topic, TIMEOUT)?;
        Ok(())
    })
    .await
    .unwrap()?;
    tracing::debug!("connection is open");

    // Start handler threads.
    let error = Arc::new(Mutex::new(Error::Pipe));
    let name = format!("{}-{}:{}", dev.bus_number(), dev.address(), interface);
    let closed = Arc::new(Mutex::new(false));

    let hnd_in = hnd.clone();
    let error_in = error.clone();
    let (tx_in, rx_in) = mpsc::channel(16);
    let in_closed = closed.clone();
    let in_thread = thread::Builder::new()
        .name(format!("UPC {name} in"))
        .spawn(move || in_thread(hnd_in, tx_in, ep_in, interface, error_in, max_transfer_size, in_closed))
        .unwrap();

    let (tx_out, rx_out) = mpsc::channel(16);
    let error_out = error.clone();
    let (stop_tx, stop_rx) = oneshot::channel();
    let out_thread = thread::Builder::new()
        .name(format!("UPC {name} out"))
        .spawn(move || out_thread(hnd, rx_out, ep_out, interface, error_out, stop_rx, max_packet_size, closed))
        .unwrap();

    // Build objects.
    let shared = Arc::new(UpcShared {
        name,
        error,
        in_thread: Some(in_thread),
        out_thread: Some(out_thread),
        stop_tx: Some(stop_tx),
    });
    let sender = UpcSender { tx: tx_out, shared: shared.clone() };
    let recv = UpcReceiver { rx: rx_in, shared, buffer: Vec::new(), max_size: MAX_SIZE, max_transfer_size };

    Ok((sender, recv))
}

fn in_thread<C: UsbContext>(
    hnd: Arc<DeviceHandle<C>>, tx: mpsc::Sender<Vec<u8>>, ep: u8, iface: u8, error: Arc<Mutex<Error>>,
    max_transfer_size: usize, closed: Arc<Mutex<bool>>,
) {
    while !tx.is_closed() {
        let mut buf = vec![0; max_transfer_size];
        match hnd.read_bulk(ep, &mut buf, TIMEOUT) {
            Ok(n) => {
                buf.truncate(n);
                if tx.blocking_send(buf).is_err() {
                    break;
                }
            }
            Err(Error::Timeout) => (),
            Err(err) => {
                tracing::warn!("receiving failed: {err}");
                *error.lock().unwrap() = err;
                break;
            }
        }
    }

    let mut closed = closed.lock().unwrap();
    if !*closed {
        close(&hnd, iface);
        *closed = true;
    }
}

#[allow(clippy::too_many_arguments)]
fn out_thread<C: UsbContext>(
    hnd: Arc<DeviceHandle<C>>, mut rx: mpsc::Receiver<Vec<u8>>, ep: u8, iface: u8, error: Arc<Mutex<Error>>,
    mut stop_rx: oneshot::Receiver<()>, max_packet_size: usize, closed: Arc<Mutex<bool>>,
) {
    'outer: while let Some(data) = rx.blocking_recv() {
        let mut data = data.as_slice();
        loop {
            match stop_rx.try_recv() {
                Err(TryRecvError::Empty) => (),
                _ => break 'outer,
            }

            match hnd.write_bulk(ep, data, TIMEOUT) {
                Ok(n) if n != data.len() => {
                    data = &data[n..];
                }
                Ok(_) => {
                    if data.is_empty() || data.len() % max_packet_size != 0 {
                        break;
                    } else {
                        // Send zero length packet to indicate end of transfer.
                        data = &[];
                    }
                }
                Err(Error::Timeout) => (),
                Err(err) => {
                    tracing::warn!("sending failed: {err}");
                    *error.lock().unwrap() = err;
                    break 'outer;
                }
            }
        }
    }

    let mut closed = closed.lock().unwrap();
    if !*closed {
        close(&hnd, iface);
        *closed = true;
    }
}

fn close<C: UsbContext>(hnd: &DeviceHandle<C>, iface: u8) {
    tracing::debug!("closing connection");
    if let Err(err) = hnd.write_control(OUT_REQUEST, CTRL_REQ_CLOSE, 0, iface.into(), &[], TIMEOUT) {
        tracing::warn!("closing connection failed: {err}");
    }
}

#[cfg(test)]
mod test {
    use super::*;

    /// Verify that connect function is Send.
    #[tokio::test]
    async fn connect_is_send() {
        let dev = {
            let Ok(devs) = rusb::devices() else { return };
            let Some(dev) = devs.iter().next() else { return };
            dev
        };
        tokio::spawn(async move {
            let _ = connect(&dev, 0, &[]).await;
        });
    }
}
