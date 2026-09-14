//! Envelope fixtures.
//!
//! Public rather than `#[cfg(test)]` because [`crate::conformance`] is public: a future
//! `uops-bus-nats` crate runs the same suite, and it needs the same envelopes.

use chrono::Utc;
use uops_core::{AttrMap, LogRecord, Signal, Source, SourceKind, TelemetryEnvelope, TenantId};

/// A minimal syslog envelope for one tenant.
#[must_use]
pub fn envelope(tenant: TenantId) -> TelemetryEnvelope {
    envelope_with_body(
        tenant,
        "%LINK-3-UPDOWN: Interface Gi0/1, changed state to down",
    )
}

/// The same, with a body the caller can recognise — used to assert ordering, where the
/// test has to tell one message from the next.
#[must_use]
pub fn envelope_with_body(tenant: TenantId, body: &str) -> TelemetryEnvelope {
    let now = Utc::now();
    TelemetryEnvelope {
        tenant_id: tenant,
        site_id: None,
        // None on purpose: the pipeline resolves identity, not the collector, so this
        // is what a real published envelope looks like.
        resource_id: None,
        identity: None,
        observed_at: now,
        ingested_at: now,
        source: Source {
            kind: SourceKind::Syslog,
            vendor: Some("cisco".into()),
            collector_id: "conformance".into(),
        },
        severity: Some(uops_core::Severity::Error),
        attributes: AttrMap::new(),
        signal: Signal::Log(LogRecord {
            body: body.to_owned(),
            facility: Some(23),
            trace_id: None,
            span_id: None,
        }),
    }
}

/// The body of a log envelope, for assertions.
#[must_use]
pub fn body_of(envelope: &TelemetryEnvelope) -> String {
    match &envelope.signal {
        Signal::Log(log) => log.body.clone(),
        other => panic!("expected a log envelope, got {}", other.kind()),
    }
}
