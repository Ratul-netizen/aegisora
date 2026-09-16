//! A syslog message, as a row of the `logs` table.
//!
//! The narrow waist of the pipeline: everything upstream is syslog-shaped and everything
//! downstream is signal-shaped, and this is where it changes. An OTLP receiver will do
//! the same job from a different input, which is why the output type is
//! `uops_store_ch::LogRow` and not something syslog-specific.
//!
//! # Semantic conventions, not syslog names
//!
//! `app_name` becomes `service.name`, `hostname` becomes `host.name`, `proc_id` becomes
//! `process.pid`. Those are `OpenTelemetry` semconv keys, and they are the same keys the
//! metrics path already writes and the Query AST already knows about.
//!
//! This is the point of the whole product. A log from a switch and a metric from the same
//! switch are only correlatable if they agree what the host is called — and the moment
//! one of them stores `syslog.hostname` instead, the join stops existing and nobody
//! notices, because both tables still look fine on their own.
//!
//! The syslog-specific fields that have no semconv equivalent keep a `syslog.` prefix.
//! They are worth keeping — `syslog.facility` is how an operator finds the firewall's
//! logs on local4 — and they are namespaced so they cannot be mistaken for a convention
//! that exists.
//!
//! # Which clock wins
//!
//! `observed_at` is the device's timestamp when there is one and the receipt time when
//! there is not. `ingested_at` is always the receipt time.
//!
//! A device with no clock, or an unreadable one, would otherwise land at the Unix epoch
//! and sort to the beginning of every search — which is worse than being a few seconds
//! out. When the substitution happens it is recorded in `syslog.timestamp.missing`, so a
//! timeline nobody can trust is at least a timeline somebody can question.

use std::collections::BTreeMap;

use uops_core::{ResourceId, SiteId, TenantId};
use uops_store_ch::LogRow;

use crate::receiver::Received;

/// What the `source_kind` column says about anything from here.
pub const SOURCE_KIND: &str = "syslog";

/// Everything the message itself cannot say.
///
/// Identity resolution decides the first three and enrichment decides the vendor; this
/// module does not guess at any of them, which is why they arrive as an argument rather
/// than being defaulted here.
#[derive(Clone, Copy, Debug)]
pub struct Attribution {
    pub tenant_id: TenantId,
    pub resource_id: ResourceId,
    /// The nil uuid when the resource has no site — `MetricRow` makes the same choice and
    /// for the same reason: the column is not nullable and one answer to "no site" beats
    /// two.
    pub site_id: SiteId,
    /// `cisco`, `mikrotik`, or empty. From the resource's profile, not from the message:
    /// a vendor guessed per-message would disagree with itself across a device's log.
    pub vendor: &'static str,
}

/// Turn one received message into one row.
#[must_use]
pub fn to_row(received: &Received, attribution: Attribution) -> LogRow {
    let message = &received.message;
    let mut attributes = BTreeMap::new();

    // Semantic conventions first. See the module docs on why these names and not syslog's.
    if let Some(host) = &message.hostname {
        attributes.insert("host.name".to_owned(), host.clone());
    }
    if let Some(app) = &message.app_name {
        attributes.insert("service.name".to_owned(), app.clone());
    }
    if let Some(pid) = &message.proc_id {
        attributes.insert("process.pid".to_owned(), pid.clone());
    }

    // Syslog's own vocabulary, namespaced. `facility` is also a real column — it is
    // filtered on often enough to deserve one — and is repeated here so that a caller
    // reading only the attribute map sees a complete picture.
    attributes.insert("syslog.facility".to_owned(), message.facility.to_string());
    if let Some(msg_id) = &message.msg_id {
        attributes.insert("syslog.msgid".to_owned(), msg_id.clone());
    }

    // The sender, which is not the same as the device — a relay forwards other devices'
    // messages under its own address. Kept because it is the one thing certainly true.
    attributes.insert(
        "syslog.source.address".to_owned(),
        received.peer.ip().to_string(),
    );

    // Structured data, already flattened to `sdid.param` by the parser.
    for (key, value) in &message.structured_data {
        attributes.insert(format!("syslog.sd.{key}"), value.clone());
    }

    if let Some(why) = message.parse_error {
        // SPEC names this key. An operator finds every malformed message with one filter,
        // and a vendor can be told exactly what their firmware emits.
        attributes.insert("parse.error".to_owned(), why.to_owned());
    }

    let observed_at = message.timestamp.unwrap_or_else(|| {
        attributes.insert("syslog.timestamp.missing".to_owned(), "true".to_owned());
        received.received_at
    });

    LogRow {
        tenant_id: attribution.tenant_id,
        resource_id: attribution.resource_id,
        site_id: attribution.site_id,
        observed_at,
        ingested_at: received.received_at,
        source_kind: SOURCE_KIND.to_owned(),
        source_vendor: attribution.vendor.to_owned(),
        severity: message.severity.as_str().to_owned(),
        facility: message.facility,
        body: message.message.clone(),
        attributes,
        // Syslog carries neither. RFC 5424 structured data *can* carry a trace id by
        // convention, and reading it is an M8 question rather than a guess to make here.
        trace_id: String::new(),
        span_id: String::new(),
    }
}

/// The identifiers a syslog message offers identity resolution.
///
/// In descending order of how much a match proves, which is the order SPEC §M0.2 ranks
/// them in:
///
/// * the **sender's address**, as `mgmt_ip` — tier 3, confidence 0.80. This is the strong
///   one in practice, because it is the same identifier the poller already wrote for
///   every device it polls, so a switch that is both polled and logging resolves to one
///   resource without anybody configuring anything.
/// * the **hostname the message claims**, confidence 0.65 — weaker, and deliberately
///   second: a device's hostname is whatever somebody typed into it, two customers may
///   both have a `core-sw-01`, and a relay forwards messages whose hostname is not its
///   own.
///
/// Both are offered and the resolver weighs them. Offering only the address would lose
/// every message from a relay; offering only the hostname would trust a field the sender
/// controls.
#[must_use]
pub fn identifiers(received: &Received) -> Vec<uops_core::Identifier> {
    use uops_core::{Identifier, IdentifierKind};

    let mut out = vec![Identifier::new(
        IdentifierKind::MgmtIp,
        received.peer.ip().to_string(),
    )];
    if let Some(host) = &received.message.hostname {
        // Lowercased: DNS is case-insensitive and a device that shouts its hostname
        // should not become a second resource.
        out.push(Identifier::new(
            IdentifierKind::Hostname,
            host.to_lowercase(),
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse;

    fn received(raw: &str) -> Received {
        Received {
            message: parse(raw),
            peer: "192.0.2.10:514".parse().expect("a peer"),
            received_at: chrono::DateTime::parse_from_rfc3339("2026-09-16T12:00:00Z")
                .expect("an instant")
                .into(),
        }
    }

    fn attribution() -> Attribution {
        Attribution {
            tenant_id: TenantId::new(),
            resource_id: ResourceId::new(),
            site_id: SiteId::nil(),
            vendor: "cisco",
        }
    }

    #[test]
    fn syslog_fields_become_semantic_conventions() {
        // The point of the whole product. A log from a switch and a metric from the same
        // switch are only correlatable if they agree what the host is called; the moment
        // one stores `syslog.hostname` instead, the join stops existing and both tables
        // still look fine on their own.
        let row = to_row(
            &received("<34>Oct 11 22:14:15 rtr-01 sshd[1234]: it happened"),
            attribution(),
        );
        assert_eq!(
            row.attributes.get("host.name").map(String::as_str),
            Some("rtr-01")
        );
        assert_eq!(
            row.attributes.get("service.name").map(String::as_str),
            Some("sshd")
        );
        assert_eq!(
            row.attributes.get("process.pid").map(String::as_str),
            Some("1234")
        );
        assert_eq!(row.body, "it happened");
        assert_eq!(row.severity, "critical");
        assert_eq!(row.facility, 4);
        assert_eq!(row.source_kind, "syslog");
    }

    #[test]
    fn syslog_only_fields_are_namespaced() {
        // Worth keeping — `syslog.facility` is how an operator finds the firewall's logs
        // on local4 — and namespaced so they cannot be mistaken for a convention that
        // exists.
        let row = to_row(
            &received("<165>1 2026-09-16T12:00:00Z h app 1 ID47 [a@1 k=\"v\"] body"),
            attribution(),
        );
        assert_eq!(
            row.attributes.get("syslog.msgid").map(String::as_str),
            Some("ID47")
        );
        assert_eq!(
            row.attributes.get("syslog.sd.a@1.k").map(String::as_str),
            Some("v")
        );
        assert_eq!(
            row.attributes.get("syslog.facility").map(String::as_str),
            Some("20")
        );
        assert_eq!(
            row.attributes
                .get("syslog.source.address")
                .map(String::as_str),
            Some("192.0.2.10")
        );
    }

    #[test]
    fn a_device_timestamp_wins_and_the_receipt_time_is_kept_beside_it() {
        let row = to_row(
            &received("<34>1 2026-09-16T11:59:00Z h a - - - late arrival"),
            attribution(),
        );
        assert_eq!(row.observed_at.to_rfc3339(), "2026-09-16T11:59:00+00:00");
        assert_eq!(row.ingested_at.to_rfc3339(), "2026-09-16T12:00:00+00:00");
        assert!(!row.attributes.contains_key("syslog.timestamp.missing"));
    }

    #[test]
    fn a_missing_timestamp_falls_back_to_receipt_and_says_so() {
        // Without the substitution the row lands at the Unix epoch and sorts to the
        // beginning of every search, which is worse than being a few seconds out. Saying
        // so is what makes a timeline nobody can trust into one somebody can question.
        let row = to_row(&received("no priority, no timestamp"), attribution());
        assert_eq!(row.observed_at, row.ingested_at);
        assert_eq!(
            row.attributes
                .get("syslog.timestamp.missing")
                .map(String::as_str),
            Some("true")
        );
    }

    #[test]
    fn a_parse_failure_is_a_row_with_the_text_and_the_reason() {
        // SPEC: never dropped, `parse.error` set, the raw bytes as the body. One filter
        // finds every malformed message in the estate.
        let row = to_row(&received("<34 this is broken"), attribution());
        assert_eq!(row.body, "<34 this is broken");
        assert!(row.attributes.contains_key("parse.error"));
    }

    #[test]
    fn the_identifiers_put_the_address_before_the_hostname() {
        // The address is tier 3 at 0.80 and is the same identifier the poller already
        // wrote, so a switch that is both polled and logging resolves to one resource
        // with nothing configured. The hostname is 0.65 and is whatever somebody typed
        // into the device.
        let ids = identifiers(&received("<34>Oct 11 22:14:15 RTR-01 app: x"));
        assert_eq!(ids.len(), 2);
        assert_eq!(ids[0].kind, uops_core::IdentifierKind::MgmtIp);
        assert_eq!(ids[0].value, "192.0.2.10");
        assert_eq!(ids[1].kind, uops_core::IdentifierKind::Hostname);
        assert_eq!(ids[1].value, "rtr-01", "a hostname is case-insensitive");
    }

    #[test]
    fn a_message_with_no_hostname_still_offers_its_address() {
        // Offering only the hostname would trust a field the sender controls; offering
        // only the address would lose every message from a relay. A message with neither
        // would be unattributable, and there is always an address.
        let ids = identifiers(&received("<34>malformed"));
        assert_eq!(ids.len(), 1);
        assert_eq!(ids[0].kind, uops_core::IdentifierKind::MgmtIp);
    }
}
