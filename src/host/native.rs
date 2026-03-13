//! Native host backend

use bytes::{Bytes, BytesMut};
use futures::FutureExt;
use nusb::{
    descriptors::TransferType,
    transfer::{
        Buffer, Bulk, Completion, ControlIn, ControlOut, ControlType, Direction, In, Out, Recipient,
        TransferError,
    },
    Device, DeviceInfo, Endpoint, Interface,
};
use std::{
    io::{Error, ErrorKind, Result},
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    sync::{mpsc, watch},
    time::{sleep, timeout},
};

use super::{DeviceStatus, UpcOptions, UpcReceiver, UpcSender};
use crate::{ctrl_req, status, Capabilities, Class, INFO_SIZE, MAX_SIZE};

const TIMEOUT: Duration = Duration::from_secs(1);
const CLAIM_TIMEOUT: Duration = Duration::from_secs(5);
const CLAIM_RETRY_INTERVAL: Duration = Duration::from_millis(50);
const BUFFER_COUNT: usize = 16;

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
    /// If `notify_device` is true (host-initiated close), sends ctrl_req::CLOSE_SEND.
    /// If false (device-initiated close via notification), no control request is sent.
    async fn close_send(&mut self, notify_device: bool) {
        if !self.send_closed {
            self.send_closed = true;
            if notify_device {
                tracing::debug!("host send closed, sending CTRL_REQ_CLOSE_SEND");
                if let Err(err) = self
                    .iface
                    .control_out(control_out(ctrl_req::CLOSE_SEND, 0, self.interface.into(), &[]), TIMEOUT)
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
    connect_with(dev, interface, UpcOptions { topic: topic.into(), ..Default::default() }).await
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
    let mut max_packet_size = usize::MAX;
    {
        let cfg = dev.active_configuration().map_err(|err| Error::new(ErrorKind::NetworkDown, err))?;
        let iface_desc = cfg
            .interfaces()
            .find(|i| i.interface_number() == interface)
            .ok_or_else(|| Error::new(ErrorKind::NotFound, format!("interface {interface} not found")))?;
        for ep in iface_desc.first_alt_setting().endpoints() {
            match (ep.direction(), ep.transfer_type()) {
                (Direction::In, TransferType::Bulk) => {
                    max_packet_size = max_packet_size.min(ep.max_packet_size());
                    ep_in = Some(ep.address());
                }
                (Direction::Out, TransferType::Bulk) => {
                    max_packet_size = max_packet_size.min(ep.max_packet_size());
                    ep_out = Some(ep.address());
                }
                _ => {}
            }
        }
    }
    let (Some(ep_in), Some(ep_out)) = (ep_in, ep_out) else {
        return Err(Error::new(ErrorKind::NotFound, "transfer endpoints not found"));
    };
    let max_transfer_size = max_packet_size * 128;

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
        ep_in.submit(Buffer::new(max_packet_size));
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
            control_in(ctrl_req::CAPABILITIES, 0, interface.into(), Capabilities::SIZE.try_into().unwrap()),
            TIMEOUT,
        )
        .await
    {
        Ok(data) => Capabilities::decode(&data)?,
        Err(err) => {
            tracing::debug!("capabilities query failed: {err}");
            Capabilities::default()
        }
    };
    tracing::debug!("device capabilities: {caps:?}");

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
    let (tx_in, rx_in) = mpsc::channel(BUFFER_COUNT);
    let half_close_in = half_close.clone();
    let status_rx_in = status_rx.clone();
    tokio::spawn(in_task(ep_in, tx_in, error_in, max_transfer_size, half_close_in, status_rx_in));

    let (tx_out, rx_out) = mpsc::channel(BUFFER_COUNT);
    let error_out = error.clone();
    let half_close_out = half_close.clone();
    tokio::spawn(out_task(ep_out, rx_out, error_out, max_packet_size, half_close_out, status_rx));

    // Build objects.
    let shared = Arc::new(UpcShared { name, error });
    let sender = UpcSender { tx: tx_out, shared: shared.clone() };
    let recv = UpcReceiver { rx: rx_in, shared, buffer: BytesMut::new(), max_size: MAX_SIZE, max_transfer_size };

    Ok((sender, recv))
}

async fn in_task(
    mut ep: Endpoint<Bulk, In>, tx: mpsc::Sender<Bytes>, error: Arc<Mutex<Option<TaskError>>>,
    max_transfer_size: usize, half_close: Arc<tokio::sync::Mutex<HalfCloseHandle>>,
    mut status_rx: watch::Receiver<DeviceStatus>,
) {
    let host_initiated = {
        // Pre-submit buffers.
        for _ in 0..BUFFER_COUNT {
            ep.submit(Buffer::new(max_transfer_size));
        }

        loop {
            ep.submit(Buffer::new(max_transfer_size));
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
                    // Device halted the IN endpoint — clean half-close signal
                    // meaning the device is done sending.
                    tracing::debug!("device halted IN endpoint (done sending)");
                } else {
                    tracing::warn!("receiving failed: {err}");
                    error.lock().unwrap().get_or_insert(err.into());
                }
                break false;
            }

            let mut buf = comp.buffer.into_vec();
            buf.truncate(comp.actual_len);

            #[cfg(feature = "trace-packets")]
            tracing::trace!("Received packet of {} bytes", buf.len());

            if tx.send(buf.into()).await.is_err() {
                break true;
            }
        }
    };

    half_close.lock().await.close_recv(host_initiated).await;
}

#[allow(clippy::too_many_arguments)]
async fn out_task(
    mut ep: Endpoint<Bulk, Out>, mut rx: mpsc::Receiver<Bytes>, error: Arc<Mutex<Option<TaskError>>>,
    max_packet_size: usize, half_close: Arc<tokio::sync::Mutex<HalfCloseHandle>>,
    mut status_rx: watch::Receiver<DeviceStatus>,
) {
    let host_initiated = 'task: {
        loop {
            let data = tokio::select! {
                biased;
                _ = status_rx.wait_for(|s| s.recv_closed || s.dead) => {
                    tracing::debug!("device status change in out_task, stopping");
                    break 'task false;
                }
                data = rx.recv() => data,
            };
            let Some(data) = data else {
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
                        break 'task false;
                    }
                }
                break 'task true;
            };

            #[cfg(feature = "trace-packets")]
            tracing::trace!("Queueing packet of {} bytes for sending", data.len());

            ep.submit(Buffer::from(data.to_vec()));

            if !data.is_empty() && data.len() % max_packet_size == 0 {
                ep.submit(Buffer::new(0));
            }

            let mut get_complete = async || {
                let pending = ep.pending();
                if pending == 0 {
                    None
                } else if pending >= BUFFER_COUNT {
                    Some(ep.next_complete().await)
                } else {
                    ep.next_complete().now_or_never()
                }
            };

            while let Some(comp) = get_complete().await {
                if let Err(err) = comp.status {
                    if err == TransferError::Stall {
                        // Device halted the OUT endpoint — clean half-close signal
                        // meaning the device is done receiving.
                        tracing::debug!("device halted OUT endpoint (done receiving)");
                    } else {
                        tracing::warn!("sending failed: {err}");
                        error.lock().unwrap().get_or_insert(err.into());
                    }
                    break 'task false;
                }

                #[cfg(feature = "trace-packets")]
                tracing::trace!("Sent packet of {} bytes", comp.actual_len);

                if comp.actual_len != comp.buffer.len() {
                    tracing::warn!("only {} out of {} bytes were sent", comp.actual_len, comp.buffer.len());
                    *error.lock().unwrap() =
                        Some(TaskError::PartialSend { size: comp.buffer.len(), sent: comp.actual_len });
                }
            }
        }
    };

    half_close.lock().await.close_send(host_initiated).await;
}

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

    /// Verify that connect function is Send.
    #[tokio::test]
    async fn connect_is_send() {
        let hnd = {
            let Ok(mut devs) = nusb::list_devices().await else { return };
            let Some(dev) = devs.next() else { return };
            let Ok(hnd) = dev.open().await else { return };
            hnd
        };
        tokio::spawn(async move {
            let _ = connect(hnd, 0, &[]).await;
        });
    }
}
