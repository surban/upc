//! Device-side USB packet channel (UPC).
//!
//! Use [`UpcFunction::new`] to create an USB function that accepts incoming connections.
//! Pass the returned handle to the [`usb_gadget`] library to expose the USB gadget.
//!

use bytes::{Bytes, BytesMut};
use futures::{
    future, sink,
    stream::{self, BoxStream},
    FutureExt, Sink, SinkExt, Stream, StreamExt,
};
use std::{
    fmt,
    future::Future,
    io::{Error, ErrorKind, Result},
    mem::take,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};
use tokio::{
    sync::{mpsc, Mutex},
    task::JoinSet,
};
use usb_gadget::function::{
    custom::{
        Custom, Endpoint, EndpointDirection, EndpointReceiver, EndpointSender, Event, Interface, OsExtCompat,
        OsExtProp,
    },
    Handle,
};
use uuid::Uuid;

use crate::{Class, CTRL_REQ_CLOSE, CTRL_REQ_INFO, CTRL_REQ_OPEN, INFO_SIZE, MAX_SIZE};

/// Sends data into a USB packet channel.
pub struct UpcSender {
    tx: mpsc::Sender<Bytes>,
}

impl fmt::Debug for UpcSender {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("UpcSender").finish()
    }
}

impl UpcSender {
    /// Send packet.
    ///
    /// ## Cancel safety
    /// If canceled, no data will have been sent.
    pub async fn send(&self, data: Bytes) -> Result<()> {
        self.tx.send(data).await.map_err(|_| Error::new(ErrorKind::ConnectionReset, "connection closed"))
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
    max_packet_size: usize,
}

impl fmt::Debug for UpcReceiver {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("UpcReceiver").field("topic", &self.topic).finish()
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
    pub async fn recv(&mut self) -> Result<BytesMut> {
        loop {
            let packet = self
                .rx
                .recv()
                .await
                .ok_or_else(|| Error::new(ErrorKind::ConnectionReset, "connection closed"))?;
            let packet_len = packet.len();
            self.buffer.unsplit(packet);

            if self.buffer.len() > self.max_size {
                self.buffer.clear();
                return Err(Error::new(
                    ErrorKind::InvalidData,
                    "incoming packet exceeds maximum receive packet size",
                ));
            }

            if packet_len < self.max_packet_size {
                return Ok(take(&mut self.buffer));
            }
        }
    }

    /// The maximum receive packet size.
    pub fn max_size(&self) -> usize {
        self.max_size
    }

    /// Sets the maximum receive packet size.
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

        UpcStream(stream.boxed())
    }
}

/// Packet stream from a USB packet channel.
pub struct UpcStream(BoxStream<'static, Result<BytesMut>>);

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

struct Head {
    tx: mpsc::Sender<BytesMut>,
    rx: mpsc::Receiver<Bytes>,
}

fn connection(topic: Vec<u8>, max_packet_size: usize) -> (UpcSender, UpcReceiver, Head) {
    let (tx_in, rx_in) = mpsc::channel(32);
    let (tx_out, rx_out) = mpsc::channel(32);
    let sender = UpcSender { tx: tx_out };
    let recv = UpcReceiver { topic, rx: rx_in, buffer: BytesMut::new(), max_size: MAX_SIZE, max_packet_size };
    let head = Head { tx: tx_in, rx: rx_out };
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
    info: Arc<Mutex<Vec<u8>>>,
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
    pub fn new(interface_id: InterfaceId) -> (Self, Handle) {
        let (ep_rx, ep_rx_dir) = EndpointDirection::host_to_device();
        let (ep_tx, ep_tx_dir) = EndpointDirection::device_to_host();

        let mut interface = Interface::new(interface_id.class.into(), &interface_id.name)
            .with_endpoint(Endpoint::bulk(ep_rx_dir))
            .with_endpoint(Endpoint::bulk(ep_tx_dir))
            .with_os_ext_compat(OsExtCompat::winusb());
        if let Some(guid) = interface_id.guid {
            interface = interface.with_os_ext_prop(OsExtProp::device_interface_guid(guid));
        }

        let (ep0, handle) = Custom::builder().with_interface(interface).build();

        let (conn_tx, conn_rx) = mpsc::channel(4);
        let info = Arc::new(Mutex::new(Vec::new()));

        let mut task = JoinSet::new();
        task.spawn(Self::task(ep0, ep_tx, ep_rx, conn_tx, info.clone()));

        (Self { class: interface_id.class, name: interface_id.name.to_string(), task, conn_rx, info }, handle)
    }

    /// Sets the info data readable by host.
    ///
    /// # Panics
    /// Panics if info is larger than [`INFO_SIZE`].
    pub async fn set_info(&self, info: Vec<u8>) {
        assert!(info.len() <= INFO_SIZE, "info too big");
        *self.info.lock().await = info;
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

    async fn in_task(ep_rx: &Mutex<EndpointReceiver>, tx: mpsc::Sender<BytesMut>) -> Result<()> {
        let mut ep_rx = ep_rx.lock().await;
        let max_packet_size = ep_rx.max_packet_size()?;

        while let Ok(permit) = tx.reserve().await {
            if let Some(data) = ep_rx.recv_async(BytesMut::with_capacity(max_packet_size)).await? {
                permit.send(data);
            }
        }

        Ok(())
    }

    async fn out_task(ep_tx: &Mutex<EndpointSender>, mut rx: mpsc::Receiver<Bytes>) -> Result<()> {
        let mut ep_tx = ep_tx.lock().await;
        let max_packet_size = ep_tx.max_packet_size()?;

        while let Some(mut data) = rx.recv().await {
            loop {
                let part = data.split_to(data.len().min(max_packet_size));
                let part_len = part.len();
                ep_tx.send_async(part).await?;
                if part_len != max_packet_size {
                    break;
                }
            }
        }

        Ok(())
    }

    async fn task(
        mut ep0: Custom, mut ep_tx: EndpointSender, ep_rx: EndpointReceiver,
        conn_tx: mpsc::Sender<(UpcSender, UpcReceiver)>, info: Arc<Mutex<Vec<u8>>>,
    ) -> Result<()> {
        let status = ep0.status();

        tracing::debug!("waiting for device bind");
        status.bound().await?;

        let max_packet_size = ep_tx.max_packet_size()?;
        tracing::debug!("maximum packet size is {max_packet_size} bytes");

        let ep_tx = Mutex::new(ep_tx);
        let ep_rx = Mutex::new(ep_rx);

        let mut in_task = future::pending().boxed();
        let mut out_task = future::pending().boxed();

        let mut do_halt = false;
        let mut is_open = false;

        loop {
            if do_halt {
                tracing::debug!("halting endpoints");
                in_task = future::pending().boxed();
                out_task = future::pending().boxed();
                ep_tx.lock().await.cancel()?;
                ep_rx.lock().await.cancel()?;
                let _ = ep_tx.lock().await.control()?.halt();
                let _ = ep_rx.lock().await.control()?.halt();
                do_halt = false;
                is_open = false;
            }

            tokio::select! {
                () = status.unbound() => {
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
                        Event::Enable => tracing::debug!("device enabled"),

                        Event::Disable => {
                            tracing::debug!("device disabled");
                            in_task = future::pending().boxed();
                            out_task = future::pending().boxed();
                            ep_tx.lock().await.cancel()?;
                            ep_rx.lock().await.cancel()?;
                            is_open = false;
                        }

                        Event::SetupHostToDevice(req) => {
                            let ctrl_req = req.ctrl_req();
                            tracing::debug!("incoming control request: {ctrl_req:?}");
                            match ctrl_req.request {
                                CTRL_REQ_OPEN => {
                                    tracing::debug!("open connection request");

                                    if is_open {
                                        tracing::debug!("closing previous connection");
                                        in_task = future::pending().boxed();
                                        out_task = future::pending().boxed();
                                        ep_tx.lock().await.cancel()?;
                                        ep_rx.lock().await.cancel()?;
                                    }

                                    // WORKAROUND: some UDCs fail the first incoming transfer when no
                                    //             receive buffers are enqueued.
                                    while ep_rx.lock().await.try_recv(BytesMut::with_capacity(max_packet_size)).is_ok() {}

                                    match req.recv_all() {
                                        Ok(topic) => {
                                            let (ctx, crx, Head { tx, rx }) = connection(topic, max_packet_size);
                                            let _ = conn_tx.send((ctx, crx)).await;
                                            in_task = Self::in_task(&ep_rx, tx).boxed();
                                            out_task = Self::out_task(&ep_tx, rx).boxed();
                                            is_open = true;
                                            tracing::debug!("connection established");
                                        }
                                        Err(err) => {
                                            tracing::warn!("topic receive error: {err}");
                                            ep_rx.lock().await.cancel()?;
                                        }
                                    }
                                }

                                CTRL_REQ_CLOSE => {
                                    tracing::debug!("close connection request");
                                    in_task = future::pending().boxed();
                                    out_task = future::pending().boxed();
                                    ep_tx.lock().await.cancel()?;
                                    ep_rx.lock().await.cancel()?;
                                    if let Err(err) = req.recv_all() {
                                        tracing::warn!("close request receive error: {err}");
                                    }
                                    is_open = false;
                                }

                                other => tracing::warn!("unknown control request {other:x}"),
                            }
                        }

                        Event::SetupDeviceToHost(req) => {
                            let ctrl_req = req.ctrl_req();
                            tracing::debug!("outgoing control request: {ctrl_req:?}");
                            match ctrl_req.request {
                                CTRL_REQ_INFO => {
                                    tracing::debug!("sending info");
                                    if let Err(err) = req.send(&info.lock().await) {
                                        tracing::warn!("info send error: {err}");
                                    }
                                }
                                other => tracing::warn!("unknown control request {other:x}"),
                            }
                        }

                        _ => (),
                    }
                }

                res = &mut in_task => {
                    match res {
                        Ok(()) => tracing::debug!("receiver dropped"),
                        Err(err) => tracing::warn!("receiver failed: {err}")
                    }
                    do_halt = true;
                }

                res = &mut out_task => {
                    match res {
                        Ok(()) => tracing::debug!("sender dropped"),
                        Err(err) => tracing::warn!("sender failed: {err}")
                    }
                    do_halt = true;
                }

            }
        }

        Ok(())
    }
}
