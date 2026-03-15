//! Packet tracing.

#![allow(dead_code)]

#[cfg(feature = "trace-packets")]
mod inner {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tracing::Span;

    /// Packet tracer.
    pub(crate) struct Tracer {
        span: Span,
        seq: AtomicUsize,
        packet_span: Span,
    }

    impl Tracer {
        pub(super) fn new(span: Span) -> Self {
            Self { span, seq: AtomicUsize::new(0), packet_span: Span::none() }
        }

        pub fn device_enqueue() -> Self {
            Self::new(tracing::trace_span!("device_enqueue"))
        }

        pub fn device_dequeue() -> Self {
            Self::new(tracing::trace_span!("device_dequeue"))
        }

        pub fn device_in() -> Self {
            Self::new(tracing::trace_span!("device_in"))
        }

        pub fn device_out() -> Self {
            Self::new(tracing::trace_span!("device_out"))
        }

        pub fn host_enqueue(name: &str) -> Self {
            Self::new(tracing::trace_span!("host_enqueue", %name))
        }

        pub fn host_dequeue(name: &str) -> Self {
            Self::new(tracing::trace_span!("host_dequeue", %name))
        }

        pub fn host_in(name: &str) -> Self {
            Self::new(tracing::trace_span!("host_in", %name))
        }

        pub fn host_out(name: &str) -> Self {
            Self::new(tracing::trace_span!("host_out", %name))
        }

        pub fn next(&mut self) {
            let seq = self.seq.fetch_add(1, Ordering::Relaxed);
            self.packet_span = tracing::trace_span!(parent: &self.span, "data", seq);
        }

        pub fn enqueued(&self, len: usize) {
            let seq = self.seq.fetch_add(1, Ordering::Relaxed);
            let span = tracing::trace_span!(parent: &self.span, "data", seq);
            let _enter = span.enter();
            tracing::trace!(target: "upc", len, "data enqueued for sending");
        }

        pub fn received_data(&self, len: usize) {
            let _enter = self.packet_span.enter();
            tracing::trace!(target: "upc", %len, "received data");
        }

        pub fn dequeued_part(&self, len: usize) {
            let _enter = self.packet_span.enter();
            tracing::trace!(target: "upc", len, "dequeued part of data");
        }

        pub fn dequeing(&self) {
            let _enter = self.packet_span.enter();
            tracing::trace!(target: "upc", "dequeing received data");
        }

        pub fn dequeued(&self, len: usize) {
            let _enter = self.packet_span.enter();
            tracing::trace!(target: "upc", len, "dequeued received data");
        }

        pub fn deque_transfer(&self, len: usize, short: bool) {
            let _enter = self.packet_span.enter();
            tracing::trace!(target: "upc", len, short, "dequeued received transfer");
        }

        pub fn sending_data(&self, len: usize) {
            let _enter = self.packet_span.enter();
            tracing::trace!(target: "upc", %len, "sending data");
        }

        pub fn send_part(&self, len: usize) {
            let _enter = self.packet_span.enter();
            tracing::trace!(target: "upc", %len, "sent packet part");
        }

        pub fn send_terminator(&self) {
            let _enter = self.packet_span.enter();
            tracing::trace!(target: "upc", "sent zero length packet terminator");
        }

        pub fn data_finished(&self) {
            let _enter = self.packet_span.enter();
            tracing::trace!(target: "upc", "sending data finished");
        }

        pub fn received_packet(&self, len: usize) {
            let _enter = self.span.enter();
            tracing::trace!(target: "upc", len, "received USB transfer");
        }
    }
}

#[cfg(not(feature = "trace-packets"))]
mod inner {
    /// No-op packet tracer.
    pub(crate) struct Tracer;

    impl Tracer {
        #[inline(always)]
        pub fn device_enqueue() -> Self {
            Self
        }

        #[inline(always)]
        pub fn device_dequeue() -> Self {
            Self
        }

        #[inline(always)]
        pub fn device_in() -> Self {
            Self
        }

        #[inline(always)]
        pub fn device_out() -> Self {
            Self
        }

        #[inline(always)]
        pub fn host_enqueue(_name: &str) -> Self {
            Self
        }

        #[inline(always)]
        pub fn host_dequeue(_name: &str) -> Self {
            Self
        }

        #[inline(always)]
        pub fn host_in(_name: &str) -> Self {
            Self
        }

        #[inline(always)]
        pub fn host_out(_name: &str) -> Self {
            Self
        }

        #[inline(always)]
        pub fn next(&mut self) {}
        #[inline(always)]
        pub fn enqueued(&self, _len: usize) {}
        #[inline(always)]
        pub fn received_data(&self, _len: usize) {}
        #[inline(always)]
        pub fn dequeued_part(&self, _len: usize) {}
        #[inline(always)]
        pub fn dequeing(&self) {}
        #[inline(always)]
        pub fn dequeued(&self, _len: usize) {}
        #[inline(always)]
        pub fn deque_transfer(&self, _len: usize, _short: bool) {}
        #[inline(always)]
        pub fn sending_data(&self, _len: usize) {}
        #[inline(always)]
        pub fn send_part(&self, _len: usize) {}
        #[inline(always)]
        pub fn send_terminator(&self) {}
        #[inline(always)]
        pub fn data_finished(&self) {}
        #[inline(always)]
        pub fn received_packet(&self, _len: usize) {}
    }
}

#[allow(unused)]
pub(crate) use inner::Tracer;
