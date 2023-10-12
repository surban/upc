//! Device-side USB packet channel.

use futures_util::{future, FutureExt};
use std::{
    fmt,
    future::Future,
    io::{Error, ErrorKind, Result},
    sync::Arc,
};
use tokio::{
    sync::{mpsc, Mutex},
    task::JoinSet,
};

use usb_gadget::function::{
    custom::{Custom, Endpoint, EndpointDirection, EndpointReceiver, EndpointSender, Event, Interface},
    Handle,
};

use crate::{Class, BUFFER_SIZE, CTRL_REQ_CLOSE, CTRL_REQ_INFO, CTRL_REQ_OPEN};

/// Sends data into a USB packet channel.
pub struct UpcSender {
    tx: mpsc::Sender<Vec<u8>>,
}

impl fmt::Debug for UpcSender {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("UpcSender").finish()
    }
}

impl UpcSender {
    /// Send packet.
    pub async fn send(&self, data: Vec<u8>) -> Result<()> {
        self.tx.send(data).await.map_err(|_| Error::new(ErrorKind::ConnectionReset, "connection closed"))
    }

    /// Wait until connection is closed.
    pub fn closed(&self) -> impl Future<Output = ()> {
        let tx = self.tx.clone();
        async move { tx.closed().await }
    }
}

/// Receives data from a USB packet channel.
pub struct UpcReceiver {
    topic: Vec<u8>,
    rx: mpsc::Receiver<Vec<u8>>,
}

impl fmt::Debug for UpcReceiver {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("UpcReceiver").field("topic", &self.topic).finish()
    }
}

impl UpcReceiver {
    /// Topic provided by host.
    pub fn topic(&self) -> &[u8] {
        &self.topic
    }

    /// Receive packet.
    pub async fn recv(&mut self) -> Option<Vec<u8>> {
        self.rx.recv().await
    }
}

struct Head {
    tx: mpsc::Sender<Vec<u8>>,
    rx: Mutex<mpsc::Receiver<Vec<u8>>>,
}

fn connection(topic: Vec<u8>) -> (UpcSender, UpcReceiver, Head) {
    let (tx_in, rx_in) = mpsc::channel(16);
    let (tx_out, rx_out) = mpsc::channel(16);
    let sender = UpcSender { tx: tx_out };
    let recv = UpcReceiver { topic, rx: rx_in };
    let head = Head { tx: tx_in, rx: Mutex::new(rx_out) };
    (sender, recv, head)
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
    /// Creates a new USB packet device-side interface with the specified interface class
    /// and interface name.
    pub fn new(class: Class, name: impl AsRef<str>) -> (Self, Handle) {
        let name = name.as_ref();

        let (ep_rx, ep_rx_dir) = EndpointDirection::host_to_device();
        let (ep_tx, ep_tx_dir) = EndpointDirection::device_to_host();

        let (ep0, handle) = Custom::builder()
            .with_interface(
                Interface::new(class.into(), name)
                    .with_endpoint(Endpoint::bulk(ep_rx_dir))
                    .with_endpoint(Endpoint::bulk(ep_tx_dir)),
            )
            .build();

        let (conn_tx, conn_rx) = mpsc::channel(4);
        let info = Arc::new(Mutex::new(Vec::new()));

        let mut task = JoinSet::new();
        task.spawn(Self::task(ep0, ep_tx, ep_rx, conn_tx, info.clone()));

        (Self { class, name: name.to_string(), task, conn_rx, info }, handle)
    }

    /// Sets the info data readable by host.
    pub async fn set_info(&self, info: Vec<u8>) {
        *self.info.lock().await = info;
    }

    /// Waits for and accepts an incoming connection.
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

    async fn task(
        mut ep0: Custom, mut ep_tx: EndpointSender, mut ep_rx: EndpointReceiver,
        conn_tx: mpsc::Sender<(UpcSender, UpcReceiver)>, info: Arc<Mutex<Vec<u8>>>,
    ) -> Result<()> {
        ep0.status().bound().await?;

        let mut conn: Option<Head> = None;

        loop {
            let status = ep0.status();

            let conn_task_tx = match &conn {
                Some(conn) => async {
                    match conn.rx.lock().await.recv().await {
                        Some(data) => match ep_tx.send_async(data).await {
                            Ok(()) => true,
                            Err(err) => {
                                tracing::warn!("send error: {err}");
                                false
                            }
                        },
                        None => {
                            tracing::debug!("connection closed");
                            false
                        }
                    }
                }
                .left_future(),
                None => future::pending().right_future(),
            };

            let conn_task_rx = match &conn {
                Some(conn) => async {
                    match ep_rx.recv_async(BUFFER_SIZE).await {
                        Ok(Some(data)) => conn.tx.send(data).await.is_ok(),
                        Ok(None) => true,
                        Err(err) => {
                            tracing::warn!("receive error: {err}");
                            false
                        }
                    }
                }
                .left_future(),
                None => future::pending().right_future(),
            };

            let event_task = async {
                ep0.wait_event().await?;
                match ep0.event()? {
                    Event::Enable => tracing::debug!("device enabled"),
                    Event::Disable => {
                        tracing::debug!("device disabled");
                        return Ok(Some(None));
                    }
                    Event::SetupHostToDevice(req) => {
                        let ctrl_req = req.ctrl_req();
                        tracing::debug!("incoming control request: {ctrl_req:?}");
                        match ctrl_req.request {
                            CTRL_REQ_OPEN => {
                                tracing::debug!("open connection request");
                                if let Ok(permit) = conn_tx.reserve().await {
                                    match req.recv_all() {
                                        Ok(topic) => {
                                            let (conn_tx, conn_rx, conn_head) = connection(topic);
                                            permit.send((conn_tx, conn_rx));
                                            return Ok(Some(Some(conn_head)));
                                        }
                                        Err(err) => tracing::warn!("topic receive error: {err}"),
                                    }
                                }
                            }
                            CTRL_REQ_CLOSE => {
                                tracing::debug!("close connection request");
                                return Ok(Some(None));
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
                                let info = info.lock().await;
                                if let Err(err) = req.send(&info) {
                                    tracing::warn!("info send error: {err}");
                                }
                            }
                            other => tracing::warn!("unknown control request {other:x}"),
                        }
                    }
                    _ => (),
                }

                Ok::<_, Error>(None)
            };

            tokio::select! {
                () = status.unbound() => {
                    tracing::debug!("device unbound");
                    break;
                }
                () = conn_tx.closed() => {
                    tracing::debug!("UsbPacketDevice dropped");
                    break;
                }
                ok = conn_task_tx => {
                    if !ok {
                        tracing::debug!("connection closed locally");
                        conn = None;
                        ep_tx.cancel()?;
                        ep_rx.cancel()?;
                        ep_tx.control()?.halt()?;
                        ep_rx.control()?.halt()?;
                    }
                }
                ok = conn_task_rx => {
                    if !ok {
                        tracing::debug!("connection closed locally");
                        conn = None;
                        ep_tx.cancel()?;
                        ep_rx.cancel()?;
                        ep_tx.control()?.halt()?;
                        ep_rx.control()?.halt()?;
                    }
                }
                res = event_task => {
                    if let Some(new_conn) = res? {
                        conn = new_conn;
                        if conn.is_none() {
                            ep_tx.cancel()?;
                            ep_rx.cancel()?;
                        }
                    }
                }
            }
        }

        Ok(())
    }
}
