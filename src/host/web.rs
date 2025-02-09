//! Web host backend

use bytes::{Bytes, BytesMut};
use std::{
    cell::Cell,
    io::{Error, ErrorKind, Result},
    rc::Rc,
    sync::{Arc, Mutex},
};
use tokio::sync::{mpsc, oneshot, oneshot::error::TryRecvError};
use wasm_bindgen_futures::spawn_local;
use webusb_web::{OpenUsbDevice, UsbControlRequest, UsbDevice, UsbDirection, UsbRecipient, UsbRequestType};

use super::{UpcReceiver, UpcSender};
use crate::{host::InUseGuard, Class, CTRL_REQ_CLOSE, CTRL_REQ_INFO, CTRL_REQ_OPEN, INFO_SIZE, MAX_SIZE};

pub(crate) fn to_io_err(error: webusb_web::Error) -> Error {
    let kind = match error.kind() {
        webusb_web::ErrorKind::Unsupported => ErrorKind::Unsupported,
        webusb_web::ErrorKind::AlreadyOpen => ErrorKind::ResourceBusy,
        webusb_web::ErrorKind::Disconnected => ErrorKind::NotFound,
        webusb_web::ErrorKind::Security => ErrorKind::PermissionDenied,
        webusb_web::ErrorKind::Stall => ErrorKind::ConnectionAborted,
        webusb_web::ErrorKind::Babble => ErrorKind::ConnectionAborted,
        webusb_web::ErrorKind::Transfer => ErrorKind::ConnectionAborted,
        webusb_web::ErrorKind::InvalidAccess => ErrorKind::InvalidInput,
        webusb_web::ErrorKind::Other => ErrorKind::Other,
        _ => ErrorKind::Other,
    };

    Error::new(kind, error)
}

/// Finds the interface by interface class.
///
/// Returns the interface number.
pub fn find_interface(dev: &UsbDevice, class: Class) -> Result<u8> {
    let Some(cfg) = dev.configuration() else {
        return Err(Error::new(ErrorKind::NotFound, "USB device has no active configuration"));
    };

    for iface in cfg.interfaces {
        let alt = &iface.alternate;
        if alt.interface_class == class.class
            && alt.interface_subclass == class.sub_class
            && alt.interface_protocol == class.protocol
        {
            return Ok(iface.interface_number);
        }
    }

    Err(Error::new(ErrorKind::NotFound, "UPC interface not found"))
}

/// Read the device-provided information on the specified device and interface.
///
/// The maximum size of the data is [`INFO_SIZE`].
pub async fn info(hnd: &OpenUsbDevice, interface: u8) -> Result<Vec<u8>> {
    // Claim interface.
    hnd.claim_interface(interface).await.map_err(to_io_err)?;

    // Read info.
    tracing::debug!("reading info");
    let req = UsbControlRequest::new(
        UsbRequestType::Vendor,
        UsbRecipient::Interface,
        CTRL_REQ_INFO,
        0,
        interface.into(),
    );
    let info = hnd.control_transfer_in(&req, INFO_SIZE as _).await.map_err(to_io_err)?;

    Ok(info)
}

pub(crate) struct UpcShared {
    pub(crate) name: String,
    pub(crate) error: Arc<Mutex<Option<webusb_web::Error>>>,
    pub(crate) stop_tx: Option<oneshot::Sender<()>>,
}

impl Drop for UpcShared {
    fn drop(&mut self) {
        let _ = self.stop_tx.take().unwrap().send(());
    }
}

/// Connect to the specified device and interface.
///
/// Use [`find_interface`] to determine the `interface` number.
/// The `topic` is provided to the device and may contain user-defined data.
///
/// # Panics
/// Panics if the size of `topic` is larger than [`INFO_SIZE`].
pub async fn connect(hnd: Rc<OpenUsbDevice>, interface: u8, topic: &[u8]) -> Result<(UpcSender, UpcReceiver)> {
    assert!(topic.len() <= INFO_SIZE, "topic too big");

    let guard = Rc::new(InUseGuard::new(Rc::as_ptr(&hnd) as _, interface)?);
    let dev = hnd.device();

    // Get endpoints.
    let mut ep_in = None;
    let mut ep_out = None;
    let mut max_packet_size = usize::MAX;
    {
        let Some(cfg) = dev.configuration() else {
            return Err(Error::new(ErrorKind::NotFound, "USB device has no active configuration"));
        };
        let Some(iface_desc) = cfg.interfaces.iter().find(|i| i.interface_number == interface) else {
            return Err(Error::new(ErrorKind::NotFound, "specified interface not found"));
        };
        let alt = &iface_desc.alternate;
        for ep in &alt.endpoints {
            max_packet_size = max_packet_size.min(ep.packet_size as _);
            match ep.direction {
                UsbDirection::In => ep_in = Some(ep.endpoint_number),
                UsbDirection::Out => ep_out = Some(ep.endpoint_number),
            }
        }
    }
    let (Some(ep_in), Some(ep_out)) = (ep_in, ep_out) else {
        return Err(Error::new(ErrorKind::NotFound, "endpoints not found"));
    };
    let max_transfer_size = max_packet_size * 128;

    // Open device and claim interface.
    hnd.claim_interface(interface).await.map_err(to_io_err)?;
    hnd.clear_halt(UsbDirection::In, ep_in).await.map_err(to_io_err)?;
    hnd.clear_halt(UsbDirection::Out, ep_out).await.map_err(to_io_err)?;

    // Close previous connection.
    tracing::debug!("Closing previous connection");
    let control = UsbControlRequest::new(
        UsbRequestType::Vendor,
        UsbRecipient::Interface,
        CTRL_REQ_CLOSE,
        0,
        interface.into(),
    );
    hnd.control_transfer_out(&control, &[]).await.map_err(to_io_err)?;

    // // Flush buffer.
    // let mut buf = vec![0; max_transfer_size];
    // while let Ok(n) = hnd.read_bulk(ep_in, &mut buf, Duration::from_millis(10)) {
    //     if n == 0 {
    //         break;
    //     }
    // }

    // Open connection.
    tracing::debug!("opening connection");
    let control = UsbControlRequest::new(
        UsbRequestType::Vendor,
        UsbRecipient::Interface,
        CTRL_REQ_OPEN,
        0,
        interface.into(),
    );
    hnd.control_transfer_out(&control, topic).await.map_err(to_io_err)?;
    tracing::debug!("connection is open");

    // Start handler tasks.
    let error = Arc::new(Mutex::new(None));
    let name = format!("{}:{}", dev.serial_number().unwrap_or_default(), interface);
    let closed = Rc::new(Cell::new(false));

    let hnd_in = hnd.clone();
    let error_in = error.clone();
    let (tx_in, rx_in) = mpsc::channel(16);
    let in_closed = closed.clone();
    let in_guard = guard.clone();
    spawn_local(async move {
        let _in_guard = in_guard;
        in_task(hnd_in, tx_in, ep_in, interface, error_in, max_transfer_size, in_closed).await;
    });

    let (tx_out, rx_out) = mpsc::channel(16);
    let error_out = error.clone();
    let (stop_tx, stop_rx) = oneshot::channel();
    spawn_local(async move {
        let _out_guard = guard;
        out_task(hnd, rx_out, ep_out, interface, error_out, stop_rx, max_packet_size, closed).await;
    });

    // Build objects.
    let shared = Arc::new(UpcShared { name, error, stop_tx: Some(stop_tx) });
    let sender = UpcSender { tx: tx_out, shared: shared.clone() };
    let recv = UpcReceiver { rx: rx_in, shared, buffer: BytesMut::new(), max_size: MAX_SIZE, max_transfer_size };

    Ok((sender, recv))
}

async fn in_task(
    hnd: Rc<OpenUsbDevice>, tx: mpsc::Sender<BytesMut>, ep: u8, iface: u8,
    error: Arc<Mutex<Option<webusb_web::Error>>>, max_transfer_size: usize, closed: Rc<Cell<bool>>,
) {
    while !tx.is_closed() {
        match hnd.transfer_in(ep, max_transfer_size as _).await {
            Ok(buf) => {
                #[cfg(feature = "trace-packets")]
                tracing::trace!("Received packet of {} bytes", buf.len());
                if tx.blocking_send(Bytes::from(buf).into()).is_err() {
                    break;
                }
            }
            //Err(rusb::Error::Timeout) => (),
            Err(err) => {
                tracing::warn!("receiving failed: {err}");
                *error.lock().unwrap() = Some(err);
                break;
            }
        }
    }

    if !closed.replace(true) {
        close(&hnd, iface).await;
    }
}

#[allow(clippy::too_many_arguments)]
async fn out_task(
    hnd: Rc<OpenUsbDevice>, mut rx: mpsc::Receiver<Bytes>, ep: u8, iface: u8,
    error: Arc<Mutex<Option<webusb_web::Error>>>, mut stop_rx: oneshot::Receiver<()>, max_packet_size: usize,
    closed: Rc<Cell<bool>>,
) {
    'outer: while let Some(mut data) = rx.recv().await {
        loop {
            match stop_rx.try_recv() {
                Err(TryRecvError::Empty) => (),
                _ => break 'outer,
            }

            match hnd.transfer_out(ep, &data).await {
                Ok(n) if n as usize != data.len() => {
                    #[cfg(feature = "trace-packets")]
                    tracing::trace!("Sent packet of {n} bytes");
                    let _ = data.split_to(n as usize);
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
                // Err(rusb::Error::Timeout) => (),
                Err(err) => {
                    tracing::warn!("sending failed: {err}");
                    *error.lock().unwrap() = Some(err);
                    break 'outer;
                }
            }
        }
    }

    if !closed.replace(true) {
        close(&hnd, iface).await;
    }
}

async fn close(hnd: &OpenUsbDevice, iface: u8) {
    tracing::debug!("closing connection");
    let control =
        UsbControlRequest::new(UsbRequestType::Vendor, UsbRecipient::Interface, CTRL_REQ_CLOSE, 0, iface.into());
    if let Err(err) = hnd.control_transfer_out(&control, &[]).await {
        tracing::warn!("closing connection failed: {err}");
    }
}
