//! Reserve, deliver, record.
//!
//! The order is the whole design and is argued for in `uops_store_pg::notify`: the row is
//! written *before* the delivery, so a process that dies mid-send leaves a trace of having
//! tried rather than sending the same page again on the next evaluation.
//!
//! # A rule with no channels is not an error
//!
//! It is a rule being tuned. The state machine still runs, the alert still appears in the
//! list, and nobody is woken up — which is exactly what somebody writing a new rule wants
//! for the first day of its life. SPEC's default for `notify` is an empty list for this
//! reason.

use uops_core::alert::Phase;
use uops_core::{ResourceId, TenantScope};
use uops_store_pg::{Attempt, Outcome, PgStore};

use crate::notification::Notification;
use crate::smtp::Smtp;
use crate::webhook::Webhook;

/// What happened to one notification on one channel.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Delivered {
    pub channel: uuid::Uuid,
    pub outcome: Outcome,
    /// Empty when it was delivered.
    pub detail: String,
}

/// Delivers alerts to a tenant's channels.
#[derive(Clone, Debug)]
pub struct Notifier {
    pg: PgStore,
}

impl Notifier {
    #[must_use]
    pub const fn new(pg: PgStore) -> Self {
        Self { pg }
    }

    /// Send one alert to every channel a rule names.
    ///
    /// Returns one [`Delivered`] per channel, including the refusals — a caller that only
    /// heard about successes could not tell a quiet estate from a silenced one.
    ///
    /// # Errors
    ///
    /// Only when the channel list on the rule cannot be read as a list of ids. Everything
    /// else — a channel that no longer exists, a limit, a transport failure — is one
    /// entry in the result rather than the end of the send.
    pub async fn deliver(
        &self,
        scope: &TenantScope,
        channels: &serde_json::Value,
        notification: &Notification,
    ) -> uops_core::Result<Vec<Delivered>> {
        let mut out = Vec::new();

        for id in channel_ids(channels) {
            let attempt = Attempt {
                channel_id: id,
                rule_id: notification.rule_id,
                dedup_key: notification.dedup_key.clone(),
                phase: notification.phase.as_str().to_owned(),
            };

            // A channel that was deleted, or disabled, after the rule was written. There
            // is no row to record this against — the table's foreign key says a delivery
            // belongs to a channel — so it is reported to the caller, which is what puts
            // it in the log beside the alert it was about.
            let reservation = match self.pg.reserve_notification(scope, &attempt).await {
                Ok(reservation) => reservation,
                Err(uops_core::Error::NotFound { .. }) => {
                    out.push(Delivered {
                        channel: id,
                        outcome: Outcome::Failed,
                        detail: "no enabled channel with this id: it was deleted or turned \
                                 off after the rule was written"
                            .to_owned(),
                    });
                    continue;
                }
                Err(e) => return Err(e),
            };

            if !reservation.allowed() {
                out.push(Delivered {
                    channel: id,
                    outcome: reservation.outcome,
                    detail: String::new(),
                });
                continue;
            }

            let channel = self.pg.channel(scope, id).await?;
            let sent = match channel.kind.as_str() {
                "webhook" => match Webhook::from_config(&channel.config) {
                    Ok(hook) => hook.deliver(notification).await,
                    Err(e) => Err(e),
                },
                "email" => match Smtp::from_config(&channel.config) {
                    Ok(mail) => mail.deliver(notification).await,
                    Err(e) => Err(e),
                },
                other => Err(format!("unknown channel kind {other}")),
            };

            match sent {
                Ok(()) => out.push(Delivered {
                    channel: id,
                    outcome: Outcome::Sent,
                    detail: String::new(),
                }),
                Err(detail) => {
                    self.pg
                        .notification_failed(scope, reservation.id, &detail)
                        .await?;
                    out.push(Delivered {
                        channel: id,
                        outcome: Outcome::Failed,
                        detail,
                    });
                }
            }
        }

        Ok(out)
    }

    /// The name a person calls a resource, or its id when it has gone.
    ///
    /// One indexed lookup per notification. Notifications are rare by construction — the
    /// rate limit sees to that — and an alert about a uuid is an alert nobody can act on.
    pub async fn resource_name(&self, scope: &TenantScope, id: ResourceId) -> String {
        self.pg
            .resource(scope, id)
            .await
            .map_or_else(|_| id.to_string(), |r| r.name)
    }
}

/// The channel ids on a rule's `notify`.
///
/// `["<uuid>", …]`. Anything that is not a uuid is skipped rather than failing the send:
/// one malformed entry must not stop the channels beside it from being told, and the
/// entry was validated when the rule was written.
fn channel_ids(value: &serde_json::Value) -> Vec<uuid::Uuid> {
    value
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str()?.parse().ok())
                .collect()
        })
        .unwrap_or_default()
}

/// Whether a transition is one somebody is told about, for a caller that has a phase and
/// not a [`uops_core::alert::Transition`].
///
/// Exactly the two entries `firing` and `resolved`, which is what [`step`] already says —
/// this is here so that a caller reconstructing a decision from stored state cannot
/// accidentally notify on `pending`.
#[must_use]
pub fn is_worth_telling(phase: Phase) -> bool {
    matches!(phase, Phase::Firing | Phase::Resolved)
}

/// A sanity check that the two halves agree, so this file's idea of "worth telling"
/// cannot drift from the state machine's.
#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use uops_core::alert::step;

    #[test]
    fn what_is_worth_telling_is_what_the_machine_notifies_on() {
        let now = Utc::now();
        let hold = chrono::Duration::zero();

        // Entering firing, and entering resolved, are the two the machine notifies on.
        let fired = step(None, true, hold, now);
        assert!(fired.notify && is_worth_telling(fired.phase));

        let resolved = step(Some((Phase::Firing, now)), false, hold, now);
        assert!(resolved.notify && is_worth_telling(resolved.phase));

        // And pending is not, however it was reached.
        assert!(!is_worth_telling(Phase::Pending));
        assert!(!is_worth_telling(Phase::Ok));
    }

    #[test]
    fn a_rule_with_no_channels_sends_nothing_and_says_nothing() {
        assert!(channel_ids(&serde_json::json!([])).is_empty());
        assert!(channel_ids(&serde_json::json!(null)).is_empty());
    }

    #[test]
    fn one_malformed_channel_reference_does_not_take_the_others_with_it() {
        let ids = channel_ids(&serde_json::json!([
            "018f0000-0000-7000-8000-0000000000aa",
            "not a uuid",
            42,
            "018f0000-0000-7000-8000-0000000000bb"
        ]));
        assert_eq!(ids.len(), 2);
    }
}
