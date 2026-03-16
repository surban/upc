//! Web host backend

use bytes::{Bytes, BytesMut};
use std::{
    io::{Error, ErrorKind, Result},
    rc::Rc,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::sync::{mpsc, watch};
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::{spawn_local, JsFuture};

use webusb_web::{OpenUsbDevice, UsbControlRequest, UsbDevice, UsbDirection, UsbRecipient, UsbRequestType};

use super::{guard::InUseGuard, DeviceStatus, UpcOptions, UpcReceiver, UpcSender};
use crate::{
    ctrl_req, status, trace::Tracer, Class, DeviceCapabilities, HostCapabilities, INFO_SIZE, QUEUE_LEN,
    TRANSFER_PACKETS,
};

/// A received packet from the web host backend.
pub(crate) struct RecvPacket(Bytes);

impl RecvPacket {
    pub(crate) fn data(&self) -> &[u8] {
        &self.0
    }

    pub(crate) fn is_zero_copy(&self) -> bool {
        false
    }

    pub(crate) fn into_vec(self) -> Vec<u8> {
        self.0.into()
    }
}

pub(crate) fn to_io_err(error: TaskError) -> Error {
    match error {
        TaskError::WebUsb(err) => web_to_io_err(err),
        TaskError::PartialSend { size, sent } => {
            Error::new(ErrorKind::WriteZero, format!("only sent {sent} out of {size} bytes"))
        }
    }
}

fn web_to_io_err(error: webusb_web::Error) -> Error {
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

#[derive(Clone, Debug)]
pub(crate) enum TaskError {
    WebUsb(webusb_web::Error),
    PartialSend { size: usize, sent: usize },
}

impl From<webusb_web::Error> for TaskError {
    fn from(err: webusb_web::Error) -> Self {
        Self::WebUsb(err)
    }
}

/// Sleep for the specified duration.
async fn sleep(duration: Duration) {
    let ms = duration.as_millis().try_into().unwrap_or(i32::MAX);
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
    hnd.claim_interface(interface).await.map_err(web_to_io_err)?;

    // Read info.
    tracing::debug!("reading info");
    let req = UsbControlRequest::new(
        UsbRequestType::Vendor,
        UsbRecipient::Interface,
        ctrl_req::INFO,
        0,
        interface.into(),
    );
    let info = hnd.control_transfer_in(&req, INFO_SIZE as _).await.map_err(web_to_io_err)?;

    Ok(info.to_vec())
}

/// Probe whether the specified interface speaks the UPC protocol.
///
/// Returns `true` if the device responds with the expected probe response,
/// `false` if the request is stalled or the response does not match.
pub async fn probe(hnd: &OpenUsbDevice, interface: u8) -> Result<bool> {
    hnd.claim_interface(interface).await.map_err(web_to_io_err)?;

    tracing::debug!("probing interface {interface}");
    let req = UsbControlRequest::new(
        UsbRequestType::Vendor,
        UsbRecipient::Interface,
        ctrl_req::PROBE,
        0,
        interface.into(),
    );

    match hnd.control_transfer_in(&req, ctrl_req::PROBE_RESPONSE.len() as _).await {
        Ok(data) => Ok(data.to_vec() == ctrl_req::PROBE_RESPONSE),
        Err(err) if err.kind() == webusb_web::ErrorKind::Stall => Ok(false),
        Err(err) => Err(web_to_io_err(err)),
    }
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

    /// Close connection if sender and receiver are closed.
    async fn close_if_both(&mut self) {
        if self.send_closed && self.recv_closed {
            tracing::debug!("closing connection");
            let control = UsbControlRequest::new(
                UsbRequestType::Vendor,
                UsbRecipient::Interface,
                ctrl_req::CLOSE,
                0,
                self.iface.into(),
            );
            if let Err(err) = self.hnd.control_transfer_out(&control, &[]).await {
                tracing::warn!("closing connection failed: {err}");
            }

            // Release the interface.
            if let Err(err) = self.hnd.release_interface(self.iface).await {
                tracing::warn!("releasing interface failed: {err}");
            }
        }
    }
}

/// Data shared between sender and receiver.
pub(crate) struct UpcShared {
    pub(crate) name: String,
    pub(crate) error: Arc<Mutex<Option<TaskError>>>,
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
    let mut mps = usize::MAX;
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
                    mps = mps.min(ep.packet_size as _);
                    ep_in = Some(ep.endpoint_number);
                }
                (UsbDirection::Out, UsbEndpointType::Bulk) => {
                    mps = mps.min(ep.packet_size as _);
                    ep_out = Some(ep.endpoint_number);
                }
                _ => {}
            }
        }
    }
    let (Some(ep_in), Some(ep_out)) = (ep_in, ep_out) else {
        return Err(Error::new(ErrorKind::NotFound, "endpoints not found"));
    };
    let max_transfer_size = mps * TRANSFER_PACKETS;

    // Release and re-claim interface to cancel any orphaned IN transfers
    // from a previous connection that was not properly closed.
    let _ = hnd.release_interface(interface).await;
    hnd.claim_interface(interface).await.map_err(web_to_io_err)?;
    hnd.clear_halt(UsbDirection::In, ep_in).await.map_err(web_to_io_err)?;
    hnd.clear_halt(UsbDirection::Out, ep_out).await.map_err(web_to_io_err)?;

    // Close previous connection.
    tracing::debug!("Closing previous connection");
    let control = UsbControlRequest::new(
        UsbRequestType::Vendor,
        UsbRecipient::Interface,
        ctrl_req::CLOSE,
        0,
        interface.into(),
    );
    hnd.control_transfer_out(&control, &[]).await.map_err(web_to_io_err)?;

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
        Ok(data) => DeviceCapabilities::decode(&data.to_vec())?,
        Err(err) => {
            tracing::debug!("capabilities query failed: {err}");
            DeviceCapabilities::default()
        }
    };
    tracing::debug!("device capabilities: {caps:?}");

    // Send host capabilities.
    tracing::debug!("sending host capabilities");
    let host_caps = HostCapabilities { max_size: options.max_size as u64 };
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
    let (tx_in, mut rx_in) = mpsc::channel(QUEUE_LEN);
    let in_guard = guard.clone();
    let half_close_in = half_close.clone();
    let status_rx_in = status_rx.clone();
    let in_tracer = Tracer::host_in(&name);
    spawn_local(async move {
        let _in_guard = in_guard;
        in_task(hnd_in, tx_in, ep_in, error_in, max_transfer_size, half_close_in, status_rx_in, in_tracer).await;
    });

    let hnd_out = hnd.clone();
    let (tx_out, rx_out) = mpsc::channel(QUEUE_LEN);
    let error_out = error.clone();
    let half_close_out = half_close.clone();
    let out_guard = guard.clone();
    let out_tracer = Tracer::host_out(&name);
    spawn_local(async move {
        let _out_guard = out_guard;
        out_task(hnd_out, rx_out, ep_out, error_out, mps, half_close_out, status_rx, out_tracer).await;
    });

    // Flush receive buffer.
    loop {
        tokio::select! {
            biased;
            Some(_) = rx_in.recv() => (),
            () = sleep(crate::FLUSH_TIMEOUT) => break,
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
    hnd.control_transfer_out(&control, &options.topic).await.map_err(web_to_io_err)?;
    tracing::debug!("connection is open");

    // Build objects.
    let device_max_size = usize::try_from(caps.max_size).unwrap_or(usize::MAX);
    let send_tracer = Tracer::host_enqueue(&name);
    let recv_tracer = Tracer::host_dequeue(&name);
    let shared = Arc::new(UpcShared { name, error });
    let sender = UpcSender { tx: tx_out, shared: shared.clone(), max_size: device_max_size, tracer: send_tracer };
    let recv = UpcReceiver {
        rx: rx_in,
        shared,
        buffer: BytesMut::new(),
        max_size: options.max_size,
        max_transfer_size,
        tracer: recv_tracer,
    };

    Ok((sender, recv))
}

/// Task for performing USB IN transfers.
async fn in_task(
    hnd: Rc<OpenUsbDevice>, tx: mpsc::Sender<RecvPacket>, ep: u8, error: Arc<Mutex<Option<TaskError>>>,
    max_transfer_size: usize, half_close: Arc<tokio::sync::Mutex<HalfCloseHandle>>,
    mut status_rx: watch::Receiver<DeviceStatus>, mut tracer: Tracer,
) {
    let host_initiated = loop {
        tracer.next();

        // Receive USB transfer.
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
                tracer.received_packet(buf.len());
                if tx.send(RecvPacket(Bytes::from(buf.to_vec()))).await.is_err() {
                    break true;
                }
            }
            Err(err) if err.kind() == webusb_web::ErrorKind::Stall => {
                tracing::debug!("device halted IN endpoint (done sending)");
                break false;
            }
            Err(err) => {
                tracing::warn!("receiving failed: {err}");
                error.lock().unwrap().get_or_insert(err.into());
                break false;
            }
        }
    };

    half_close.lock().await.close_recv(host_initiated).await;
}

/// Task for performing USB OUT transfers.
#[allow(clippy::too_many_arguments)]
async fn out_task(
    hnd: Rc<OpenUsbDevice>, mut rx: mpsc::Receiver<Bytes>, ep: u8, error: Arc<Mutex<Option<TaskError>>>,
    mps: usize, half_close: Arc<tokio::sync::Mutex<HalfCloseHandle>>,
    mut status_rx: watch::Receiver<DeviceStatus>, mut tracer: Tracer,
) {
    let mut total_bytes_sent: u64 = 0;

    let host_initiated = 'task: loop {
        tracer.next();

        // Get data to send from queue.
        let data = tokio::select! {
            biased;

            _ = status_rx.wait_for(|s| s.recv_closed || s.dead) => {
                tracing::debug!("device status change in out_task, stopping");
                break 'task false;
            }

            data_opt = rx.recv() => {
                match data_opt {
                    Some(data) => data,
                    None => break 'task true,
                }
            }
        };

        total_bytes_sent = total_bytes_sent.wrapping_add(data.len() as u64);

        // Send data via one USB transfer.
        tracer.sending_data(data.len());
        tracer.send_part(data.len());
        match hnd.transfer_out(ep, &data).await {
            Ok(sent) if sent as usize == data.len() => (),
            Ok(sent) => {
                tracing::warn!("only {sent} out of {} bytes were sent", data.len());
                *error.lock().unwrap() = Some(TaskError::PartialSend { size: data.len(), sent: sent as _ });
                break 'task false;
            }
            Err(err) if err.kind() == webusb_web::ErrorKind::Stall => {
                tracing::debug!("device halted OUT endpoint (done receiving)");
                break 'task false;
            }
            Err(err) => {
                tracing::warn!("sending failed: {err}");
                error.lock().unwrap().get_or_insert(err.into());
                break 'task false;
            }
        }

        // Send zero length packet if length is a multiple of MPS.
        if !data.is_empty() && data.len() % mps == 0 {
            tracer.send_terminator();
            match hnd.transfer_out(ep, &[]).await {
                Ok(_) => {}
                Err(err) if err.kind() == webusb_web::ErrorKind::Stall => {
                    tracing::debug!("device halted OUT endpoint during ZLP (done receiving)");
                    break 'task false;
                }
                Err(err) => {
                    tracing::warn!("sending ZLP failed: {err}");
                    error.lock().unwrap().get_or_insert(err.into());
                    break 'task false;
                }
            }
        }

        tracer.data_finished();
    };

    half_close.lock().await.close_send(host_initiated, total_bytes_sent).await;
}

/// Task for getting device status via USB CONTROL IN transfers.
async fn status_task(
    hnd: Rc<OpenUsbDevice>, interface: u8, status_interval: Duration, status_tx: watch::Sender<DeviceStatus>,
    error: Arc<Mutex<Option<TaskError>>>,
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
                let data = data.to_vec();
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
                error.lock().unwrap().get_or_insert(err.into());
                status_tx.send_modify(|status| status.dead = true);
                return;
            }
        }
    }
}
