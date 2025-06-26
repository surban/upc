//! Native host backend

use bytes::{Bytes, BytesMut};
use futures::FutureExt;
use nusb::{
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
use tokio::{sync::mpsc, task::JoinSet, time::timeout};

use super::{UpcReceiver, UpcSender};
use crate::{Class, CTRL_REQ_CLOSE, CTRL_REQ_INFO, CTRL_REQ_OPEN, INFO_SIZE, MAX_SIZE};

const TIMEOUT: Duration = Duration::from_secs(1);
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

const fn control_out(request: u8, value: u16, index: u16, data: &[u8]) -> ControlOut {
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
        .control_in(control_in(CTRL_REQ_INFO, 0, interface.into(), INFO_SIZE.try_into().unwrap()), TIMEOUT)
        .await
        .map_err(transfer_to_io_err)?;

    Ok(info)
}

pub(crate) struct UpcShared {
    pub(crate) name: String,
    pub(crate) error: Arc<Mutex<Option<TaskError>>>,
    pub(crate) _tasks: JoinSet<()>,
    pub(crate) _close_tx: mpsc::Sender<()>,
}

impl Drop for UpcShared {
    fn drop(&mut self) {
        // empty: tasks will be terminated by JoinSet
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
    assert!(topic.len() <= INFO_SIZE, "topic too big");

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
            max_packet_size = max_packet_size.min(ep.max_packet_size());
            match ep.direction() {
                Direction::In => ep_in = Some(ep.address()),
                Direction::Out => ep_out = Some(ep.address()),
            }
        }
    }
    let (Some(ep_in), Some(ep_out)) = (ep_in, ep_out) else {
        return Err(Error::new(ErrorKind::NotFound, "transfer endpoints not found"));
    };
    let max_transfer_size = max_packet_size * 128;

    // Open device and claim interface.
    let iface = dev.claim_interface(interface).await.map_err(nusb_to_io_err)?;
    let mut ep_in = iface.endpoint(ep_in).map_err(nusb_to_io_err)?;
    let mut ep_out = iface.endpoint(ep_out).map_err(nusb_to_io_err)?;
    ep_in.clear_halt().await.map_err(nusb_to_io_err)?;
    ep_out.clear_halt().await.map_err(nusb_to_io_err)?;

    // Close previous connection.
    tracing::debug!("closing previous connection");
    iface
        .control_out(control_out(CTRL_REQ_CLOSE, 0, interface.into(), &[]), TIMEOUT)
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

    // Open connection.
    tracing::debug!("opening connection");
    iface
        .control_out(control_out(CTRL_REQ_OPEN, 0, interface.into(), topic), TIMEOUT)
        .await
        .map_err(transfer_to_io_err)?;
    tracing::debug!("connection is open");

    // Start handler taks.
    let error = Arc::new(Mutex::new(None));
    let name = format!("{dev:?}:{interface}");
    let (close_tx, close_rx) = mpsc::channel(1);

    let mut tasks = JoinSet::new();
    let error_in = error.clone();
    let (tx_in, rx_in) = mpsc::channel(BUFFER_COUNT);
    tasks.spawn(in_task(ep_in, tx_in, error_in, max_transfer_size, close_tx.clone()));

    let (tx_out, rx_out) = mpsc::channel(BUFFER_COUNT);
    let error_out = error.clone();
    tasks.spawn(out_task(ep_out, rx_out, error_out, max_packet_size, close_tx.clone()));

    tokio::spawn(close_task(iface, interface, close_rx));

    // Build objects.
    let shared = Arc::new(UpcShared { name, error, _tasks: tasks, _close_tx: close_tx });
    let sender = UpcSender { tx: tx_out, shared: shared.clone() };
    let recv = UpcReceiver { rx: rx_in, shared, buffer: BytesMut::new(), max_size: MAX_SIZE, max_transfer_size };

    Ok((sender, recv))
}

async fn in_task(
    mut ep: Endpoint<Bulk, In>, tx: mpsc::Sender<Bytes>, error: Arc<Mutex<Option<TaskError>>>,
    max_transfer_size: usize, _close_tx: mpsc::Sender<()>,
) {
    // Pre-submit buffers.
    for _ in 0..BUFFER_COUNT {
        ep.submit(Buffer::new(max_transfer_size));
    }

    loop {
        ep.submit(Buffer::new(max_transfer_size));
        let comp = tokio::select! {
            comp = ep.next_complete() => comp,
            () = tx.closed() => break,
        };

        if let Err(err) = comp.status {
            tracing::warn!("receiving failed: {err}");
            *error.lock().unwrap() = Some(err.into());
            break;
        }

        let mut buf = comp.buffer.into_vec();
        buf.truncate(comp.actual_len);

        #[cfg(feature = "trace-packets")]
        tracing::trace!("Received packet of {} bytes", buf.len());

        if tx.send(buf.into()).await.is_err() {
            break;
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn out_task(
    mut ep: Endpoint<Bulk, Out>, mut rx: mpsc::Receiver<Bytes>, error: Arc<Mutex<Option<TaskError>>>,
    max_packet_size: usize, close_tx: mpsc::Sender<()>,
) {
    while let Some(data) = rx.recv().await {
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
                tracing::warn!("sending failed: {err}");
                *error.lock().unwrap() = Some(err.into());
                break;
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

    let _ = close_tx.send(()).await;
}

async fn close_task(iface: Interface, interface: u8, mut close_rx: mpsc::Receiver<()>) {
    let _ = close_rx.recv().await;

    tracing::debug!("closing connection");
    if let Err(err) = iface.control_out(control_out(CTRL_REQ_CLOSE, 0, interface.into(), &[]), TIMEOUT).await {
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
