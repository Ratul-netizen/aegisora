//! `InProcessBus` — v0.1. A bounded channel per subscriber.
//!
//! Everything interesting about it is a consequence of one decision: **the channel
//! bound is the backpressure**, and a full channel blocks the publisher. That is the
//! correct behaviour for a monitoring platform. Dropping telemetry is silent, and it is
//! silent precisely during the incident the telemetry was collected for — a device
//! storm is exactly when the queue fills and exactly when the messages matter most.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use futures_core::stream::BoxStream;
use tokio::sync::mpsc;
use uops_core::TelemetryEnvelope;

use crate::bus::{AckHandle, Delivery, TelemetryBus};
use crate::error::{Error, Result};
use crate::subject::{Subject, SubjectPattern};

/// How many messages may be in flight to one subscriber before publishing blocks.
///
/// Sized to absorb a burst without hiding a stall: a consumer that is merely jittery
/// drains it, a consumer that is actually stuck fills it and the pressure reaches the
/// collector, where it is visible.
pub const DEFAULT_CAPACITY: usize = 1_024;

struct Subscription {
    pattern: SubjectPattern,
    group: Option<String>,
    sender: mpsc::Sender<Delivery>,
}

#[derive(Default)]
struct Registry {
    subscriptions: Vec<Subscription>,
    /// Round-robin cursor per queue group, so work spreads rather than piling onto
    /// whichever member happens to be first in the list.
    cursors: std::collections::HashMap<String, usize>,
}

/// The in-process bus.
///
/// Cheap to clone; clones share one registry, so a component can hold its own handle
/// without the wiring having to thread a reference through every constructor.
#[derive(Clone)]
pub struct InProcessBus {
    registry: Arc<Mutex<Registry>>,
    capacity: usize,
}

impl std::fmt::Debug for InProcessBus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let count = self
            .registry
            .lock()
            .map_or(0, |r: std::sync::MutexGuard<'_, Registry>| {
                r.subscriptions.len()
            });
        f.debug_struct("InProcessBus")
            .field("subscribers", &count)
            .field("capacity", &self.capacity)
            .finish()
    }
}

impl Default for InProcessBus {
    fn default() -> Self {
        Self::new(DEFAULT_CAPACITY)
    }
}

impl InProcessBus {
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        Self {
            registry: Arc::new(Mutex::new(Registry::default())),
            capacity: capacity.max(1),
        }
    }

    /// How many live subscriptions there are. For health output and for tests.
    #[must_use]
    pub fn subscriber_count(&self) -> usize {
        self.lock().subscriptions.len()
    }

    /// A poisoned registry means another thread panicked while holding it. The contents
    /// are a `Vec` of channel senders — there is no invariant a panic could have left
    /// half-applied — so recovering is strictly better than poisoning the whole bus and
    /// taking telemetry collection down with it. Same reasoning as the access log in
    /// `uops-secrets`.
    fn lock(&self) -> std::sync::MutexGuard<'_, Registry> {
        self.registry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Choose who receives this subject: every ungrouped matching subscriber, plus
    /// exactly one member of each matching queue group.
    fn recipients(&self, subject: &Subject) -> Vec<mpsc::Sender<Delivery>> {
        let mut registry = self.lock();

        // Collected first because picking a group member mutates the cursor, and the
        // borrow checker is right to object to doing that while iterating.
        let matching: Vec<(usize, Option<String>)> = registry
            .subscriptions
            .iter()
            .enumerate()
            .filter(|(_, s)| s.pattern.matches(subject))
            .map(|(i, s)| (i, s.group.clone()))
            .collect();

        let mut chosen: Vec<usize> = Vec::new();
        let mut groups: Vec<(String, Vec<usize>)> = Vec::new();

        for (index, group) in matching {
            match group {
                None => chosen.push(index),
                Some(name) => match groups.iter_mut().find(|(g, _)| *g == name) {
                    Some((_, members)) => members.push(index),
                    None => groups.push((name, vec![index])),
                },
            }
        }

        for (name, members) in groups {
            let cursor = registry.cursors.entry(name).or_insert(0);
            let pick = *cursor % members.len();
            *cursor = cursor.wrapping_add(1);
            chosen.push(members[pick]);
        }

        chosen
            .into_iter()
            .filter_map(|i| registry.subscriptions.get(i).map(|s| s.sender.clone()))
            .collect()
    }

    /// Forget subscribers whose receiver has been dropped.
    fn prune(&self) {
        self.lock().subscriptions.retain(|s| !s.sender.is_closed());
    }
}

#[async_trait]
impl TelemetryBus for InProcessBus {
    async fn publish(&self, subject: &Subject, envelope: TelemetryEnvelope) -> Result<()> {
        // The senders are cloned out and the registry lock released BEFORE any await.
        // Holding a std::sync::Mutex across an await would park the whole runtime behind
        // a slow consumer — the one deadlock this design could plausibly ship with.
        let senders = self.recipients(subject);
        if senders.is_empty() {
            // Matches NATS core, and JetStream with no consumer bound: a publish with
            // nobody listening succeeds and the message is gone. Telemetry published
            // during startup, before the pipeline subscribes, is not an error.
            return Ok(());
        }

        let mut closed = false;
        for sender in senders {
            let delivery = Delivery::new(envelope.clone(), AckHandle::noop());
            // `send` awaits when the channel is full. THIS is the backpressure: the
            // collector waits rather than dropping. Do not replace it with try_send.
            if sender.send(delivery).await.is_err() {
                // The subscriber went away between choosing it and sending. Not an
                // error for the publisher; it just needs cleaning up.
                closed = true;
            }
        }

        if closed {
            self.prune();
        }
        Ok(())
    }

    async fn subscribe(
        &self,
        pattern: &SubjectPattern,
        group: Option<&str>,
    ) -> Result<BoxStream<'static, Delivery>> {
        if let Some(name) = group
            && name.trim().is_empty()
        {
            return Err(Error::InvalidSubject(
                "a queue group name cannot be blank; pass None for a plain subscription".into(),
            ));
        }

        let (tx, rx) = mpsc::channel(self.capacity);
        self.lock().subscriptions.push(Subscription {
            pattern: pattern.clone(),
            group: group.map(str::to_owned),
            sender: tx,
        });

        Ok(Box::pin(ReceiverStream(rx)))
    }
}

/// `mpsc::Receiver` as a `Stream`.
///
/// Hand-written rather than pulling in `tokio-stream` for one adapter: the whole
/// implementation is the `poll_recv` line below, and the dependency would appear in
/// every SBOM this project ships.
struct ReceiverStream(mpsc::Receiver<Delivery>);

impl futures_core::Stream for ReceiverStream {
    type Item = Delivery;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        self.0.poll_recv(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::subject::SignalKind;
    use crate::testing::envelope;
    use futures_util::StreamExt;
    use uops_core::{SourceKind, TenantId};

    #[tokio::test]
    async fn a_subscriber_that_goes_away_is_forgotten() {
        // Otherwise every reconnecting consumer leaks a sender, and eventually every
        // publish blocks on a channel nobody is reading.
        let bus = InProcessBus::new(4);
        let tenant = TenantId::new();
        let subject = Subject::new(tenant, SignalKind::Log, SourceKind::Syslog);

        let stream = bus
            .subscribe(&SubjectPattern::tenant(tenant), None)
            .await
            .unwrap();
        assert_eq!(bus.subscriber_count(), 1);

        drop(stream);
        bus.publish(&subject, envelope(tenant)).await.unwrap();
        assert_eq!(
            bus.subscriber_count(),
            0,
            "a dead subscriber must be pruned"
        );
    }

    #[tokio::test]
    async fn clones_share_one_registry() {
        // The wiring depends on this: a collector holds a clone, the pipeline holds a
        // clone, and they are the same bus.
        let bus = InProcessBus::new(4);
        let tenant = TenantId::new();
        let mut stream = bus
            .subscribe(&SubjectPattern::tenant(tenant), None)
            .await
            .unwrap();

        let publisher = bus.clone();
        publisher
            .publish(
                &Subject::new(tenant, SignalKind::Log, SourceKind::Syslog),
                envelope(tenant),
            )
            .await
            .unwrap();

        assert!(stream.next().await.is_some());
    }

    #[tokio::test]
    async fn a_blank_queue_group_is_rejected() {
        // "" would otherwise be a group of its own, which looks like a plain
        // subscription until a second worker joins and starts stealing messages.
        let bus = InProcessBus::default();
        // Matched rather than unwrap_err()'d: a BoxStream is not Debug, so the Result's
        // own helpers are unavailable here.
        match bus
            .subscribe(&SubjectPattern::everything(), Some("  "))
            .await
        {
            Ok(_) => panic!("a blank queue group name must be rejected"),
            Err(e) => assert!(matches!(e, Error::InvalidSubject(_)), "{e}"),
        }
    }

    #[tokio::test]
    async fn queue_group_members_take_turns() {
        // Round-robin rather than "whoever is first in the vec". With a fixed pick, a
        // second worker would join, receive nothing, and look healthy.
        let bus = InProcessBus::new(8);
        let tenant = TenantId::new();
        let subject = Subject::new(tenant, SignalKind::Log, SourceKind::Syslog);

        let mut a = bus
            .subscribe(&SubjectPattern::tenant(tenant), Some("writers"))
            .await
            .unwrap();
        let mut b = bus
            .subscribe(&SubjectPattern::tenant(tenant), Some("writers"))
            .await
            .unwrap();

        for _ in 0..4 {
            bus.publish(&subject, envelope(tenant)).await.unwrap();
        }

        let mut got_a = 0;
        let mut got_b = 0;
        for _ in 0..2 {
            if a.next().await.is_some() {
                got_a += 1;
            }
            if b.next().await.is_some() {
                got_b += 1;
            }
        }
        assert_eq!((got_a, got_b), (2, 2), "four messages, two workers, evenly");
    }
}
