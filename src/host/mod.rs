#![allow(rustdoc::broken_intra_doc_links)]
//! Host-side USB packet channel (UPC)
//!
//! ### Native platforms (crate feature `native`)
//!
//! To open a channel, use [`rusb`] to find the target device and then pass it to [`connect`].
//!
//! Some errors from this module have an inner error type of [`rusb::Error`].
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
    collections::HashSet,
    fmt,
    future::Future,
    io::{Error, ErrorKind, Result},
    mem::take,
    pin::Pin,
    sync::{Arc, LazyLock, Mutex},
    task::{Context, Poll},
};
use tokio::sync::mpsc;

#[cfg(feature = "web")]
mod web;
#[cfg(feature = "web")]
pub use web::*;

#[cfg(not(feature = "web"))]
mod native;
#[cfg(not(feature = "web"))]
pub use native::*;

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
                .unwrap_or_else(|| Error::new(ErrorKind::BrokenPipe, "UPC task terminated"))),
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
    rx: mpsc::Receiver<BytesMut>,
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
    pub async fn recv(&mut self) -> Result<BytesMut> {
        loop {
            let Some(packet) = self.rx.recv().await else {
                return Err(Error::new(ErrorKind::BrokenPipe, "UPC channel closed"));
            };

            let packet_len = packet.len();
            self.buffer.unsplit(packet);

            if self.buffer.len() > self.max_size {
                self.buffer.clear();
                return Err(Error::new(ErrorKind::OutOfMemory, "maximum packet size exceeded"));
            }

            if packet_len < self.max_transfer_size {
                return Ok(take(&mut self.buffer));
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
                Ok(data) => Ok(Some((data, this))),
                Err(err) if err.kind() == ErrorKind::ConnectionReset => Ok(None),
                Err(err) => Err(Error::new(ErrorKind::ConnectionReset, err)),
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

static IN_USE: LazyLock<Mutex<HashSet<(usize, u8)>>> = LazyLock::new(|| Mutex::new(HashSet::new()));

pub(crate) struct InUseGuard {
    handle: usize,
    interface: u8,
}

impl InUseGuard {
    pub fn new(handle: usize, interface: u8) -> Result<Self> {
        let mut in_use = IN_USE.lock().unwrap();

        if in_use.contains(&(handle, interface)) {
            return Err(Error::new(ErrorKind::ResourceBusy, "interface is used by another UPC channel"));
        }

        in_use.insert((handle, interface));

        Ok(Self { handle, interface })
    }
}

impl Drop for InUseGuard {
    fn drop(&mut self) {
        let mut in_use = IN_USE.lock().unwrap();
        in_use.remove(&(self.handle, self.interface));
    }
}
