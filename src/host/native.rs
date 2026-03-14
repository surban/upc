//! Native host backend

use bytes::{Bytes, BytesMut};
use futures::FutureExt;
use std::{
    io::{Error, ErrorKind, Result},
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    sync::{mpsc, watch},
    time::{sleep, timeout},
};

use nusb::{
    descriptors::TransferType,
    transfer::{
        Buffer, Bulk, Completion, ControlIn, ControlOut, ControlType, Direction, In, Out, Recipient,
        TransferError,
    },
    Device, DeviceInfo, Endpoint, Interface,
};

use super::{DeviceStatus, UpcOptions, UpcReceiver, UpcSender};
use crate::{
    ctrl_req, status, trace::Tracer, Class, DeviceCapabilities, HostCapabilities, INFO_SIZE, QUEUE_LEN,
    TRANSFER_PACKETS,
};

const TIMEOUT: Duration = Duration::from_secs(1);
const CLAIM_TIMEOUT: Duration = Duration::from_secs(5);
const CLAIM_RETRY_INTERVAL: Duration = Duration::from_millis(50);
const DMA_BUFFERS: usize = 8;

/// A received DMA buffer that is returned to the in_task for reuse on drop.
pub(crate) struct RecvPacket {
    buffer: Option<Buffer>,
    actual_len: usize,
    return_tx: mpsc::UnboundedSender<Buffer>,
}

impl RecvPacket {
    pub(crate) fn data(&self) -> &[u8] {
        &self.buffer.as_ref().unwrap()[..self.actual_len]
    }

    pub(crate) fn is_zero_copy(&self) -> bool {
        self.buffer.as_ref().unwrap().is_zero_copy()
    }

    pub(crate) fn into_vec(mut self) -> Vec<u8> {
        let mut v = self.buffer.take().unwrap().into_vec();
        v.truncate(self.actual_len);
        v
    }
}

impl Drop for RecvPacket {
    fn drop(&mut self) {
        if let Some(buf) = self.buffer.take() {
            let _ = self.return_tx.send(buf);
        }
    }
}

const fn control_in(request: u8, value: u16, index: u16, length: u16) -> ControlIn {
    ControlIn {
        control_type: ControlType::Vendor,
        recipient: Recipient::Interface,
        request,
        value,
        index,
        length,
    }
}

const fn control_out(request: u8, value: u16, index: u16, data: &[u8]) -> ControlOut<'_> {
    ControlOut { control_type: ControlType::Vendor, recipient: Recipient::Interface, request, value, index, data }
}

pub(crate) fn nusb_to_io_err(error: nusb::Error) -> Error {
    let kind = match error.kind() {
        nusb::ErrorKind::Disconnected => ErrorKind::ConnectionAborted,
        nusb::ErrorKind::PermissionDenied => ErrorKind::PermissionDenied,
        nusb::ErrorKind::NotFound => ErrorKind::NotFound,
        nusb::ErrorKind::Busy => ErrorKind::ResourceBusy,
        nusb::ErrorKind::Unsupported => ErrorKind::Unsupported,
        _ => ErrorKind::Other,
    };

    Error::new(kind, error)
}

pub(crate) fn transfer_to_io_err(error: TransferError) -> Error {
    let kind = match error {
        TransferError::Disconnected => ErrorKind::ConnectionAborted,
        TransferError::Cancelled => ErrorKind::TimedOut,
        TransferError::Fault => ErrorKind::UnexpectedEof,
        TransferError::InvalidArgument => ErrorKind::InvalidInput,
        TransferError::Stall => ErrorKind::WriteZero,
        _ => ErrorKind::Other,
    };

    Error::new(kind, error)
}

#[derive(Clone, Debug)]
pub(crate) enum TaskError {
    Transfer(TransferError),
    PartialSend { size: usize, sent: usize },
}

pub(crate) fn to_io_err(err: TaskError) -> Error {
    match err {
        TaskError::Transfer(err) => transfer_to_io_err(err),
        TaskError::PartialSend { size, sent } => {
            Error::new(ErrorKind::WriteZero, format!("only sent {sent} out of {size} bytes"))
        }
    }
}

impl From<TransferError> for TaskError {
    fn from(err: TransferError) -> Self {
        Self::Transfer(err)
    }
}

/// Finds the interface by interface class from USB device information.
///
/// Returns the interface number.
pub fn find_interface(dev_info: &DeviceInfo, class: Class) -> Result<u8> {
    for iface in dev_info.interfaces() {
        if iface.class() == class.class
            && iface.subclass() == class.sub_class
            && iface.protocol() == class.protocol
        {
            return Ok(iface.interface_number());
        }
    }

    Err(Error::new(ErrorKind::NotFound, format!("USB interface for {class:?} not found")))
}

/// Finds the interface by interface class for an opened USB device.
///
/// Returns the interface number.
pub fn find_interface_opened(dev: &Device, class: Class) -> Result<u8> {
    let cfg = dev.active_configuration()?;
    for iface in cfg.interfaces() {
        let iface = iface.first_alt_setting();
        if iface.class() == class.class
            && iface.subclass() == class.sub_class
            && iface.protocol() == class.protocol
        {
            return Ok(iface.interface_number());
        }
    }

    Err(Error::new(ErrorKind::NotFound, format!("USB interface for {class:?} not found")))
}

/// Read the device-provided information on the specified device and interface.
///
/// The maximum size of the data is [`INFO_SIZE`].
pub async fn info(dev: &Device, interface: u8) -> Result<Vec<u8>> {
    // Claim interface.
    let iface = dev.claim_interface(interface).await.map_err(nusb_to_io_err)?;

    // Read info.
    tracing::debug!("reading info");
    let info = iface
        .control_in(control_in(ctrl_req::INFO, 0, interface.into(), INFO_SIZE.try_into().unwrap()), TIMEOUT)
        .await
        .map_err(transfer_to_io_err)?;

    Ok(info)
}

/// Probe whether the specified interface speaks the UPC protocol.
///
/// Returns `true` if the device responds with the expected probe response,
/// `false` if the request is stalled or the response does not match.
pub async fn probe(dev: &Device, interface: u8) -> Result<bool> {
    let iface = dev.claim_interface(interface).await.map_err(nusb_to_io_err)?;

    tracing::debug!("probing interface {interface}");
    let resp = iface
        .control_in(
            control_in(ctrl_req::PROBE, 0, interface.into(), ctrl_req::PROBE_RESPONSE.len().try_into().unwrap()),
            TIMEOUT,
        )
        .await;

    match resp {
        Ok(data) => Ok(data.as_slice() == ctrl_req::PROBE_RESPONSE),
        Err(TransferError::Stall) => Ok(false),
        Err(err) => Err(transfer_to_io_err(err)),
    }
}

pub(crate) struct UpcShared {
    pub(crate) name: String,
    pub(crate) error: Arc<Mutex<Option<TaskError>>>,
}

/// Tracks half-close state and sends control requests when each direction closes.
pub(crate) struct HalfCloseHandle {
    iface: Interface,
    interface: u8,
    send_closed: bool,
    recv_closed: bool,
}

impl HalfCloseHandle {
    fn new(iface: Interface, interface: u8) -> Self {
        Self { iface, interface, send_closed: false, recv_closed: false }
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
                if let Err(err) = self
                    .iface
                    .control_out(control_out(ctrl_req::CLOSE_SEND, 0, self.interface.into(), &payload), TIMEOUT)
                    .await
                {
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
                if let Err(err) = self
                    .iface
                    .control_out(control_out(ctrl_req::CLOSE_RECV, 0, self.interface.into(), &[]), TIMEOUT)
                    .await
                {
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
            tracing::debug!("closing connection, sending CTRL_REQ_CLOSE");
            if let Err(err) =
                self.iface.control_out(control_out(ctrl_req::CLOSE, 0, self.interface.into(), &[]), TIMEOUT).await
            {
                tracing::warn!("sending CTRL_REQ_CLOSE failed: {err}");
            }
        }
    }
}

/// Connect to the specified device and interface.
///
/// Use [`find_interface`] to determine the `interface` number.
/// The `topic` is provided to the device and may contain user-defined data.
///
/// # Panics
/// Panics if the size of `topic` is larger than [`INFO_SIZE`].
pub async fn connect(dev: Device, interface: u8, topic: &[u8]) -> Result<(UpcSender, UpcReceiver)> {
    connect_with(dev, interface, UpcOptions::new().with_topic(topic.into())).await
}

/// Connect to the specified device and interface with options.
///
/// Use [`find_interface`] to determine the `interface` number.
///
/// # Panics
/// Panics if the size of `topic` is larger than [`INFO_SIZE`].
pub async fn connect_with(dev: Device, interface: u8, options: UpcOptions) -> Result<(UpcSender, UpcReceiver)> {
    assert!(options.topic.len() <= INFO_SIZE, "topic too big");

    // Get endpoints.
    let mut ep_in = None;
    let mut ep_out = None;
    let mut mps = usize::MAX;
    {
        let cfg = dev.active_configuration().map_err(|err| Error::new(ErrorKind::NetworkDown, err))?;
        let iface_desc = cfg
            .interfaces()
            .find(|i| i.interface_number() == interface)
            .ok_or_else(|| Error::new(ErrorKind::NotFound, format!("interface {interface} not found")))?;
        for ep in iface_desc.first_alt_setting().endpoints() {
            match (ep.direction(), ep.transfer_type()) {
                (Direction::In, TransferType::Bulk) => {
                    mps = mps.min(ep.max_packet_size());
                    ep_in = Some(ep.address());
                }
                (Direction::Out, TransferType::Bulk) => {
                    mps = mps.min(ep.max_packet_size());
                    ep_out = Some(ep.address());
                }
                _ => {}
            }
        }
    }
    let (Some(ep_in), Some(ep_out)) = (ep_in, ep_out) else {
        return Err(Error::new(ErrorKind::NotFound, "transfer endpoints not found"));
    };
    let max_transfer_size = mps * TRANSFER_PACKETS;

    // Open device and claim interface.
    // Retry if the interface is still busy from a previous connection whose
    // background tasks have not yet finished releasing it.
    let iface = {
        let deadline = tokio::time::Instant::now() + CLAIM_TIMEOUT;
        loop {
            match dev.claim_interface(interface).await {
                Ok(iface) => break iface,
                Err(err) if err.kind() == nusb::ErrorKind::Busy => {
                    if tokio::time::Instant::now() >= deadline {
                        return Err(nusb_to_io_err(err));
                    }
                    tracing::debug!("interface {interface} is busy, retrying…");
                    sleep(CLAIM_RETRY_INTERVAL).await;
                }
                Err(err) => return Err(nusb_to_io_err(err)),
            }
        }
    };
    let mut ep_in = iface.endpoint::<Bulk, In>(ep_in).map_err(nusb_to_io_err)?;
    let mut ep_out = iface.endpoint::<Bulk, Out>(ep_out).map_err(nusb_to_io_err)?;
    ep_in.clear_halt().await.map_err(nusb_to_io_err)?;
    ep_out.clear_halt().await.map_err(nusb_to_io_err)?;

    // Close previous connection.
    tracing::debug!("closing previous connection");
    iface
        .control_out(control_out(ctrl_req::CLOSE, 0, interface.into(), &[]), TIMEOUT)
        .await
        .map_err(transfer_to_io_err)?;

    // Flush buffers.
    tracing::debug!("flushing buffers");
    loop {
        ep_in.submit(Buffer::new(mps));
        match timeout(Duration::from_millis(10), ep_in.next_complete()).await {
            Ok(Completion { status: Ok(()), .. }) => (),
            Ok(Completion { status: Err(_), .. }) => break,
            Err(_) => {
                ep_in.cancel_all();
                let _ = ep_in.next_complete().await;
                break;
            }
        }
    }

    // Query capabilities.
    tracing::debug!("querying capabilities");
    let caps = match iface
        .control_in(
            control_in(ctrl_req::CAPABILITIES, 0, interface.into(), DeviceCapabilities::SIZE.try_into().unwrap()),
            TIMEOUT,
        )
        .await
    {
        Ok(data) => DeviceCapabilities::decode(&data)?,
        Err(err) => {
            tracing::debug!("capabilities query failed: {err}");
            DeviceCapabilities::default()
        }
    };
    tracing::debug!("device capabilities: {caps:?}");

    // Send host capabilities.
    tracing::debug!("sending host capabilities");
    let host_caps = HostCapabilities { max_size: options.max_size as u64 };
    let _ = iface
        .control_out(control_out(ctrl_req::CAPABILITIES, 0, interface.into(), &host_caps.encode()), TIMEOUT)
        .await;

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

    // Open connection.
    tracing::debug!("opening connection");
    iface
        .control_out(control_out(ctrl_req::OPEN, 0, interface.into(), &options.topic), TIMEOUT)
        .await
        .map_err(transfer_to_io_err)?;
    tracing::debug!("connection is open");

    // Start handler tasks.
    let error = Arc::new(Mutex::new(None));
    let name = format!("{dev:?}:{interface}");
    let half_close = Arc::new(tokio::sync::Mutex::new(HalfCloseHandle::new(iface.clone(), interface)));

    let (status_tx, status_rx) = watch::channel(DeviceStatus::default());
    match status_interval {
        Some(status_interval) => {
            let error_status = error.clone();
            tokio::spawn(status_task(iface, interface, status_interval, status_tx, error_status));
        }
        None => {
            tokio::spawn(async move { status_tx.closed().await });
        }
    }

    let error_in = error.clone();
    let (tx_in, rx_in) = mpsc::channel(QUEUE_LEN);
    let half_close_in = half_close.clone();
    let status_rx_in = status_rx.clone();
    let in_tracer = Tracer::host_in(&name);
    tokio::spawn(in_task(ep_in, tx_in, error_in, max_transfer_size, half_close_in, status_rx_in, in_tracer));

    let (tx_out, rx_out) = mpsc::channel(QUEUE_LEN);
    let error_out = error.clone();
    let half_close_out = half_close.clone();
    let out_tracer = Tracer::host_out(&name);
    tokio::spawn(out_task(ep_out, rx_out, error_out, mps, half_close_out, status_rx, out_tracer));

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
    mut ep: Endpoint<Bulk, In>, tx: mpsc::Sender<RecvPacket>, error: Arc<Mutex<Option<TaskError>>>,
    max_transfer_size: usize, half_close: Arc<tokio::sync::Mutex<HalfCloseHandle>>,
    mut status_rx: watch::Receiver<DeviceStatus>, mut tracer: Tracer,
) {
    let (return_tx, mut return_rx) = mpsc::unbounded_channel::<Buffer>();

    let host_initiated = {
        // Pre-submit DMA buffers.
        for _ in 0..DMA_BUFFERS {
            ep.submit(ep.allocate(max_transfer_size));
        }

        loop {
            tracer.next();

            // Reuse a recycled buffer if available, otherwise allocate a new one.
            let buf = match return_rx.try_recv() {
                Ok(buf) => buf,
                Err(_) => ep.allocate(max_transfer_size),
            };
            ep.submit(buf);

            let comp = tokio::select! {
                biased;
                comp = ep.next_complete() => comp,
                () = tx.closed() => break true,
                _ = status_rx.wait_for(|s| s.dead) => {
                    tracing::debug!("device dead (ping timeout), stopping in_task");
                    break false;
                }
            };

            if let Err(err) = comp.status {
                if err == TransferError::Stall {
                    tracing::debug!("device halted IN endpoint (done sending)");
                } else {
                    tracing::warn!("receiving failed: {err}");
                    error.lock().unwrap().get_or_insert(err.into());
                }
                break false;
            }

            tracer.received_packet(comp.actual_len);

            let packet = RecvPacket {
                buffer: Some(comp.buffer),
                actual_len: comp.actual_len,
                return_tx: return_tx.clone(),
            };

            if tx.send(packet).await.is_err() {
                break true;
            }
        }
    };

    half_close.lock().await.close_recv(host_initiated).await;
}

/// Task for performing USB OUT transfers.
#[allow(clippy::too_many_arguments)]
async fn out_task(
    mut ep: Endpoint<Bulk, Out>, mut rx: mpsc::Receiver<Bytes>, error: Arc<Mutex<Option<TaskError>>>, mps: usize,
    half_close: Arc<tokio::sync::Mutex<HalfCloseHandle>>, mut status_rx: watch::Receiver<DeviceStatus>,
    mut tracer: Tracer,
) {
    let mut total_bytes_sent: u64 = 0;

    let mut host_initiated = 'task: loop {
        tracer.next();

        // Get data for sending from queue.
        let data = tokio::select! {
            biased;

            _ = status_rx.wait_for(|s| s.recv_closed || s.dead) => {
                tracing::debug!("device closed in out_task, stopping");
                break 'task false;
            }

            data_opt = rx.recv() => {
                match data_opt {
                    Some(data) => data,
                    None => break 'task true,
                }
            }
        };

        let data_len = data.len();
        total_bytes_sent = total_bytes_sent.wrapping_add(data_len as u64);

        // Send data via ons USB transfer.
        tracer.sending_data(data_len);
        tracer.send_part(data_len);
        ep.submit(Buffer::from(Vec::from(data)));

        // Terminate with zero length packet if length is multiple of MPS.
        if data_len > 0 && data_len % mps == 0 {
            tracer.send_terminator();
            ep.submit(Buffer::new(0));
        }
        tracer.data_finished();

        // Finish completed USB transfers.
        let mut get_complete = async || {
            let pending = ep.pending();
            if pending == 0 {
                None
            } else if pending >= DMA_BUFFERS {
                Some(ep.next_complete().await)
            } else {
                ep.next_complete().now_or_never()
            }
        };

        while let Some(comp) = get_complete().await {
            if let Err(err) = comp.status {
                if err == TransferError::Stall {
                    tracing::debug!("device halted OUT endpoint (done receiving)");
                } else {
                    tracing::warn!("sending failed: {err}");
                    error.lock().unwrap().get_or_insert(err.into());
                }
                break 'task false;
            }

            if comp.actual_len != comp.buffer.len() {
                tracing::warn!("only {} out of {} bytes were sent", comp.actual_len, comp.buffer.len());
                *error.lock().unwrap() =
                    Some(TaskError::PartialSend { size: comp.buffer.len(), sent: comp.actual_len });
            }
        }
    };

    // Drain all pending completions so data reaches the device
    // before we signal close.
    while ep.pending() > 0 {
        let comp = ep.next_complete().await;
        if let Err(err) = comp.status {
            if err == TransferError::Stall {
                tracing::debug!("device halted OUT endpoint during flush");
            } else {
                tracing::warn!("sending failed during flush: {err}");
                error.lock().unwrap().get_or_insert(err.into());
            }
            host_initiated = false;
        }
    }

    half_close.lock().await.close_send(host_initiated, total_bytes_sent).await;
}

/// Task for getting device status via USB CONTROL IN transfers.
async fn status_task(
    iface: Interface, interface: u8, status_interval: Duration, status_tx: watch::Sender<DeviceStatus>,
    error: Arc<Mutex<Option<TaskError>>>,
) {
    loop {
        tokio::select! {
            () = sleep(status_interval) => (),
            () = status_tx.closed() => return,
        }

        let res = iface
            .control_in(
                control_in(ctrl_req::STATUS, 0, interface.into(), status::MAX_SIZE.try_into().unwrap()),
                TIMEOUT,
            )
            .await;

        match res {
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
                error.lock().unwrap().get_or_insert(err.into());
                status_tx.send_modify(|status| status.dead = true);
                return;
            }
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;

    fn assert_send<T: Send>(_: &T) {}

    #[test]
    fn connect_is_send() {
        fn check(dev: Device) {
            let fut = connect(dev, 0, &[]);
            assert_send(&fut);
        }
        _ = check;
    }
}
