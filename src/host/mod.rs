#![allow(rustdoc::broken_intra_doc_links)]
//! Host-side USB packet channel (UPC)
//!
//! ### Native platforms (crate feature `native`)
//!
//! To open a channel, use [`nusb`] to find the target device and then pass it to [`connect`].
//!
//! Some errors from this module have an inner error type of [`nusb::Error`].
//!
//! ### Web platform (crate feature `web` and targeting `wasm32-*`)
//!
//! To open a channel, use [`webusb_web`] to find the target device and then pass it to [`connect`].
//!
//! Some errors from this module have an inner error type of [`webusb_web::Error`].
//!

use bytes::{Bytes, BytesMut};
use futures::{sink, stream, Sink, SinkExt, Stream, StreamExt};
use std::{
    fmt,
    future::Future,
    io::{Error, ErrorKind, Result},
    mem::take,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
    time::Duration,
};
use tokio::sync::mpsc;

#[cfg(feature = "web")]
mod guard;
#[cfg(feature = "web")]
mod web;
#[cfg(feature = "web")]
pub use web::*;

#[cfg(not(feature = "web"))]
mod native;
#[cfg(not(feature = "web"))]
pub use native::*;

/// Device status reported by the periodic STATUS control request.
#[derive(Clone, Copy, Default)]
pub(crate) struct DeviceStatus {
    /// Device receiver has been dropped (device done receiving).
    pub(crate) recv_closed: bool,
    /// Device is unreachable (ping timed out).
    pub(crate) dead: bool,
}

/// Options for connecting to a USB packet channel.
///
/// Use [`UpcOptions::new`] to create a default set of options,
/// then chain `with_*` methods to customize.
#[derive(Debug, Clone)]
pub struct UpcOptions {
    pub(crate) topic: Vec<u8>,
    pub(crate) ping_interval: Option<Duration>,
}

impl UpcOptions {
    /// Creates a new set of default connection options.
    pub fn new() -> Self {
        Self { topic: Vec::new(), ping_interval: Some(Duration::from_secs(5)) }
    }

    /// Sets the topic data provided to the device.
    ///
    /// The maximum size is [`crate::INFO_SIZE`].
    pub fn with_topic(mut self, topic: Vec<u8>) -> Self {
        self.topic = topic;
        self
    }

    /// Sets the requested interval for pinging the device.
    ///
    /// The actual ping interval is the minimum of this value and half
    /// the device's ping timeout (set via [`crate::device::UpcFunction::set_ping_timeout`]).
    /// If the device does not support status polling, it is disabled
    /// regardless of this setting.
    ///
    /// The default is 5 seconds.
    pub fn with_ping_interval(mut self, ping_interval: Option<Duration>) -> Self {
        self.ping_interval = ping_interval;
        self
    }

    /// Returns the topic data.
    pub fn topic(&self) -> &[u8] {
        &self.topic
    }

    /// Returns the requested ping interval.
    pub fn ping_interval(&self) -> Option<Duration> {
        self.ping_interval
    }
}

impl Default for UpcOptions {
    fn default() -> Self {
        Self::new()
    }
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
            Err(_) => Err(self
                .shared
                .error
                .lock()
                .unwrap()
                .clone()
                .map(to_io_err)
                .unwrap_or_else(|| Error::new(ErrorKind::BrokenPipe, "UPC channel closed"))),
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
    rx: mpsc::Receiver<Bytes>,
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
    pub async fn recv(&mut self) -> Result<Option<Bytes>> {
        loop {
            let Some(packet) = self.rx.recv().await else {
                // Channel closed — check if there was an error or a clean half-close.
                return match self.shared.error.lock().unwrap().clone() {
                    Some(err) => Err(to_io_err(err)),
                    None => Ok(None),
                };
            };

            let packet_len = packet.len();
            self.buffer.unsplit(packet.into());

            if self.buffer.len() > self.max_size {
                self.buffer.clear();
                return Err(Error::new(ErrorKind::OutOfMemory, "maximum packet size exceeded"));
            }

            if packet_len < self.max_transfer_size {
                return Ok(Some(take(&mut self.buffer).into()));
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
                Ok(Some(data)) => Ok(Some((data, this))),
                Ok(None) => Ok(None),
                Err(err) => Err(err),
            }
        });

        UpcStream(Box::pin(stream))
    }
}

/// Packet stream from a USB packet channel.
pub struct UpcStream(Pin<Box<dyn Stream<Item = Result<Bytes>> + Send + Sync + 'static>>);

impl fmt::Debug for UpcStream {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_tuple("UpcStream").finish()
    }
}

impl Stream for UpcStream {
    type Item = Result<Bytes>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Option<Self::Item>> {
        Pin::into_inner(self).0.poll_next_unpin(cx)
    }
}
