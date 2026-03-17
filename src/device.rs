//! Device-side USB packet channel (UPC)
//!
//! Use [`UpcFunction::new`] to create an USB function that accepts incoming connections.
//! Pass the returned handle to the [`usb_gadget`] library to expose the USB gadget.
//!

use bytes::{Bytes, BytesMut};
use futures::{future, sink, stream, FutureExt, Sink, SinkExt, Stream, StreamExt};
use std::{
    fmt,
    future::Future,
    io::{Error, ErrorKind, Result},
    mem::{replace, take},
    path::Path,
    pin::{pin, Pin},
    sync::Arc,
    task::{Context, Poll},
    time::Duration,
};
use tokio::{
    sync::{mpsc, watch, Mutex},
    task::JoinSet,
    time::{sleep, timeout, Instant},
};
use uuid::Uuid;

use usb_gadget::function::{
    custom::{
        Custom, CustomBuilder, Endpoint, EndpointDirection, EndpointReceiver, EndpointSender, Event, Interface,
        OsExtCompat, OsExtProp,
    },
    Handle,
};

use crate::{
    channel_error, ctrl_req, status, trace::Tracer, Class, DeviceCapabilities, HostCapabilities, INFO_SIZE,
    MAX_SIZE, QUEUE_LEN, TRANSFER_PACKETS,
};

#[derive(Debug, Clone)]
struct Cfg {
    info: Vec<u8>,
    ping_timeout: Option<Duration>,
    max_size: u64,
    recv_pool_size: usize,
}

impl Default for Cfg {
    fn default() -> Self {
        Self {
            info: Vec::new(),
            ping_timeout: Some(Duration::from_secs(10)),
            max_size: MAX_SIZE as u64,
            recv_pool_size: 5 * 1024 * 1024,
        }
    }
}

/// Sends data into a USB packet channel.
pub struct UpcSender {
    tx: mpsc::Sender<Bytes>,
    error: watch::Receiver<Option<ErrorKind>>,
    max_size: usize,
    tracer: Tracer,
}

impl fmt::Debug for UpcSender {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("UpcSender").field("max_size", &self.max_size).finish()
    }
}

impl UpcSender {
    /// Send packet.
    ///
    /// If the host has closed its receive direction (half-close),
    /// this returns an error with [`ErrorKind::BrokenPipe`].
    ///
    /// ## Cancel safety
    /// If canceled, no data will have been sent.
    pub async fn send(&self, data: Bytes) -> Result<()> {
        if data.len() > self.max_size {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                format!("packet size {} exceeds maximum of {}", data.len(), self.max_size),
            ));
        }

        self.tracer.enqueued(data.len());

        match self.tx.send(data).await {
            Ok(()) => Ok(()),
            Err(_) => Err(channel_error((*self.error.borrow()).unwrap_or(ErrorKind::BrokenPipe))),
        }
    }

    /// The maximum packet size accepted by the host.
    pub fn max_size(&self) -> usize {
        self.max_size
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
    topic: Vec<u8>,
    rx: mpsc::Receiver<BytesMut>,
    buffer: BytesMut,
    max_size: usize,
    max_transfer_size: usize,
    error: watch::Receiver<Option<ErrorKind>>,
    tracer: Tracer,
}

impl fmt::Debug for UpcReceiver {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("UpcReceiver")
            .field("topic", &String::from_utf8_lossy(&self.topic))
            .field("max_size", &self.max_size)
            .finish()
    }
}

impl UpcReceiver {
    /// Topic provided by host when establishing the connection.
    pub fn topic(&self) -> &[u8] {
        &self.topic
    }

    /// Receive packet.
    ///
    /// ## Cancel safety
    /// If canceled, no data will have been removed from the receive queue.
    pub async fn recv(&mut self) -> Result<Option<BytesMut>> {
        self.tracer.next();
        self.tracer.dequeing();

        loop {
            let Some(packet) = self.rx.recv().await else {
                // Channel closed — check if there was an error or a clean half-close.
                return match *self.error.borrow() {
                    Some(kind) => Err(channel_error(kind)),
                    None => Ok(None),
                };
            };

            let packet_len = packet.len();
            self.tracer.dequeued_part(packet_len);

            self.buffer.unsplit(packet);

            if self.buffer.len() > self.max_size {
                self.buffer.clear();
                return Err(Error::new(
                    ErrorKind::InvalidData,
                    "incoming packet exceeds maximum receive packet size",
                ));
            }

            if packet_len < self.max_transfer_size {
                self.tracer.dequeued(self.buffer.len());
                return Ok(Some(take(&mut self.buffer)));
            }
        }
    }

    /// The maximum receive packet size.
    pub fn max_size(&self) -> usize {
        self.max_size
    }

    /// Turns this into a stream of packets.
    pub fn into_stream(self) -> UpcStream {
        let stream = stream::try_unfold(self, |mut this| async move {
            match this.recv().await {
                Ok(Some(data)) => Ok(Some((data, this))),
                Ok(None) => Ok(None),
                Err(err) => Err(err),
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

/// Pre-allocated buffer pool for zero-copy reassembly.
///
/// Most chunks carved from the same pool are contiguous in memory,
/// so [`BytesMut::unsplit`] can merge them without copying.
struct BytesPool {
    buf: BytesMut,
    pool_size: usize,
}

impl BytesPool {
    /// Create a new pool of specified size.
    pub fn new(pool_size: usize) -> Self {
        Self { buf: BytesMut::with_capacity(pool_size), pool_size }
    }

    /// Take one chunk from the pool, reallocating if exhausted.
    pub fn chunk(&mut self, chunk_size: usize) -> BytesMut {
        if self.buf.capacity() < chunk_size {
            self.buf = BytesMut::with_capacity(self.pool_size.max(chunk_size));
        }
        let remaining = self.buf.split_off(chunk_size);
        replace(&mut self.buf, remaining)
    }
}

struct Head {
    tx: mpsc::Sender<BytesMut>,
    rx: mpsc::Receiver<Bytes>,
    error: watch::Sender<Option<ErrorKind>>,
}

fn connection(
    topic: Vec<u8>, max_transfer_size: usize, max_send_size: usize, max_recv_size: usize,
) -> (UpcSender, UpcReceiver, Head) {
    let (tx_in, rx_in) = mpsc::channel(QUEUE_LEN);
    let (tx_out, rx_out) = mpsc::channel(QUEUE_LEN);
    let (error_tx, error_rx) = watch::channel(None);
    let sender = UpcSender {
        tx: tx_out,
        error: error_rx.clone(),
        max_size: max_send_size,
        tracer: Tracer::device_enqueue(),
    };
    let recv = UpcReceiver {
        topic,
        rx: rx_in,
        buffer: BytesMut::new(),
        max_size: max_recv_size,
        max_transfer_size,
        error: error_rx,
        tracer: Tracer::device_dequeue(),
    };
    let head = Head { tx: tx_in, rx: rx_out, error: error_tx };
    (sender, recv, head)
}

/// USB interface id.
#[derive(Debug, Clone)]
pub struct InterfaceId {
    /// Interface class.
    pub class: Class,
    /// Interface name.
    pub name: String,
    /// Device interface GUID for Microsoft OS.
    pub guid: Option<Uuid>,
}

impl InterfaceId {
    /// Creates a new interface id using the specified interface class.
    pub fn new(class: Class) -> Self {
        Self { class, name: "USB packet channel (UPC)".to_string(), guid: None }
    }

    /// Sets the interface name.
    #[must_use]
    pub fn with_name(mut self, name: impl AsRef<str>) -> Self {
        self.name = name.as_ref().to_string();
        self
    }

    /// Sets the device interface GUID for Microsoft OS.
    #[must_use]
    pub fn with_guid(mut self, guid: Uuid) -> Self {
        self.guid = Some(guid);
        self
    }
}

impl From<Class> for InterfaceId {
    fn from(class: Class) -> Self {
        Self::new(class)
    }
}

/// USB packet device-side function.
pub struct UpcFunction {
    class: Class,
    name: String,
    task: JoinSet<Result<()>>,
    conn_rx: mpsc::Receiver<(UpcSender, UpcReceiver)>,
    cfg: Arc<Mutex<Cfg>>,
}

impl fmt::Debug for UpcFunction {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("UpcFunction").field("class", &self.class).field("name", &self.name).finish()
    }
}

impl UpcFunction {
    /// Creates a new USB packet device-side interface with the specified interface identification.
    ///
    /// Keep the returned `UpcFunction` and pass the [`Handle`] to
    /// [`usb_gadget::Config::with_function`] to include the interface in your
    /// USB gadget.
    ///
    /// Use [`InterfaceId::with_guid`] to set a Device Interface GUID so that
    /// Windows automatically loads the WinUSB driver for this interface.
    /// Without a GUID, the device will not be accessible on Windows.
    pub fn new(interface_id: InterfaceId) -> (Self, Handle) {
        Self::init(interface_id, |builder| Ok(builder.build())).unwrap()
    }

    /// Creates a new USB packet device-side interface with the specified interface identification
    /// using the provided FunctionFS directory.
    pub fn with_ffs(interface_id: InterfaceId, ffs_dir: impl AsRef<Path>) -> Result<Self> {
        let (this, ()) = Self::init(interface_id, |builder| Ok((builder.existing(ffs_dir)?, ())))?;
        Ok(this)
    }

    fn init<T>(
        interface_id: InterfaceId, build_fn: impl FnOnce(CustomBuilder) -> Result<(Custom, T)>,
    ) -> Result<(Self, T)> {
        let (ep_rx, ep_rx_dir) = EndpointDirection::host_to_device();
        let (ep_tx, ep_tx_dir) = EndpointDirection::device_to_host();

        let mut interface = Interface::new(interface_id.class.into(), &interface_id.name)
            .with_endpoint(Endpoint::bulk(ep_rx_dir))
            .with_endpoint(Endpoint::bulk(ep_tx_dir))
            .with_os_ext_compat(OsExtCompat::winusb());
        if let Some(guid) = interface_id.guid {
            interface = interface.with_os_ext_prop(OsExtProp::device_interface_guid(guid));
        }

        let builder = Custom::builder().with_interface(interface);
        let (ep0, ret) = build_fn(builder)?;

        let (conn_tx, conn_rx) = mpsc::channel(4);
        let cfg = Arc::new(Mutex::new(Cfg::default()));

        let mut task = JoinSet::new();
        task.spawn(Self::task(ep0, ep_tx, ep_rx, conn_tx, cfg.clone()));

        Ok((Self { class: interface_id.class, name: interface_id.name.to_string(), task, conn_rx, cfg }, ret))
    }

    /// Returns the info data readable by host.
    pub async fn info(&self) -> Vec<u8> {
        self.cfg.lock().await.info.clone()
    }

    /// Sets the info data readable by host.
    ///
    /// # Panics
    /// Panics if info is larger than [`INFO_SIZE`].
    pub async fn set_info(&self, info: Vec<u8>) {
        assert!(info.len() <= INFO_SIZE, "info too big");
        self.cfg.lock().await.info = info;
    }

    /// Returns the ping timeout duration.
    pub async fn ping_timeout(&self) -> Option<Duration> {
        self.cfg.lock().await.ping_timeout
    }

    /// Sets the ping timeout duration.
    ///
    /// If the device does not hear from the host within
    /// this duration, it considers the host process dead and fails the
    /// connection.
    ///
    /// Set to `None` to disable ping timeout detection.
    ///
    /// The default is 10 seconds.
    pub async fn set_ping_timeout(&self, timeout: Option<Duration>) {
        self.cfg.lock().await.ping_timeout = timeout;
    }

    /// Returns the maximum receive packet size.
    pub async fn max_size(&self) -> u64 {
        self.cfg.lock().await.max_size
    }

    /// Sets the maximum receive packet size accepted.
    ///
    /// The default is [`MAX_SIZE`].
    pub async fn set_max_size(&self, max_size: u64) {
        self.cfg.lock().await.max_size = max_size;
    }

    /// Returns the receive pool size in bytes.
    pub async fn recv_pool_size(&self) -> usize {
        self.cfg.lock().await.recv_pool_size
    }

    /// Sets the receive pool size in bytes.
    ///
    /// The receive pool pre-allocates a contiguous buffer so that
    /// consecutive USB transfers can be merged without copying
    /// during packet reassembly. A larger pool reduces the number
    /// of copies at the cost of higher memory usage.
    ///
    /// The default is 5 MB.
    pub async fn set_recv_pool_size(&self, size: usize) {
        self.cfg.lock().await.recv_pool_size = size;
    }

    /// Waits for and accepts an incoming connection.
    ///
    /// ## Cancel safety
    /// If canceled, no connection will have been accepted.
    pub async fn accept(&mut self) -> Result<(UpcSender, UpcReceiver)> {
        match self.conn_rx.recv().await {
            Some(conn) => Ok(conn),
            None => match self.task.join_next().await {
                Some(Ok(Ok(()))) => Err(Error::new(ErrorKind::ConnectionReset, "the USB device was unbound")),
                Some(Ok(Err(err))) => Err(err),
                Some(Err(err)) => Err(err.into()),
                None => Err(Error::new(ErrorKind::BrokenPipe, "the USB device has failed")),
            },
        }
    }

    /// Prepare USB IN transfers
    async fn prepare_in(ep_rx: &Mutex<EndpointReceiver>, max_transfer_size: usize, pool: &mut BytesPool) {
        let mut ep_rx = ep_rx.lock().await;

        // Drain receive queue and pre-post receive buffers.
        while let Ok(Ok(_)) = timeout(crate::FLUSH_TIMEOUT, ep_rx.recv_async(pool.chunk(max_transfer_size))).await
        {
        }
    }

    /// Task that performs USB IN transfers.
    ///
    /// Returns `true` if the host initiated the close (CLOSE_SEND with byte count reached),
    /// `false` if the device-side receiver was dropped.
    async fn in_task(
        ep_rx: &Mutex<EndpointReceiver>, tx: mpsc::Sender<BytesMut>, mut close_rx: watch::Receiver<Option<u64>>,
        max_transfer_size: usize, mut pool: BytesPool,
    ) -> Result<bool> {
        let mut tracer = Tracer::device_in();

        let mut ep_rx = ep_rx.lock().await;
        let mut total_received: u64 = 0;
        let tx2 = tx.clone();

        loop {
            // Check if we have already received all bytes the host sent.
            if Some(total_received) == *close_rx.borrow() {
                tracing::debug!("received all {total_received} bytes from host");
                return Ok(true);
            }

            // Reserve space in receive queue.
            let permit = tokio::select! {
                biased;

                res = tx.reserve() => {
                    match res {
                        Ok(permit) => permit,
                        Err(_) => break,
                    }
                },

                Ok(()) = close_rx.changed() => continue,
            };

            // Select between receiving USB data, detecting that the
            // UpcReceiver has been dropped (so we can send the CLOSE_RECV
            // notification promptly even when the host is idle), and
            // the host signaling CLOSE_SEND with a byte count.
            tokio::select! {
                biased;

                res = ep_rx.recv_async(pool.chunk(max_transfer_size)) => {
                    if let Some(data) = res? {
                        total_received = total_received.wrapping_add(data.len() as u64);

                        tracer.next();
                        tracer.received_data(data.len());

                        permit.send(data);
                    }
                }

                () = tx2.closed() => break,

                Ok(()) = close_rx.changed() => continue,
            }
        }

        Ok(false)
    }

    /// Task that performs USB OUT transfers.
    async fn out_task(
        ep_tx: &Mutex<EndpointSender>, mut rx: mpsc::Receiver<Bytes>, max_transfer_size: usize,
    ) -> Result<()> {
        let mut tracer = Tracer::device_out();

        let mut ep_tx = ep_tx.lock().await;
        let mps = ep_tx.max_packet_size()?;

        while let Some(mut data) = rx.recv().await {
            tracer.next();

            let data_len = data.len();
            tracer.sending_data(data_len);

            // Limit maximum USB transfer size to avoid issues with some UDCs.
            while !data.is_empty() {
                let part = data.split_to(data.len().min(max_transfer_size));

                tracer.send_part(part.len());
                ep_tx.send_async(part).await?;
            }

            // If data length align with transfer size of endpoint, we need to insert a
            // zero length packet to indicate that packet is finished.
            if data_len % mps == 0 {
                // WORKAROUND: dummy_hcd driver needs a flush before a ZLP,
                //             otherwise it may deliver partial packets.
                ep_tx.flush_async().await?;

                tracer.send_terminator();
                ep_tx.send_async(Bytes::new()).await?;
            }

            tracer.data_finished();
        }

        // Flush the send queue so all enqueued data reaches the host
        // before we signal closure.
        ep_tx.flush_async().await?;

        Ok(())
    }

    async fn task(
        mut ep0: Custom, ep_tx: EndpointSender, ep_rx: EndpointReceiver,
        conn_tx: mpsc::Sender<(UpcSender, UpcReceiver)>, cfg: Arc<Mutex<Cfg>>,
    ) -> Result<()> {
        let status = ep0.status();

        if let Some(status) = &status {
            tracing::debug!("waiting for device bind");
            status.bound().await?;
        }

        let mut unbound = pin!(async move {
            match status {
                Some(status) => status.unbound().await,
                None => future::pending().await,
            }
        });

        // USB endpoint maximum packet size.
        let mut mps = None;

        let ep_tx = Mutex::new(ep_tx);
        let ep_rx = Mutex::new(ep_rx);

        let mut in_task = future::pending().boxed();
        let mut out_task = future::pending().boxed();
        let mut in_close_tx = watch::channel(None::<u64>).0;

        let mut send_open = false;
        let mut recv_open = false;
        let mut last_status;
        let mut conn_error = watch::channel(None).0;
        let mut host_caps = HostCapabilities::default();
        let mut ping_timeout = DeviceCapabilities::default().ping_timeout;
        let mut timeout_sleep = pin!(sleep(Duration::MAX));

        loop {
            tokio::select! {
                biased;

                () = &mut unbound => {
                    tracing::debug!("device unbound");
                    break;
                }

                () = conn_tx.closed() => {
                    tracing::debug!("UsbPacketDevice dropped");
                    break;
                }

                res = ep0.wait_event() => {
                    res?;

                    match ep0.event()? {
                        Event::Enable => {
                            mps = Some(ep_tx.lock().await.max_packet_size()?);
                            tracing::debug!(mps =% mps.unwrap(), "device enabled");
                            host_caps = HostCapabilities::default();
                        }

                        Event::Disable => {
                            tracing::debug!("device disabled");
                            in_task = future::pending().boxed();
                            out_task = future::pending().boxed();
                            ep_tx.lock().await.cancel()?;
                            ep_rx.lock().await.cancel()?;
                            let _ = ep_tx.lock().await.control()?.discard_fifo();
                            let _ = ep_rx.lock().await.control()?.discard_fifo();
                            send_open = false;
                            recv_open = false;
                            mps = None;
                        }

                        Event::SetupHostToDevice(req) => {
                            let ctrl_req = req.ctrl_req();
                            tracing::debug!("incoming control request: {ctrl_req:?}");
                            match ctrl_req.request {
                                ctrl_req::OPEN => {
                                    tracing::debug!("open connection request");

                                    // Close any previous connection.
                                    if send_open || recv_open {
                                        tracing::debug!("closing previous connection");
                                        in_task = future::pending().boxed();
                                        out_task = future::pending().boxed();
                                        ep_tx.lock().await.cancel()?;
                                        ep_rx.lock().await.cancel()?;
                                        send_open = false;
                                        recv_open = false;
                                        host_caps = HostCapabilities::default();
                                    }

                                    // Clear any halt status left from a previous connection's half-close.
                                    let _ = ep_tx.lock().await.control()?.clear_halt();
                                    let _ = ep_rx.lock().await.control()?.clear_halt();

                                    // Discard stale data in hardware FIFOs from a previous connection.
                                    let _ = ep_tx.lock().await.control()?.discard_fifo();
                                    let _ = ep_rx.lock().await.control()?.discard_fifo();

                                    // Determine limits.
                                    let max_transfer_size = mps.expect("request without enable") * TRANSFER_PACKETS;
                                    let max_send_size = usize::try_from(host_caps.max_size).unwrap_or(usize::MAX);
                                    let max_recv_size = usize::try_from(cfg.lock().await.max_size).unwrap_or(usize::MAX);

                                    // Prepare IN transfers. They must be enqueued before host starts sending.
                                    let mut pool = BytesPool::new(cfg.lock().await.recv_pool_size);
                                    Self::prepare_in(&ep_rx, max_transfer_size, &mut pool).await;

                                    // Accept request from host and start tasks.
                                    match req.recv_all() {
                                        Ok(topic) => {
                                            let (ctx, crx, Head { tx, rx, error }) =
                                                connection(topic, max_transfer_size, max_send_size, max_recv_size);
                                            let _ = conn_tx.send((ctx, crx)).await;
                                            let (close_tx, close_rx) = watch::channel(None);
                                            in_task = Self::in_task(&ep_rx, tx, close_rx, max_transfer_size, pool).boxed();
                                            out_task = Self::out_task(&ep_tx, rx, max_transfer_size).boxed();
                                            in_close_tx = close_tx;
                                            send_open = true;
                                            recv_open = true;
                                            last_status = Instant::now();
                                            if let Some(d) = ping_timeout {
                                                timeout_sleep.as_mut().reset(last_status + d);
                                            }
                                            conn_error = error;
                                            tracing::debug!("connection established");
                                        }
                                        Err(err) => {
                                            tracing::warn!("open request receive error: {err}");
                                            ep_rx.lock().await.cancel()?;
                                        }
                                    }
                                }

                                ctrl_req::CLOSE => {
                                    tracing::debug!("close connection request");
                                    in_task = future::pending().boxed();
                                    out_task = future::pending().boxed();
                                    ep_tx.lock().await.cancel()?;
                                    ep_rx.lock().await.cancel()?;
                                    let _ = ep_tx.lock().await.control()?.discard_fifo();
                                    let _ = ep_rx.lock().await.control()?.discard_fifo();
                                    if let Err(err) = req.recv_all() {
                                        tracing::warn!("close request receive error: {err}");
                                    }
                                    send_open = false;
                                    recv_open = false;
                                    host_caps = HostCapabilities::default();
                                }

                                ctrl_req::CLOSE_SEND => {
                                    tracing::debug!("host closed send direction");
                                    match req.recv_all() {
                                        Ok(data) if data.len() >= 8 => {
                                            let total_bytes = u64::from_le_bytes(data[..8].try_into().unwrap());
                                            tracing::debug!("CLOSE_SEND with total_bytes={total_bytes}");
                                            let _ = in_close_tx.send(Some(total_bytes));
                                        }
                                        Ok(_) => tracing::warn!("CLOSE_SEND without byte count"),
                                        Err(err) => tracing::warn!("close-send request receive error: {err}"),
                                    }
                                }

                                ctrl_req::CLOSE_RECV => {
                                    tracing::debug!("host closed receive direction");
                                    if let Err(err) = req.recv_all() {
                                        tracing::warn!("close-recv request receive error: {err}");
                                    }
                                    if send_open {
                                        // Stop sending to the host (the out_task writes to IN endpoint).
                                        out_task = future::pending().boxed();
                                        ep_tx.lock().await.cancel()?;
                                        send_open = false;
                                        tracing::debug!("send direction closed");
                                    }
                                }

                                ctrl_req::CAPABILITIES => {
                                    match req.recv_all() {
                                        Ok(data) => match HostCapabilities::decode(&data) {
                                            Ok(caps) => {
                                                tracing::debug!("host capabilities: {caps:?}");
                                                host_caps = caps;
                                            }
                                            Err(err) => tracing::warn!("host capabilities decode error: {err}"),
                                        },
                                        Err(err) => tracing::warn!("host capabilities receive error: {err}"),
                                    }
                                }

                                ctrl_req::ECHO => {
                                    if !send_open && !recv_open {
                                        match req.recv_all() {
                                            Ok(data) => {
                                                tracing::debug!("echoing {} bytes via bulk IN", data.len());
                                                let mut ep = ep_tx.lock().await;
                                                if let Err(err) = ep.send_async(Bytes::from(data)).await {
                                                    tracing::warn!("echo send error: {err}");
                                                }
                                                if let Err(err) = ep.flush_async().await {
                                                    tracing::warn!("echo flush error: {err}");
                                                }
                                            }
                                            Err(err) => tracing::warn!("echo request receive error: {err}"),
                                        }
                                    } else {
                                        tracing::warn!("ECHO rejected: connection is open");
                                    }
                                }

                                other => tracing::warn!("unknown control request {other:x}"),
                            }
                        }

                        Event::SetupDeviceToHost(req) => {
                            let ctrl_req = req.ctrl_req();
                            tracing::debug!("outgoing control request: {ctrl_req:?}");
                            match ctrl_req.request {
                                ctrl_req::PROBE => {
                                    tracing::debug!("sending probe response");
                                    if let Err(err) = req.send(ctrl_req::PROBE_RESPONSE) {
                                        tracing::warn!("probe send error: {err}");
                                    }
                                }

                                ctrl_req::INFO => {
                                    tracing::debug!("sending info");
                                    if let Err(err) = req.send(&cfg.lock().await.info) {
                                        tracing::warn!("info send error: {err}");
                                    }
                                }

                                ctrl_req::CAPABILITIES => {
                                    tracing::debug!("sending capabilities");
                                    let cfg = cfg.lock().await;
                                    ping_timeout = cfg.ping_timeout;
                                    let caps = DeviceCapabilities {
                                        ping_timeout,
                                        status_supported: true,
                                        echo_supported: true,
                                        max_size: cfg.max_size,
                                    };
                                    if let Err(err) = req.send(&caps.encode()) {
                                        tracing::warn!("capabilities send error: {err}");
                                    }
                                }

                                ctrl_req::STATUS => {
                                    last_status = Instant::now();
                                    if let Some(d) = ping_timeout {
                                        timeout_sleep.as_mut().reset(last_status + d);
                                    }
                                    let mut buf = [0u8; status::MAX_SIZE];
                                    let mut len = 0;
                                    if !recv_open {
                                        buf[len] = status::RECV_CLOSED;
                                        len += 1;
                                    }
                                    tracing::debug!("sending status ({len} bytes)");
                                    if let Err(err) = req.send(&buf[..len]) {
                                        tracing::warn!("status send error: {err}");
                                    }
                                }

                                other => tracing::warn!("unknown control request {other:x}"),
                            }
                        }

                        _ => (),
                    }
                }

                () = &mut timeout_sleep, if ping_timeout.is_some() && (send_open || recv_open) => {
                    tracing::warn!("host ping timeout, closing connection");
                    let _ = conn_error.send(Some(ErrorKind::TimedOut));
                    in_task = future::pending().boxed();
                    out_task = future::pending().boxed();
                    ep_tx.lock().await.cancel()?;
                    ep_rx.lock().await.cancel()?;
                    send_open = false;
                    recv_open = false;
                }

                res = &mut in_task => {
                    in_close_tx = watch::channel(None).0;
                    in_task = future::pending().boxed();
                    ep_rx.lock().await.cancel()?;
                    recv_open = false;
                    match res {
                        Ok(true) => {
                            // Host-initiated: CLOSE_SEND byte count reached.
                            // All expected data has been received.
                            tracing::debug!("all host bytes received after CLOSE_SEND");
                        }
                        Ok(false) => {
                            // Device-initiated: UpcReceiver was dropped.
                            // Halt the OUT endpoint so the host learns about it.
                            tracing::debug!("device receiver dropped");
                            let _ = ep_rx.lock().await.control()?.halt();
                            tracing::debug!("OUT endpoint halted (device done receiving)");
                        }
                        Err(err) => tracing::warn!("device receiver failed: {err}"),
                    }
                }

                res = &mut out_task => {
                    // out_task ended: device-side UpcSender was dropped.
                    // The send queue has been flushed, so all data has been delivered.
                    // Halt the IN endpoint and send notification to signal the host.
                    match res {
                        Ok(()) => tracing::debug!("device sender dropped"),
                        Err(err) => tracing::warn!("device sender failed: {err}")
                    }
                    send_open = false;
                    out_task = future::pending().boxed();
                    let _ = ep_tx.lock().await.control()?.halt();
                    tracing::debug!("IN endpoint halted (device done sending)");
                }
            }
        }

        Ok(())
    }
}
