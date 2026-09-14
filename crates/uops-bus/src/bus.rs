//! The boundary — SPEC §M0.7, PLAN §0 change #4: *keep the boundary, defer the daemon.*
//!
//! 10–100 devices on one box does not need a durable broker, and running one would be a
//! second thing to install, monitor and back up on a customer's hardware. But a bus
//! bolted on later is a rewrite of every producer and consumer, so the **trait** exists
//! from the first commit and the daemon does not.
//!
//! # Why `ack` exists on day one, doing nothing
//!
//! [`InProcessBus`](crate::InProcessBus) acknowledges into a void: there is no
//! redelivery to suppress. The method exists anyway because the alternative is a
//! pipeline written without it, and *that* is what makes the NATS migration a rewrite.
//! A consumer that acknowledges after a durable write already behaves correctly under
//! `JetStream`; a consumer that never learned to is a redesign.
//!
//! So the contract below is written to **`JetStream`'s** semantics rather than to what a
//! channel happens to do, and [`crate::conformance`] is the executable version of it.

use async_trait::async_trait;
use futures_core::stream::BoxStream;
use uops_core::TelemetryEnvelope;

use crate::error::Result;
use crate::subject::{Subject, SubjectPattern};

/// Why a message is being returned to the bus.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Nack {
    /// Transient: the consumer could not write it, and a retry may succeed. Under
    /// `JetStream` this is a redelivery.
    Retry,
    /// Permanent: this message will never be processable — a malformed envelope, a
    /// signal the build does not implement. Redelivering it forever is a poison pill,
    /// so it goes to the dead-letter path instead.
    Undeliverable,
}

/// A handle back to the bus for one message.
///
/// Not `Clone`: an acknowledgement is about one delivery, and a copy of it is a second
/// answer to a question that was asked once.
#[derive(Debug)]
pub struct AckHandle {
    /// `None` for a bus with no redelivery. Kept as a field rather than as a separate
    /// type so the consumer's code is identical under both.
    responder: Option<tokio::sync::oneshot::Sender<Outcome>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    Acked,
    Nacked(Nack),
}

impl AckHandle {
    /// A handle that discards the answer. What `InProcessBus` hands out.
    #[must_use]
    pub const fn noop() -> Self {
        Self { responder: None }
    }

    /// A handle that reports the outcome back to whoever is listening.
    #[must_use]
    pub const fn reporting(responder: tokio::sync::oneshot::Sender<Outcome>) -> Self {
        Self {
            responder: Some(responder),
        }
    }

    fn answer(self, outcome: Outcome) {
        if let Some(responder) = self.responder {
            // The receiver being gone is normal — nobody is required to be listening.
            let _ = responder.send(outcome);
        }
    }
}

/// One message, and the means to answer for it.
#[derive(Debug)]
pub struct Delivery {
    envelope: TelemetryEnvelope,
    ack: AckHandle,
}

impl Delivery {
    #[must_use]
    pub const fn new(envelope: TelemetryEnvelope, ack: AckHandle) -> Self {
        Self { envelope, ack }
    }

    #[must_use]
    pub const fn envelope(&self) -> &TelemetryEnvelope {
        &self.envelope
    }

    /// Take the envelope and acknowledge in one step.
    ///
    /// The shape that is safe to write: a consumer that acknowledges *before* its
    /// durable write has already lost the message if it crashes in between. The verbose
    /// form is available for consumers that need to write first.
    #[must_use = "taking the envelope without using it drops the message"]
    pub fn take(self) -> TelemetryEnvelope {
        self.ack.answer(Outcome::Acked);
        self.envelope
    }

    /// Acknowledge: this message has been dealt with and must not be redelivered.
    pub fn ack(self) {
        self.ack.answer(Outcome::Acked);
    }

    /// Return it to the bus.
    pub fn nack(self, reason: Nack) {
        self.ack.answer(Outcome::Nacked(reason));
    }

    /// Split, for a consumer that must write durably before answering.
    #[must_use]
    pub fn split(self) -> (TelemetryEnvelope, AckHandle) {
        (self.envelope, self.ack)
    }
}

/// Publish and subscribe. Implemented in-process in v0.1 and over `JetStream` in v0.2.
///
/// # The contract a conformance-passing implementation must honour
///
/// | property | required behaviour |
/// |---|---|
/// | delivery | every subscriber whose pattern matches receives the message |
/// | queue groups | subscribers sharing a group name receive it **once between them** |
/// | isolation | a pattern built for one tenant never receives another's |
/// | ordering | messages on one subject arrive in publish order |
/// | backpressure | when a consumer is behind, `publish` **blocks**; it never drops |
/// | history | a subscriber receives what is published *after* it subscribes |
/// | no subscribers | `publish` succeeds and the message is discarded |
///
/// Backpressure is the one that people ask about. A full channel blocking the collector
/// is correct: the alternative is dropping telemetry during exactly the incident it was
/// collected for. See [`crate::conformance`].
#[async_trait]
pub trait TelemetryBus: Send + Sync {
    /// Publish one envelope. Blocks while every matching subscriber is at capacity.
    async fn publish(&self, subject: &Subject, envelope: TelemetryEnvelope) -> Result<()>;

    /// Subscribe. `group` makes this one member of a queue group, which is how the
    /// pipeline scales to several workers without processing anything twice.
    async fn subscribe(
        &self,
        pattern: &SubjectPattern,
        group: Option<&str>,
    ) -> Result<BoxStream<'static, Delivery>>;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn envelope() -> TelemetryEnvelope {
        crate::testing::envelope(uops_core::TenantId::new())
    }

    #[tokio::test]
    async fn a_noop_ack_is_indistinguishable_to_the_consumer() {
        // The point of the handle existing under a bus with no redelivery: consumer
        // code is byte-for-byte the same, so it is already correct when the bus changes.
        let d = Delivery::new(envelope(), AckHandle::noop());
        assert_eq!(d.envelope().signal.kind(), "log");
        d.ack();
    }

    #[tokio::test]
    async fn a_reporting_handle_tells_the_bus_what_happened() {
        let (tx, rx) = tokio::sync::oneshot::channel();
        Delivery::new(envelope(), AckHandle::reporting(tx)).nack(Nack::Retry);
        assert_eq!(rx.await.unwrap(), Outcome::Nacked(Nack::Retry));

        let (tx, rx) = tokio::sync::oneshot::channel();
        let _taken = Delivery::new(envelope(), AckHandle::reporting(tx)).take();
        assert_eq!(rx.await.unwrap(), Outcome::Acked);
    }

    #[tokio::test]
    async fn dropping_a_delivery_answers_nothing() {
        // Silence, not an acknowledgement. Under JetStream that is what produces a
        // redelivery after the ack wait expires, and a handle that acked on drop would
        // silently convert every consumer panic into a lost message.
        let (tx, rx) = tokio::sync::oneshot::channel();
        drop(Delivery::new(envelope(), AckHandle::reporting(tx)));
        assert!(
            rx.await.is_err(),
            "a dropped delivery must not report success"
        );
    }
}
