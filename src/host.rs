//! Host-side USB packet channel (UPC).
//!
//! To open a channel, use [`rusb`] to find the target device and then pass it to [`connect`].
//!
//! All errors returned from this module have an inner error type of [`rusb::Error`].
//!

use bytes::{Bytes, BytesMut};
use futures::{sink, stream, Sink, SinkExt, Stream, StreamExt};
use rusb::{request_type, Device, DeviceHandle, Direction, Recipient, RequestType, UsbContext};
use std::{
    collections::HashSet,
    fmt,
    future::Future,
    io::{Error, ErrorKind, Result},
    mem::take,
    pin::Pin,
    sync::{Arc, Mutex, MutexGuard, OnceLock},
    task::{Context, Poll},
    thread::{self, JoinHandle},
    time::Duration,
};
use tokio::{
    sync::{mpsc, oneshot, oneshot::error::TryRecvError},
    task::spawn_blocking,
};

use crate::{Class, CTRL_REQ_CLOSE, CTRL_REQ_INFO, CTRL_REQ_OPEN, INFO_SIZE, MAX_SIZE};

const IN_REQUEST: u8 = request_type(Direction::In, RequestType::Vendor, Recipient::Interface);
const OUT_REQUEST: u8 = request_type(Direction::Out, RequestType::Vendor, Recipient::Interface);

const TIMEOUT: Duration = Duration::from_secs(1);

fn to_io_err(error: rusb::Error) -> Error {
    let kind = match error {
        rusb::Error::Io => ErrorKind::ConnectionAborted,
        rusb::Error::InvalidParam => ErrorKind::InvalidInput,
        rusb::Error::Access => ErrorKind::PermissionDenied,
        rusb::Error::NoDevice => ErrorKind::NotFound,
        rusb::Error::NotFound => ErrorKind::NotFound,
        rusb::Error::Busy => ErrorKind::AddrInUse,
        rusb::Error::Timeout => ErrorKind::TimedOut,
        rusb::Error::Overflow => ErrorKind::OutOfMemory,
        rusb::Error::Pipe => ErrorKind::BrokenPipe,
        rusb::Error::Interrupted => ErrorKind::Interrupted,
        rusb::Error::NoMem => ErrorKind::OutOfMemory,
        rusb::Error::NotSupported => ErrorKind::Unsupported,
        rusb::Error::BadDescriptor => ErrorKind::InvalidInput,
        rusb::Error::Other => ErrorKind::Other,
    };

    Error::new(kind, error)
}

/// Finds the interface by interface class.
///
/// Returns the interface number.
pub fn find_interface<C: UsbContext>(dev: &Device<C>, class: Class) -> Result<u8> {
    let cfg = dev.active_config_descriptor().map_err(to_io_err)?;
    for iface in cfg.interfaces() {
        for desc in iface.descriptors() {
            if desc.class_code() == class.class
                && desc.sub_class_code() == class.sub_class
                && desc.protocol_code() == class.protocol
            {
                return Ok(desc.interface_number());
            }
        }
    }

    Err(Error::new(ErrorKind::NotFound, rusb::Error::NotFound))
}

/// Read the device-provided information on the specified device and interface.
///
/// The maximum size of the data is [`INFO_SIZE`].
pub fn info<C: UsbContext + 'static>(hnd: &DeviceHandle<C>, interface: u8) -> Result<Vec<u8>> {
    // Claim interface.
    hnd.claim_interface(interface).map_err(to_io_err)?;

    // Read info.
    tracing::debug!("reading info");
    let mut info = vec![0; INFO_SIZE];
    let n = hnd
        .read_control(IN_REQUEST, CTRL_REQ_INFO, 0, interface.into(), &mut info, TIMEOUT)
        .map_err(to_io_err)?;
    info.truncate(n);

    Ok(info)
}

/// Sends data into a USB packet channel.
pub struct UpcSender {
    tx: mpsc::Sender<Bytes>,
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
    pub async fn send(&self, data: Bytes) -> Result<()> {
        match self.tx.send(data).await {
            Ok(()) => Ok(()),
            Err(_) => Err(*self.shared.error.lock().unwrap()).map_err(to_io_err),
        }
    }

    /// Wait until connection is closed.
    pub fn closed(&self) -> impl Future<Output = ()> {
        let tx = self.tx.clone();
        async move { tx.closed().await }
    }

    /// Turns this into a sink for packets.
    pub fn into_sink(self) -> UpcSink {
        let sink = sink::unfold(self, |this, data: Bytes| async move {
            this.send(data).await?;
            Ok(this)
        });

        UpcSink(Box::pin(sink))
    }
}

/// Packet sink into a USB packet channel.
pub struct UpcSink(Pin<Box<dyn Sink<Bytes, Error = Error> + Send + Sync + 'static>>);

impl fmt::Debug for UpcSink {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_tuple("UpcSink").finish()
    }
}

impl Sink<Bytes> for UpcSink {
    type Error = Error;

    fn poll_ready(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Result<()>> {
        Pin::into_inner(self).0.poll_ready_unpin(cx)
    }

    fn start_send(self: Pin<&mut Self>, item: Bytes) -> Result<()> {
        Pin::into_inner(self).0.start_send_unpin(item)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Result<()>> {
        Pin::into_inner(self).0.poll_flush_unpin(cx)
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Result<()>> {
        Pin::into_inner(self).0.poll_close_unpin(cx)
    }
}

/// Receives data from a USB packet channel.
pub struct UpcReceiver {
    rx: mpsc::Receiver<BytesMut>,
    shared: Arc<UpcShared>,
    buffer: BytesMut,
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
    pub async fn recv(&mut self) -> Result<BytesMut> {
        loop {
            let packet = self.rx.recv().await.ok_or(rusb::Error::Pipe).map_err(to_io_err)?;
            let packet_len = packet.len();
            self.buffer.unsplit(packet);

            if self.buffer.len() > self.max_size {
                self.buffer.clear();
                return Err(rusb::Error::Overflow).map_err(to_io_err);
            }

            if packet_len < self.max_transfer_size {
                return Ok(take(&mut self.buffer));
            }
        }
    }

    /// Sets the maximum packet size.
    pub fn set_max_size(&mut self, max_size: usize) {
        self.max_size = max_size;
    }

    /// Turns this into a stream of packets.
    pub fn into_stream(self) -> UpcStream {
        let stream = stream::try_unfold(self, |mut this| async move {
            match this.recv().await {
                Ok(data) => Ok(Some((data, this))),
                Err(err) if err.kind() == ErrorKind::ConnectionReset => Ok(None),
                Err(err) => Err(Error::new(ErrorKind::ConnectionReset, err)),
            }
        });

        UpcStream(Box::pin(stream))
    }
}

/// Packet stream from a USB packet channel.
pub struct UpcStream(Pin<Box<dyn Stream<Item = Result<BytesMut>> + Send + Sync + 'static>>);

impl fmt::Debug for UpcStream {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_tuple("UpcStream").finish()
    }
}

impl Stream for UpcStream {
    type Item = Result<BytesMut>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Option<Self::Item>> {
        Pin::into_inner(self).0.poll_next_unpin(cx)
    }
}

static IN_USE: OnceLock<Mutex<HashSet<(usize, u8)>>> = OnceLock::new();
fn in_use() -> MutexGuard<'static, HashSet<(usize, u8)>> {
    IN_USE.get_or_init(|| Mutex::new(HashSet::new())).lock().unwrap()
}

struct InUseGuard {
    handle: usize,
    interface: u8,
}

impl InUseGuard {
    fn new(handle: usize, interface: u8) -> Result<Self> {
        let mut in_use = in_use();

        if in_use.contains(&(handle, interface)) {
            return Err(to_io_err(rusb::Error::Busy));
        }

        in_use.insert((handle, interface));

        Ok(Self { handle, interface })
    }
}

impl Drop for InUseGuard {
    fn drop(&mut self) {
        let mut in_use = in_use();
        in_use.remove(&(self.handle, self.interface));
    }
}

struct UpcShared {
    name: String,
    error: Arc<Mutex<rusb::Error>>,
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
    hnd: Arc<DeviceHandle<C>>, interface: u8, topic: &[u8],
) -> Result<(UpcSender, UpcReceiver)> {
    assert!(topic.len() <= INFO_SIZE, "topic too big");

    let guard = Arc::new(InUseGuard::new(hnd.as_raw() as _, interface)?);
    let dev = hnd.device();

    // Get endpoints.
    let mut ep_in = None;
    let mut ep_out = None;
    let mut max_packet_size = usize::MAX;
    {
        let cfg = dev.active_config_descriptor().map_err(to_io_err)?;
        let iface_desc =
            cfg.interfaces().find(|i| i.number() == interface).ok_or(rusb::Error::NotFound).map_err(to_io_err)?;
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
    let (Some(ep_in), Some(ep_out)) = (ep_in, ep_out) else {
        return Err(rusb::Error::NotFound).map_err(to_io_err);
    };
    let max_transfer_size = max_packet_size * 128;

    // Open device and claim interface.
    hnd.claim_interface(interface).map_err(to_io_err)?;
    hnd.clear_halt(ep_in).map_err(to_io_err)?;
    hnd.clear_halt(ep_out).map_err(to_io_err)?;

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
    .unwrap()
    .map_err(to_io_err)?;
    tracing::debug!("connection is open");

    // Start handler threads.
    let error = Arc::new(Mutex::new(rusb::Error::Pipe));
    let name = format!("{}-{}:{}", dev.bus_number(), dev.address(), interface);
    let closed = Arc::new(Mutex::new(false));

    let hnd_in = hnd.clone();
    let error_in = error.clone();
    let (tx_in, rx_in) = mpsc::channel(16);
    let in_closed = closed.clone();
    let in_guard = guard.clone();
    let in_thread = thread::Builder::new()
        .name(format!("UPC {name} in"))
        .spawn(move || {
            let _in_guard = in_guard;
            in_thread(hnd_in, tx_in, ep_in, interface, error_in, max_transfer_size, in_closed)
        })
        .unwrap();

    let (tx_out, rx_out) = mpsc::channel(16);
    let error_out = error.clone();
    let (stop_tx, stop_rx) = oneshot::channel();
    let out_thread = thread::Builder::new()
        .name(format!("UPC {name} out"))
        .spawn(move || {
            let _out_guard = guard;
            out_thread(hnd, rx_out, ep_out, interface, error_out, stop_rx, max_packet_size, closed)
        })
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
    let recv = UpcReceiver { rx: rx_in, shared, buffer: BytesMut::new(), max_size: MAX_SIZE, max_transfer_size };

    Ok((sender, recv))
}

fn in_thread<C: UsbContext>(
    hnd: Arc<DeviceHandle<C>>, tx: mpsc::Sender<BytesMut>, ep: u8, iface: u8, error: Arc<Mutex<rusb::Error>>,
    max_transfer_size: usize, closed: Arc<Mutex<bool>>,
) {
    while !tx.is_closed() {
        let mut buf = BytesMut::zeroed(max_transfer_size);
        match hnd.read_bulk(ep, &mut buf, TIMEOUT) {
            Ok(n) => {
                #[cfg(feature = "trace-packets")]
                tracing::trace!("Received packet of {n} bytes");
                buf.truncate(n);
                if tx.blocking_send(buf).is_err() {
                    break;
                }
            }
            Err(rusb::Error::Timeout) => (),
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
    hnd: Arc<DeviceHandle<C>>, mut rx: mpsc::Receiver<Bytes>, ep: u8, iface: u8, error: Arc<Mutex<rusb::Error>>,
    mut stop_rx: oneshot::Receiver<()>, max_packet_size: usize, closed: Arc<Mutex<bool>>,
) {
    'outer: while let Some(mut data) = rx.blocking_recv() {
        loop {
            match stop_rx.try_recv() {
                Err(TryRecvError::Empty) => (),
                _ => break 'outer,
            }

            match hnd.write_bulk(ep, &data, TIMEOUT) {
                Ok(n) if n != data.len() => {
                    #[cfg(feature = "trace-packets")]
                    tracing::trace!("Sent packet of {n} bytes");
                    let _ = data.split_to(n);
                }
                Ok(_) => {
                    #[cfg(feature = "trace-packets")]
                    tracing::trace!("Sent packet of {} bytes", data.len());
                    if data.is_empty() || data.len() % max_packet_size != 0 {
                        break;
                    } else {
                        // Send zero length packet to indicate end of transfer.
                        data = Bytes::new();
                    }
                }
                Err(rusb::Error::Timeout) => (),
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
        let hnd = {
            let Ok(devs) = rusb::devices() else { return };
            let Some(dev) = devs.iter().next() else { return };
            let Ok(hnd) = dev.open() else { return };
            Arc::new(hnd)
        };
        tokio::spawn(async move {
            let _ = connect(hnd, 0, &[]).await;
        });
    }
}
