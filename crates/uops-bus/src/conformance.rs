//! The conformance suite — SPEC M0 acceptance: *written once, run against both impls.*
//!
//! This module is the executable form of the contract in [`crate::TelemetryBus`]. It
//! exists because the migration to NATS `JetStream` in v0.2 is only a wiring change if
//! both implementations behave identically, and "behave identically" is not something
//! prose can hold two implementations to a year apart.
//!
//! Shipped in the library rather than in `tests/`, so that a future `uops-bus-nats`
//! crate runs **these same functions** rather than a copy that has drifted.
//!
//! # Use
//!
//! ```
//! use uops_bus::{InProcessBus, conformance::BusFactory, conformance_suite};
//!
//! struct InProcess;
//!
//! #[async_trait::async_trait]
//! impl BusFactory for InProcess {
//!     type Bus = InProcessBus;
//!     async fn create(&self, capacity: usize) -> Self::Bus {
//!         InProcessBus::new(capacity)
//!     }
//! }
//!
//! conformance_suite!(InProcess);
//! ```

use std::time::Duration;

use async_trait::async_trait;
use futures_core::stream::BoxStream;
use futures_util::StreamExt;
use uops_core::{SourceKind, TenantId};

use crate::bus::{Delivery, TelemetryBus};
use crate::subject::{SignalKind, Subject, SubjectPattern};
use crate::testing::{body_of, envelope, envelope_with_body};

/// How the suite builds the implementation under test.
///
/// `capacity` is a parameter because backpressure cannot be tested against a queue whose
/// depth the test does not know.
#[async_trait]
pub trait BusFactory: Send + Sync {
    type Bus: TelemetryBus + Send + Sync + 'static;

    async fn create(&self, capacity: usize) -> Self::Bus;
}

/// Long enough that a slow CI runner is not mistaken for a blocked publisher.
const SETTLE: Duration = Duration::from_millis(250);
/// Short enough that asserting "this did NOT happen" stays quick.
const NEGATIVE: Duration = Duration::from_millis(150);

async fn next(stream: &mut BoxStream<'static, Delivery>) -> Option<Delivery> {
    tokio::time::timeout(SETTLE, stream.next()).await.ok()?
}

async fn nothing_arrives(stream: &mut BoxStream<'static, Delivery>) -> bool {
    tokio::time::timeout(NEGATIVE, stream.next()).await.is_err()
}

fn log_subject(tenant: TenantId) -> Subject {
    Subject::new(tenant, SignalKind::Log, SourceKind::Syslog)
}

/// Every subscriber whose pattern matches receives the message.
pub async fn fans_out_to_every_matching_subscriber<F: BusFactory>(factory: &F) {
    let bus = factory.create(8).await;
    let tenant = TenantId::new();

    let mut wide = bus
        .subscribe(&SubjectPattern::tenant(tenant), None)
        .await
        .unwrap();
    let mut narrow = bus
        .subscribe(&SubjectPattern::signal(tenant, SignalKind::Log), None)
        .await
        .unwrap();

    bus.publish(&log_subject(tenant), envelope(tenant))
        .await
        .unwrap();

    assert!(next(&mut wide).await.is_some(), "the broad pattern matched");
    assert!(
        next(&mut narrow).await.is_some(),
        "two subscribers, two copies — a match is not consumed by the first taker"
    );
}

/// A subscriber built for one tenant never receives another tenant's telemetry.
///
/// The most important assertion in the suite. An MSP runs every customer's pipeline
/// against one bus, and a subject-matching bug here is a cross-customer data leak that
/// no amount of `TenantScope` in the query layer can catch — the data has already
/// arrived in the wrong process.
pub async fn never_crosses_tenants<F: BusFactory>(factory: &F) {
    let bus = factory.create(8).await;
    let mine = TenantId::new();
    let theirs = TenantId::new();

    let mut mine_stream = bus
        .subscribe(&SubjectPattern::tenant(mine), None)
        .await
        .unwrap();

    bus.publish(&log_subject(theirs), envelope(theirs))
        .await
        .unwrap();
    assert!(
        nothing_arrives(&mut mine_stream).await,
        "another tenant's message reached this subscriber"
    );

    // And the subscription is not simply broken.
    bus.publish(&log_subject(mine), envelope(mine))
        .await
        .unwrap();
    assert!(next(&mut mine_stream).await.is_some());
}

/// A signal pattern does not receive other signals.
pub async fn respects_the_signal_token<F: BusFactory>(factory: &F) {
    let bus = factory.create(8).await;
    let tenant = TenantId::new();

    let mut logs = bus
        .subscribe(&SubjectPattern::signal(tenant, SignalKind::Log), None)
        .await
        .unwrap();

    bus.publish(
        &Subject::new(tenant, SignalKind::Metric, SourceKind::Snmp),
        envelope(tenant),
    )
    .await
    .unwrap();

    assert!(
        nothing_arrives(&mut logs).await,
        "a metric reached a log subscriber"
    );
}

/// Members of a queue group receive a message once between them, not once each.
pub async fn a_queue_group_receives_each_message_once<F: BusFactory>(factory: &F) {
    const SENT: usize = 6;

    let bus = factory.create(16).await;
    let tenant = TenantId::new();

    let mut a = bus
        .subscribe(&SubjectPattern::tenant(tenant), Some("writers"))
        .await
        .unwrap();
    let mut b = bus
        .subscribe(&SubjectPattern::tenant(tenant), Some("writers"))
        .await
        .unwrap();

    for i in 0..SENT {
        bus.publish(
            &log_subject(tenant),
            envelope_with_body(tenant, &format!("message {i}")),
        )
        .await
        .unwrap();
    }

    let mut seen = Vec::new();
    while seen.len() < SENT {
        let mut progressed = false;
        if let Some(d) = next(&mut a).await {
            seen.push(body_of(d.envelope()));
            progressed = true;
        }
        if let Some(d) = next(&mut b).await {
            seen.push(body_of(d.envelope()));
            progressed = true;
        }
        assert!(progressed, "the group stopped receiving at {}", seen.len());
    }

    seen.sort();
    seen.dedup();
    assert_eq!(
        seen.len(),
        SENT,
        "a queue group must not process the same message twice: {seen:?}"
    );
}

/// An ungrouped subscriber still receives everything, even alongside a queue group.
pub async fn a_plain_subscriber_is_unaffected_by_a_queue_group<F: BusFactory>(factory: &F) {
    let bus = factory.create(16).await;
    let tenant = TenantId::new();

    let mut group = bus
        .subscribe(&SubjectPattern::tenant(tenant), Some("writers"))
        .await
        .unwrap();
    let mut watcher = bus
        .subscribe(&SubjectPattern::tenant(tenant), None)
        .await
        .unwrap();

    bus.publish(&log_subject(tenant), envelope(tenant))
        .await
        .unwrap();

    assert!(next(&mut group).await.is_some());
    assert!(
        next(&mut watcher).await.is_some(),
        "a queue group must not steal from unrelated subscribers"
    );
}

/// Messages on one subject arrive in the order they were published.
pub async fn preserves_order_within_a_subject<F: BusFactory>(factory: &F) {
    let bus = factory.create(32).await;
    let tenant = TenantId::new();

    let mut stream = bus
        .subscribe(&SubjectPattern::tenant(tenant), None)
        .await
        .unwrap();

    let expected: Vec<String> = (0..10).map(|i| format!("message {i}")).collect();
    for body in &expected {
        bus.publish(&log_subject(tenant), envelope_with_body(tenant, body))
            .await
            .unwrap();
    }

    let mut got = Vec::new();
    for _ in 0..expected.len() {
        let d = next(&mut stream).await.expect("message went missing");
        got.push(body_of(d.envelope()));
    }
    assert_eq!(
        got, expected,
        "ordering within a subject is part of the contract"
    );
}

/// Publishing with nobody listening succeeds and discards the message.
pub async fn publishing_into_the_void_is_not_an_error<F: BusFactory>(factory: &F) {
    let bus = factory.create(4).await;
    let tenant = TenantId::new();

    // Telemetry arriving before the pipeline has subscribed is normal at startup, and
    // failing the collector for it would turn a race into an outage.
    bus.publish(&log_subject(tenant), envelope(tenant))
        .await
        .unwrap();
}

/// A subscriber receives what is published after it subscribes, not before.
pub async fn there_is_no_replay<F: BusFactory>(factory: &F) {
    let bus = factory.create(8).await;
    let tenant = TenantId::new();

    bus.publish(
        &log_subject(tenant),
        envelope_with_body(tenant, "published before anyone subscribed"),
    )
    .await
    .unwrap();

    let mut stream = bus
        .subscribe(&SubjectPattern::tenant(tenant), None)
        .await
        .unwrap();

    // Stated explicitly so the NATS implementation configures a new-messages-only
    // consumer rather than inheriting JetStream's replay-from-start default, which
    // would flood a restarting pipeline with the whole retained stream.
    assert!(
        nothing_arrives(&mut stream).await,
        "a new subscriber must not be sent history"
    );
}

/// When a consumer stops draining, `publish` blocks instead of dropping.
///
/// The behaviour PLAN §0 change #4 chose deliberately: backpressure reaches the
/// collector, where it is visible, rather than being absorbed by silently discarding
/// telemetry during exactly the incident it was collected for.
pub async fn backpressure_blocks_rather_than_drops<F: BusFactory>(factory: &F) {
    let bus = factory.create(1).await;
    let tenant = TenantId::new();

    let mut stream = bus
        .subscribe(&SubjectPattern::tenant(tenant), None)
        .await
        .unwrap();

    // Fills the single slot.
    bus.publish(&log_subject(tenant), envelope_with_body(tenant, "first"))
        .await
        .unwrap();

    let blocked = tokio::time::timeout(
        NEGATIVE,
        bus.publish(&log_subject(tenant), envelope_with_body(tenant, "second")),
    )
    .await;
    assert!(
        blocked.is_err(),
        "publish returned while the consumer was behind — the message was dropped or \
         the queue is unbounded"
    );

    // Once the consumer catches up, publishing proceeds.
    let first = next(&mut stream)
        .await
        .expect("the first message is queued");
    assert_eq!(body_of(first.envelope()), "first");

    tokio::time::timeout(
        SETTLE,
        bus.publish(&log_subject(tenant), envelope_with_body(tenant, "third")),
    )
    .await
    .expect("publish must resume once there is room")
    .unwrap();
}

/// Acknowledging returns the envelope intact.
pub async fn acknowledging_does_not_disturb_the_envelope<F: BusFactory>(factory: &F) {
    let bus = factory.create(4).await;
    let tenant = TenantId::new();

    let mut stream = bus
        .subscribe(&SubjectPattern::tenant(tenant), None)
        .await
        .unwrap();
    bus.publish(
        &log_subject(tenant),
        envelope_with_body(tenant, "the payload"),
    )
    .await
    .unwrap();

    let delivery = next(&mut stream).await.expect("delivered");
    let envelope = delivery.take();
    assert_eq!(body_of(&envelope), "the payload");
    assert_eq!(envelope.tenant_id, tenant);
    assert!(
        envelope.is_unresolved(),
        "the bus must not modify the envelope it carries"
    );
}

/// Dropping the stream is how a subscriber leaves; publishing afterwards still works.
pub async fn unsubscribing_does_not_break_the_bus<F: BusFactory>(factory: &F) {
    let bus = factory.create(4).await;
    let tenant = TenantId::new();

    let gone = bus
        .subscribe(&SubjectPattern::tenant(tenant), None)
        .await
        .unwrap();
    let mut staying = bus
        .subscribe(&SubjectPattern::tenant(tenant), None)
        .await
        .unwrap();
    drop(gone);

    bus.publish(&log_subject(tenant), envelope(tenant))
        .await
        .unwrap();
    assert!(
        next(&mut staying).await.is_some(),
        "one subscriber leaving must not disturb another"
    );
}

/// Generate the whole suite as named `#[tokio::test]` functions.
///
/// Named rather than one blob so a failure says *which property* broke — the difference
/// between "the bus is wrong" and "queue groups deliver twice".
#[macro_export]
macro_rules! conformance_suite {
    ($factory:expr) => {
        $crate::conformance_suite!(
            $factory;
            fans_out_to_every_matching_subscriber,
            never_crosses_tenants,
            respects_the_signal_token,
            a_queue_group_receives_each_message_once,
            a_plain_subscriber_is_unaffected_by_a_queue_group,
            preserves_order_within_a_subject,
            publishing_into_the_void_is_not_an_error,
            there_is_no_replay,
            backpressure_blocks_rather_than_drops,
            acknowledging_does_not_disturb_the_envelope,
            unsubscribing_does_not_break_the_bus,
        );
    };
    ($factory:expr; $($case:ident),+ $(,)?) => {
        $(
            #[tokio::test]
            async fn $case() {
                $crate::conformance::$case(&$factory).await;
            }
        )+
    };
}
