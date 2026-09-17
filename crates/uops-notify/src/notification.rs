//! What an alert says when it reaches a person.
//!
//! Rendered once and handed to every channel, rather than each transport building its
//! own. Two channels describing the same alert differently is how an operator ends up
//! believing there are two problems.
//!
//! # Why the resource's name is fetched
//!
//! An alert about `018f0000-0000-7000-8000-0000000000aa` is an alert nobody can act on.
//! The name costs one indexed lookup per notification, and notifications are rare by
//! construction — the rate limit sees to that. A dedup key carries the id for the machine
//! reading the webhook; the sentence carries the name for the person reading the page.

use chrono::{DateTime, Utc};
use serde::Serialize;
use uops_core::ResourceId;
use uops_core::alert::{AlertSeverity, Phase};

/// One alert, ready to be delivered.
#[derive(Clone, Debug, Serialize)]
pub struct Notification {
    /// `firing` or `resolved`. The only two phases anybody is told about.
    pub phase: Phase,
    pub severity: AlertSeverity,
    pub rule: String,
    pub rule_id: uuid::Uuid,
    pub resource_id: ResourceId,
    /// What a person calls the device. Falls back to the id when the resource has been
    /// deleted between the evaluation and the delivery, which is rare and is still better
    /// than an empty string where a hostname should be.
    pub resource: String,
    pub dedup_key: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<f64>,
    /// When the alert entered its current phase — not when this was sent. An operator
    /// arriving at a screen needs to know the problem started eleven minutes ago, and a
    /// notification that was delayed by a rate limit would otherwise claim it started
    /// just now.
    pub since: DateTime<Utc>,
    pub at: DateTime<Utc>,
}

impl Notification {
    /// The one-line summary. A subject line, a chat message, the first thing read.
    ///
    /// Severity leads because it is what decides whether to get out of bed, and the
    /// resource comes before the rule because an operator recognises their own devices
    /// faster than the names somebody gave the rules.
    #[must_use]
    pub fn summary(&self) -> String {
        let verb = match self.phase {
            Phase::Resolved => "resolved",
            _ => "firing",
        };
        match self.value {
            Some(v) => format!(
                "[{}] {} — {} {} ({:.3})",
                self.severity.as_str(),
                self.resource,
                self.rule,
                verb,
                v
            ),
            None => format!(
                "[{}] {} — {} {}",
                self.severity.as_str(),
                self.resource,
                self.rule,
                verb
            ),
        }
    }

    /// The body a person reads, in plain text.
    ///
    /// Deliberately not HTML and deliberately short. This is read on a phone at 4am, and
    /// the four facts that matter are what, where, since when, and how bad.
    #[must_use]
    pub fn text(&self) -> String {
        let mut lines = vec![
            self.summary(),
            String::new(),
            format!("Rule:     {}", self.rule),
            format!("Resource: {} ({})", self.resource, self.resource_id),
            format!("State:    {}", self.phase.as_str()),
            format!("Since:    {}", self.since.format("%Y-%m-%d %H:%M:%S UTC")),
        ];
        if let Some(value) = self.value {
            lines.push(format!("Value:    {value:.3}"));
        }
        lines.push(format!("Alert:    {}", self.dedup_key));
        lines.join("\n")
    }

    /// What a webhook receives.
    ///
    /// The struct itself, plus the rendered sentence — because the thing on the other end
    /// is as likely to be a chat bridge that wants a line of text as a system that wants
    /// the fields.
    #[must_use]
    pub fn payload(&self) -> serde_json::Value {
        let mut value = serde_json::to_value(self).unwrap_or_else(|_| serde_json::json!({}));
        if let Some(object) = value.as_object_mut() {
            object.insert("summary".to_owned(), serde_json::json!(self.summary()));
            object.insert("text".to_owned(), serde_json::json!(self.text()));
        }
        value
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn notification(phase: Phase, value: Option<f64>) -> Notification {
        Notification {
            phase,
            severity: AlertSeverity::Critical,
            rule: "CPU hot".to_owned(),
            rule_id: uuid::Uuid::nil(),
            resource_id: ResourceId::nil(),
            resource: "rtr-01".to_owned(),
            dedup_key: "rule/rtr-01".to_owned(),
            value,
            since: DateTime::from_timestamp(1_700_000_000, 0).expect("an instant"),
            at: DateTime::from_timestamp(1_700_000_600, 0).expect("an instant"),
        }
    }

    #[test]
    fn the_summary_leads_with_what_decides_whether_to_get_up() {
        let firing = notification(Phase::Firing, Some(94.5));
        assert_eq!(
            firing.summary(),
            "[critical] rtr-01 — CPU hot firing (94.500)"
        );

        // The same alert ending says so in the same shape, so a person reading a list of
        // them can pair the two up at a glance.
        let resolved = notification(Phase::Resolved, Some(11.0));
        assert!(
            resolved.summary().contains("resolved"),
            "{}",
            resolved.summary()
        );
    }

    #[test]
    fn the_body_says_when_the_problem_started_not_when_this_was_sent() {
        // A notification delayed by a rate limit would otherwise claim the problem began
        // at the moment it happened to get through.
        let text = notification(Phase::Firing, Some(94.5)).text();
        assert!(text.contains("2023-11-14"), "{text}");
        assert!(text.contains("rtr-01"), "{text}");
        assert!(text.contains("Value:    94.500"), "{text}");
    }

    #[test]
    fn an_absence_alert_has_no_value_and_does_not_print_one() {
        let text = notification(Phase::Firing, None).text();
        assert!(!text.contains("Value:"), "{text}");
        assert!(!notification(Phase::Firing, None).summary().contains('('));
    }

    #[test]
    fn the_payload_carries_both_the_fields_and_the_sentence() {
        // The thing on the other end is as likely to be a chat bridge that wants a line
        // of text as a system that wants the fields.
        let payload = notification(Phase::Firing, Some(94.5)).payload();
        assert_eq!(payload["rule"], "CPU hot");
        assert_eq!(payload["phase"], "firing");
        assert_eq!(payload["severity"], "critical");
        assert!(
            payload["summary"]
                .as_str()
                .unwrap_or_default()
                .contains("rtr-01")
        );
        assert!(
            payload["text"]
                .as_str()
                .unwrap_or_default()
                .contains("Since:")
        );
    }
}
