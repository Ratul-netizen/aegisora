//! The telemetry bus — SPEC §M0.7.
//!
//! PLAN §0 change #4: **keep the boundary, defer the daemon.** Ten to a hundred devices
//! on one box does not need a durable broker, and running one would be another thing to
//! install, monitor and back up on a customer's hardware. But a bus introduced later is
//! a rewrite of every producer and consumer, so the trait ships now and the daemon does
//! not.
//!
//! | version | implementation | acks |
//! |---|---|---|
//! | v0.1 | [`InProcessBus`] — a bounded `tokio` channel per subscriber | no-op |
//! | v0.2 | `NatsBus` — `JetStream`, same trait | real, with redelivery |
//!
//! # What makes the second one a wiring change
//!
//! Three decisions, all of which cost nothing now and cannot be retrofitted:
//!
//! 1. **Subjects are hierarchical and match NATS rules exactly** —
//!    `telemetry.{tenant}.{signal}.{source_kind}`, with `*` and `>` behaving as they do
//!    on a real server. A simpler scheme would change *which messages each subscriber
//!    receives* when the transport changes, which is not a wiring change.
//! 2. **`ack` exists from the first commit**, doing nothing. A consumer that
//!    acknowledges after its durable write is already correct under `JetStream`; one that
//!    never learned to is a redesign.
//! 3. **[`conformance`] is a shipped, public test suite**, not a file under `tests/`.
//!    Both implementations run the same functions, a year apart.
//!
//! # Backpressure
//!
//! A full channel blocks the publisher. That is the design, not a limitation: the
//! alternative is dropping telemetry silently, and the queue fills during exactly the
//! device storm the telemetry was collected for.
//!
//! # Example
//!
//! ```
//! use futures_util::StreamExt;
//! use uops_bus::{InProcessBus, SignalKind, Subject, SubjectPattern, TelemetryBus};
//! use uops_core::{SourceKind, TenantId};
//!
//! # #[tokio::main(flavor = "current_thread")]
//! # async fn main() {
//! let bus = InProcessBus::default();
//! let tenant = TenantId::new();
//!
//! let mut stream = bus.subscribe(&SubjectPattern::tenant(tenant), None).await.unwrap();
//!
//! let envelope = uops_bus::testing::envelope(tenant);
//! bus.publish(&Subject::of(&envelope), envelope).await.unwrap();
//!
//! let delivery = stream.next().await.unwrap();
//! assert_eq!(delivery.envelope().tenant_id, tenant);
//! delivery.ack();
//! # }
//! ```

pub mod bus;
pub mod conformance;
pub mod error;
pub mod inprocess;
pub mod subject;
pub mod testing;

pub use bus::{AckHandle, Delivery, Nack, Outcome, TelemetryBus};
pub use error::{Error, Result};
pub use inprocess::{DEFAULT_CAPACITY, InProcessBus};
pub use subject::{SignalKind, Subject, SubjectPattern};
