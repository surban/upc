//! Web host backend

use bytes::{Bytes, BytesMut};
use std::{
    io::{Error, ErrorKind, Result},
    rc::Rc,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::sync::{mpsc, oneshot, oneshot::error::TryRecvError, watch};
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::{spawn_local, JsFuture};

use webusb_web::{OpenUsbDevice, UsbControlRequest, UsbDevice, UsbDirection, UsbRecipient, UsbRequestType};

use super::{guard::InUseGuard, DeviceStatus, UpcOptions, UpcReceiver, UpcSender};
use crate::{ctrl_req, status, Class, DeviceCapabilities, HostCapabilities, INFO_SIZE};

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

/// Sleep for the specified duration.
async fn sleep(duration: Duration) {
    let ms = duration.as_millis() as i32;
    let promise = js_sys::Promise::new(&mut |resolve, _reject| {
        let global = js_sys::global();
        if let Some(window) = global.dyn_ref::<web_sys::Window>() {
            window.set_timeout_with_callback_and_timeout_and_arguments_0(&resolve, ms).unwrap();
        } else if let Some(worker) = global.dyn_ref::<web_sys::WorkerGlobalScope>() {
            worker.set_timeout_with_callback_and_timeout_and_arguments_0(&resolve, ms).unwrap();
        } else {
            panic!("unsupported global scope");
        }
    });
    JsFuture::from(promise).await.unwrap();
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
        ctrl_req::INFO,
        0,
        interface.into(),
    );
    let info = hnd.control_transfer_in(&req, INFO_SIZE as _).await.map_err(to_io_err)?;

    Ok(info)
}

/// Tracks half-close state and sends control requests when each direction closes.
struct HalfCloseHandle {
    hnd: Rc<OpenUsbDevice>,
    iface: u8,
    send_closed: bool,
    recv_closed: bool,
}

impl HalfCloseHandle {
    fn new(hnd: Rc<OpenUsbDevice>, iface: u8) -> Self {
        Self { hnd, iface, send_closed: false, recv_closed: false }
    }

    /// Mark send direction as closed.
    ///
    /// If `notify_device` is true (host-initiated close), sends ctrl_req::CLOSE_SEND
    /// with the total number of bytes sent so the device can drain the endpoint.
    /// If false (device-initiated close via notification), no control request is sent.
    async fn close_send(&mut self, notify_device: bool, total_bytes_sent: u64) {
        if !self.send_closed {
            self.send_closed = true;
            if notify_device {
                tracing::debug!(
                    "host send closed, sending CTRL_REQ_CLOSE_SEND (total_bytes_sent={total_bytes_sent})"
                );
                let payload = total_bytes_sent.to_le_bytes();
                let control = UsbControlRequest::new(
                    UsbRequestType::Vendor,
                    UsbRecipient::Interface,
                    ctrl_req::CLOSE_SEND,
                    0,
                    self.iface.into(),
                );
                if let Err(err) = self.hnd.control_transfer_out(&control, &payload).await {
                    tracing::warn!("sending CTRL_REQ_CLOSE_SEND failed: {err}");
                }
            } else {
                tracing::debug!("send direction closed (device-initiated)");
            }
            self.close_if_both().await;
        }
    }

    /// Mark receive direction as closed.
    ///
    /// If `notify_device` is true (host-initiated close), sends ctrl_req::CLOSE_RECV.
    /// If false (device-initiated close via notification), no control request is sent.
    async fn close_recv(&mut self, notify_device: bool) {
        if !self.recv_closed {
            self.recv_closed = true;
            if notify_device {
                tracing::debug!("host recv closed, sending CTRL_REQ_CLOSE_RECV");
                let control = UsbControlRequest::new(
                    UsbRequestType::Vendor,
                    UsbRecipient::Interface,
                    ctrl_req::CLOSE_RECV,
                    0,
                    self.iface.into(),
                );
                if let Err(err) = self.hnd.control_transfer_out(&control, &[]).await {
                    tracing::warn!("sending CTRL_REQ_CLOSE_RECV failed: {err}");
                }
            } else {
                tracing::debug!("recv direction closed (device-initiated)");
            }
            self.close_if_both().await;
        }
    }

    async fn close_if_both(&mut self) {
        if self.send_closed && self.recv_closed {
            close(&self.hnd, self.iface).await;
        }
    }
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
    connect_with(hnd, interface, UpcOptions::new().with_topic(topic.into())).await
}

/// Connect to the specified device and interface with options.
///
/// Use [`find_interface`] to determine the `interface` number.
///
/// # Panics
/// Panics if the size of `topic` is larger than [`INFO_SIZE`].
pub async fn connect_with(
    hnd: Rc<OpenUsbDevice>, interface: u8, options: UpcOptions,
) -> Result<(UpcSender, UpcReceiver)> {
    assert!(options.topic.len() <= INFO_SIZE, "topic too big");

    // Retry if the interface is still in use from a previous connection whose
    // background tasks have not yet finished releasing it.
    let guard = {
        let key = Rc::as_ptr(&hnd) as usize;
        let mut remaining = 100u32;
        Rc::new(loop {
            match InUseGuard::new(key, interface) {
                Ok(guard) => break guard,
                Err(err) if err.kind() == ErrorKind::ResourceBusy && remaining > 0 => {
                    remaining -= 1;
                    tracing::debug!("interface {interface} is in use, retrying…");
                    sleep(Duration::from_millis(50)).await;
                }
                Err(err) => return Err(err),
            }
        })
    };
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
            use webusb_web::UsbEndpointType;
            match (ep.direction, ep.endpoint_type) {
                (UsbDirection::In, UsbEndpointType::Bulk) => {
                    max_packet_size = max_packet_size.min(ep.packet_size as _);
                    ep_in = Some(ep.endpoint_number);
                }
                (UsbDirection::Out, UsbEndpointType::Bulk) => {
                    max_packet_size = max_packet_size.min(ep.packet_size as _);
                    ep_out = Some(ep.endpoint_number);
                }
                _ => {}
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
        ctrl_req::CLOSE,
        0,
        interface.into(),
    );
    hnd.control_transfer_out(&control, &[]).await.map_err(to_io_err)?;

    // Query capabilities.
    tracing::debug!("querying capabilities");
    let req = UsbControlRequest::new(
        UsbRequestType::Vendor,
        UsbRecipient::Interface,
        ctrl_req::CAPABILITIES,
        0,
        interface.into(),
    );
    let caps = match hnd.control_transfer_in(&req, DeviceCapabilities::SIZE as _).await {
        Ok(data) => DeviceCapabilities::decode(&data)?,
        Err(err) => {
            tracing::debug!("capabilities query failed: {err}");
            DeviceCapabilities::default()
        }
    };
    tracing::debug!("device capabilities: {caps:?}");

    // Send host capabilities.
    tracing::debug!("sending host capabilities");
    let host_caps = HostCapabilities { max_packet_size: options.max_size as u64 };
    let host_caps_req = UsbControlRequest::new(
        UsbRequestType::Vendor,
        UsbRecipient::Interface,
        ctrl_req::CAPABILITIES,
        0,
        interface.into(),
    );
    let _ = hnd.control_transfer_out(&host_caps_req, &host_caps.encode()).await;

    // Determine status polling interval.
    let status_interval = if caps.status_supported {
        match (options.ping_interval, caps.ping_timeout) {
            (Some(ping_interval), Some(ping_timeout)) => Some(ping_interval.min(ping_timeout / 2)),
            (None, Some(ping_timeout)) => Some(ping_timeout / 2),
            (Some(ping_interval), None) => Some(ping_interval),
            (None, None) => None,
        }
    } else {
        None
    };

    // Start handler tasks.
    let error = Arc::new(Mutex::new(None));
    let name = format!("{}:{}", dev.serial_number().unwrap_or_default(), interface);
    let half_close = Arc::new(tokio::sync::Mutex::new(HalfCloseHandle::new(hnd.clone(), interface)));

    let (status_tx, status_rx) = watch::channel(DeviceStatus::default());
    match status_interval {
        Some(status_interval) => {
            let hnd_status = hnd.clone();
            let status_guard = guard.clone();
            let error_status = error.clone();
            spawn_local(async move {
                let _status_guard = status_guard;
                status_task(hnd_status, interface, status_interval, status_tx, error_status).await;
            });
        }
        None => {
            let status_guard = guard.clone();
            spawn_local(async move {
                let _status_guard = status_guard;
                status_tx.closed().await;
            });
        }
    }

    let hnd_in = hnd.clone();
    let error_in = error.clone();
    let (tx_in, mut rx_in) = mpsc::channel(16);
    let in_guard = guard.clone();
    let half_close_in = half_close.clone();
    let status_rx_in = status_rx.clone();
    spawn_local(async move {
        let _in_guard = in_guard;
        in_task(hnd_in, tx_in, ep_in, error_in, max_transfer_size, half_close_in, status_rx_in).await;
    });

    let hnd_out = hnd.clone();
    let (tx_out, rx_out) = mpsc::channel(16);
    let error_out = error.clone();
    let (stop_tx, stop_rx) = oneshot::channel();
    let half_close_out = half_close.clone();
    let out_guard = guard.clone();
    spawn_local(async move {
        let _out_guard = out_guard;
        out_task(hnd_out, rx_out, ep_out, error_out, stop_rx, max_packet_size, half_close_out, status_rx).await;
    });

    // Flush receive buffer.
    loop {
        tokio::select! {
            biased;
            Some(_) = rx_in.recv() => (),
            () = sleep(Duration::from_millis(10)) => break,
        }
    }

    // Open connection.
    tracing::debug!("opening connection");
    let control = UsbControlRequest::new(
        UsbRequestType::Vendor,
        UsbRecipient::Interface,
        ctrl_req::OPEN,
        0,
        interface.into(),
    );
    hnd.control_transfer_out(&control, &options.topic).await.map_err(to_io_err)?;
    tracing::debug!("connection is open");

    // Build objects.
    let device_max_size = usize::try_from(caps.max_packet_size).unwrap_or(usize::MAX);
    let shared = Arc::new(UpcShared { name, error, stop_tx: Some(stop_tx) });
    let sender = UpcSender { tx: tx_out, shared: shared.clone(), max_size: device_max_size };
    let recv =
        UpcReceiver { rx: rx_in, shared, buffer: BytesMut::new(), max_size: options.max_size, max_transfer_size };

    Ok((sender, recv))
}

async fn in_task(
    hnd: Rc<OpenUsbDevice>, tx: mpsc::Sender<Bytes>, ep: u8, error: Arc<Mutex<Option<webusb_web::Error>>>,
    max_transfer_size: usize, half_close: Arc<tokio::sync::Mutex<HalfCloseHandle>>,
    mut status_rx: watch::Receiver<DeviceStatus>,
) {
    let host_initiated = loop {
        let res = tokio::select! {
            biased;
            () = tx.closed() => break true,
            _ = status_rx.wait_for(|s| s.dead) => {
                tracing::debug!("device dead (ping timeout), stopping in_task");
                break false;
            }
            res = hnd.transfer_in(ep, max_transfer_size as _) => res,
        };

        match res {
            Ok(buf) => {
                #[cfg(feature = "trace-packets")]
                tracing::trace!("Received packet of {} bytes", buf.len());
                if tx.send(Bytes::from(buf).into()).await.is_err() {
                    break true;
                }
            }
            Err(err) if err.kind() == webusb_web::ErrorKind::Stall => {
                // Device halted the IN endpoint — clean half-close signal.
                tracing::debug!("device halted IN endpoint (done sending)");
                break false;
            }
            Err(err) => {
                tracing::warn!("receiving failed: {err}");
                error.lock().unwrap().get_or_insert(err);
                break false;
            }
        }
    };

    half_close.lock().await.close_recv(host_initiated).await;
}

#[allow(clippy::too_many_arguments)]
async fn out_task(
    hnd: Rc<OpenUsbDevice>, mut rx: mpsc::Receiver<Bytes>, ep: u8, error: Arc<Mutex<Option<webusb_web::Error>>>,
    mut stop_rx: oneshot::Receiver<()>, max_packet_size: usize,
    half_close: Arc<tokio::sync::Mutex<HalfCloseHandle>>, mut status_rx: watch::Receiver<DeviceStatus>,
) {
    let mut total_bytes_sent: u64 = 0;
    let host_initiated = 'outer: {
        loop {
            let data = tokio::select! {
                biased;
                data = rx.recv() => data,
                _ = status_rx.wait_for(|s| s.recv_closed || s.dead) => {
                    tracing::debug!("device status change in out_task, stopping");
                    break 'outer false;
                }
            };
            let Some(mut data) = data else { break 'outer true };

            total_bytes_sent = total_bytes_sent.wrapping_add(data.len() as u64);

            loop {
                match stop_rx.try_recv() {
                    Err(TryRecvError::Empty) => (),
                    _ => break 'outer false,
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
                    Err(err) if err.kind() == webusb_web::ErrorKind::Stall => {
                        // Device halted the OUT endpoint — clean half-close signal.
                        tracing::debug!("device halted OUT endpoint (done receiving)");
                        break 'outer false;
                    }
                    Err(err) => {
                        tracing::warn!("sending failed: {err}");
                        error.lock().unwrap().get_or_insert(err);
                        break 'outer false;
                    }
                }
            }
        }
    };

    half_close.lock().await.close_send(host_initiated, total_bytes_sent).await;
}

async fn close(hnd: &OpenUsbDevice, iface: u8) {
    tracing::debug!("closing connection");
    let control =
        UsbControlRequest::new(UsbRequestType::Vendor, UsbRecipient::Interface, ctrl_req::CLOSE, 0, iface.into());
    if let Err(err) = hnd.control_transfer_out(&control, &[]).await {
        tracing::warn!("closing connection failed: {err}");
    }
}

async fn status_task(
    hnd: Rc<OpenUsbDevice>, interface: u8, status_interval: Duration, status_tx: watch::Sender<DeviceStatus>,
    error: Arc<Mutex<Option<webusb_web::Error>>>,
) {
    loop {
        tokio::select! {
            () = sleep(status_interval) => (),
            () = status_tx.closed() => return,
        }

        let req = UsbControlRequest::new(
            UsbRequestType::Vendor,
            UsbRecipient::Interface,
            ctrl_req::STATUS,
            0,
            interface.into(),
        );

        match hnd.control_transfer_in(&req, status::MAX_SIZE as _).await {
            Ok(data) => {
                for &byte in &data {
                    match byte {
                        status::RECV_CLOSED => {
                            if !status_tx.borrow().recv_closed {
                                tracing::debug!("device status: recv_closed");
                                status_tx.send_modify(|status| status.recv_closed = true);
                            }
                        }
                        other => tracing::warn!("unknown status byte: {other:#x}"),
                    }
                }
            }
            Err(err) => {
                tracing::warn!("status request failed: {err}");
                error.lock().unwrap().get_or_insert(err);
                status_tx.send_modify(|status| status.dead = true);
                return;
            }
        }
    }
}
