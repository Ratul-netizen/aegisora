//! `ExportLogsServiceRequest` → [`LogRow`].
//!
//! The same row a syslog message becomes, which is the whole point — see the crate docs.
//!
//! # Severity
//!
//! OTLP numbers severity 1–24 in four-wide bands: TRACE, DEBUG, INFO, WARN, ERROR, FATAL.
//! The `logs` column is an `Enum8` of nine names shared with syslog and with events, so
//! the mapping is a narrowing and the interesting end is the top one.
//!
//! FATAL's four values spread across `critical`, `alert` and `emergency` rather than
//! collapsing to one. Syslog devices really do use all three and an alert rule written
//! against `emergency` should not be unreachable from OTLP — a platform where the same
//! severity means different things depending on which collector produced it is a platform
//! whose alert rules are wrong for half the estate.
//!
//! `severity_text` is kept in the attributes untouched. It is what the application
//! actually wrote, the number is an interpretation, and an operator debugging why a rule
//! did not fire needs both.

use uops_core::semconv;
use uops_pipeline::Attribution;
use uops_store_ch::LogRow;

use opentelemetry_proto::tonic::logs::v1::{LogRecord, ResourceLogs};

use crate::{SOURCE_KIND, attributes, flatten, hex, instant, resource_attributes};

/// What the `logs` table's `Enum8` calls an OTLP severity number.
///
/// Unknown numbers — 0, or anything above 24 — become `info`. Not `unknown`: the column
/// has no such value, and a record whose severity could not be read is still a record
/// somebody needs to see. The original is in `otlp.severity_text` either way.
#[must_use]
pub fn severity(number: i32) -> &'static str {
    match number {
        1..=4 => "trace",
        5..=8 => "debug",
        13..=16 => "warn",
        17..=20 => "error",
        21 => "critical",
        22 => "alert",
        23..=24 => "emergency",
        // 9..=12 is INFO, and so is anything unrecognised.
        _ => "info",
    }
}

/// Everything one `ResourceLogs` carries, ready for a caller that has resolved it.
///
/// Split from the conversion because resolution happens between them: a caller reads
/// [`crate::observed`] from `resource`, asks the pipeline, and then calls [`to_rows`]
/// with what came back. That is the same shape the syslog path has and the reason both
/// share one resolver.
#[derive(Debug)]
pub struct Batch<'a> {
    pub resource: std::collections::BTreeMap<String, String>,
    pub records: Vec<&'a LogRecord>,
}

/// Group a request by resource, which is the unit identity resolution works on.
///
/// Scopes are flattened away. A scope is the instrumentation library that produced the
/// record — `opentelemetry-instrumentation-http` and so on — and it is an attribute of
/// the record, not a thing telemetry attaches to. Keeping the grouping would mean
/// resolving identity once per library per resource, which is the same answer several
/// times.
#[must_use]
pub fn batches(request: &[ResourceLogs]) -> Vec<Batch<'_>> {
    request
        .iter()
        .map(|rl| Batch {
            resource: resource_attributes(rl.resource.as_ref()),
            records: rl
                .scope_logs
                .iter()
                .flat_map(|sl| sl.log_records.iter())
                .collect(),
        })
        .collect()
}

/// Turn one resource's records into rows.
///
/// `received_at` is when this process read the request, and is what `ingested_at` becomes
/// for every row — including the ones whose own timestamps are missing. See [`instant`]
/// on why a zero timestamp is not the epoch.
#[must_use]
pub fn to_rows(
    batch: &Batch<'_>,
    attribution: &Attribution,
    received_at: chrono::DateTime<chrono::Utc>,
) -> Vec<LogRow> {
    batch
        .records
        .iter()
        .map(|record| to_row(&batch.resource, record, attribution, received_at))
        .collect()
}

fn to_row(
    resource: &std::collections::BTreeMap<String, String>,
    record: &LogRecord,
    attribution: &Attribution,
    received_at: chrono::DateTime<chrono::Utc>,
) -> LogRow {
    // Resource attributes first, then the record's own, so a record may override what its
    // resource said about itself. That is the direction OTLP intends: the resource
    // describes the emitter and the record describes the event, and the more specific of
    // two statements about the same key is the record's.
    let mut attrs = resource.clone();
    attrs.extend(attributes(&record.attributes));

    if !record.severity_text.is_empty() {
        // What the application actually wrote. The number is an interpretation, and an
        // operator debugging why a rule did not fire needs both.
        attrs.insert(
            "otlp.severity_text".to_owned(),
            record.severity_text.clone(),
        );
    }

    // `observed_time_unix_nano` is when the *collector* saw it, which is a better
    // fallback than this process's clock: it is closer to the event and it is still a
    // real observation rather than a guess.
    let observed_at = instant(record.time_unix_nano)
        .or_else(|| instant(record.observed_time_unix_nano))
        .unwrap_or_else(|| {
            attrs.insert("otlp.timestamp.missing".to_owned(), "true".to_owned());
            received_at
        });

    LogRow {
        tenant_id: attribution.tenant_id,
        resource_id: attribution.resource_id,
        site_id: attribution.site_id,
        observed_at,
        ingested_at: received_at,
        source_kind: SOURCE_KIND.to_owned(),
        source_vendor: attribution.vendor.clone(),
        severity: severity(record.severity_number).to_owned(),
        // Syslog's facility, which OTLP has no equivalent of. 1 is `user`, the same
        // default RFC 3164 prescribes for a message that does not say — rather than 0,
        // which is `kern` and would be a claim.
        facility: 1,
        body: record.body.as_ref().map_or_else(String::new, flatten),
        attributes: attrs,
        trace_id: hex(&record.trace_id),
        span_id: hex(&record.span_id),
    }
}

/// Whether a resource says enough to be resolved at all.
///
/// A payload with no `host.id`, no `host.name` and no `service.name` describes nothing.
/// It is still ingested — the resolver mints a provisional resource, which is rule 1 —
/// but it is worth counting, because a collector misconfigured this way sends every
/// record in the estate to one resource and the only symptom is a resource with
/// impossible traffic.
#[must_use]
pub fn is_anonymous(resource: &std::collections::BTreeMap<String, String>) -> bool {
    [semconv::HOST_ID, semconv::HOST_NAME, semconv::SERVICE_NAME]
        .iter()
        .all(|k| resource.get(*k).is_none_or(String::is_empty))
}

#[cfg(test)]
mod tests {
    use super::*;
    use opentelemetry_proto::tonic::common::v1::{AnyValue, KeyValue, any_value};
    use opentelemetry_proto::tonic::logs::v1::{ResourceLogs, ScopeLogs};
    use opentelemetry_proto::tonic::resource::v1::Resource;

    fn string(v: &str) -> AnyValue {
        AnyValue {
            value: Some(any_value::Value::StringValue(v.to_owned())),
        }
    }

    fn attribution() -> Attribution {
        Attribution {
            tenant_id: uops_core::TenantId::new(),
            resource_id: uops_core::ResourceId::new(),
            site_id: uops_core::SiteId::nil(),
            vendor: String::new(),
        }
    }

    fn at(s: &str) -> chrono::DateTime<chrono::Utc> {
        chrono::DateTime::parse_from_rfc3339(s)
            .expect("an instant")
            .with_timezone(&chrono::Utc)
    }

    fn request(records: Vec<LogRecord>) -> Vec<ResourceLogs> {
        vec![ResourceLogs {
            resource: Some(Resource {
                attributes: vec![
                    KeyValue {
                        key: semconv::HOST_NAME.to_owned(),
                        value: Some(string("app-01")),
                        ..KeyValue::default()
                    },
                    KeyValue {
                        key: semconv::SERVICE_NAME.to_owned(),
                        value: Some(string("checkout")),
                        ..KeyValue::default()
                    },
                ],
                ..Resource::default()
            }),
            scope_logs: vec![ScopeLogs {
                log_records: records,
                ..ScopeLogs::default()
            }],
            ..ResourceLogs::default()
        }]
    }

    fn record(body: &str) -> LogRecord {
        LogRecord {
            time_unix_nano: 1_700_000_000_000_000_000,
            severity_number: 17,
            severity_text: "ERROR".to_owned(),
            body: Some(string(body)),
            ..LogRecord::default()
        }
    }

    #[test]
    fn an_otlp_record_becomes_the_same_row_a_syslog_message_does() {
        // The point of the whole crate. Not a similar row — the same type, the same keys,
        // resolved by the same resolver and batched by the same batcher.
        let request = request(vec![record("database timeout")]);
        let batches = batches(&request);
        let rows = to_rows(&batches[0], &attribution(), at("2026-09-17T10:00:00Z"));

        assert_eq!(rows.len(), 1);
        let row = &rows[0];
        assert_eq!(row.body, "database timeout");
        assert_eq!(row.severity, "error");
        assert_eq!(row.source_kind, "otlp");
        // Straight through, with no translation — which is the difference from syslog,
        // where `hostname` had to become `host.name`.
        assert_eq!(
            row.attributes.get(semconv::HOST_NAME).map(String::as_str),
            Some("app-01")
        );
        assert_eq!(
            row.attributes
                .get(semconv::SERVICE_NAME)
                .map(String::as_str),
            Some("checkout")
        );
    }

    #[test]
    fn severity_spreads_fatal_rather_than_collapsing_it() {
        // Syslog devices really do use all three of critical, alert and emergency, and an
        // alert rule written against `emergency` must not be unreachable from OTLP.
        assert_eq!(severity(1), "trace");
        assert_eq!(severity(5), "debug");
        assert_eq!(severity(9), "info");
        assert_eq!(severity(13), "warn");
        assert_eq!(severity(17), "error");
        assert_eq!(severity(21), "critical");
        assert_eq!(severity(22), "alert");
        assert_eq!(severity(24), "emergency");

        // 0 is UNSPECIFIED and anything above 24 is not a severity. `info` rather than a
        // dropped record: the column has no `unknown`, and the original is kept.
        assert_eq!(severity(0), "info");
        assert_eq!(severity(99), "info");
    }

    #[test]
    fn the_text_the_application_wrote_is_kept_beside_the_number() {
        // The number is an interpretation. An operator debugging why a rule did not fire
        // needs what was actually sent.
        let mut r = record("x");
        r.severity_number = 21;
        r.severity_text = "PANIC".to_owned();

        let request = request(vec![r]);
        let rows = to_rows(
            &batches(&request)[0],
            &attribution(),
            at("2026-09-17T10:00:00Z"),
        );
        assert_eq!(rows[0].severity, "critical");
        assert_eq!(
            rows[0]
                .attributes
                .get("otlp.severity_text")
                .map(String::as_str),
            Some("PANIC")
        );
    }

    #[test]
    fn a_record_attribute_wins_over_its_resource() {
        // The direction OTLP intends: the resource describes the emitter, the record
        // describes the event, and the more specific of two statements is the record's.
        let mut r = record("x");
        r.attributes = vec![KeyValue {
            key: semconv::HOST_NAME.to_owned(),
            value: Some(string("the-real-host")),
            ..KeyValue::default()
        }];

        let request = request(vec![r]);
        let rows = to_rows(
            &batches(&request)[0],
            &attribution(),
            at("2026-09-17T10:00:00Z"),
        );
        assert_eq!(
            rows[0]
                .attributes
                .get(semconv::HOST_NAME)
                .map(String::as_str),
            Some("the-real-host")
        );
    }

    #[test]
    fn a_missing_timestamp_falls_back_and_says_so() {
        // Same rule as the syslog path. A record at the Unix epoch sorts to the top of
        // every search, which is worse than being a few seconds out.
        let mut r = record("x");
        r.time_unix_nano = 0;
        r.observed_time_unix_nano = 0;

        let request = request(vec![r]);
        let received = at("2026-09-17T10:00:00Z");
        let rows = to_rows(&batches(&request)[0], &attribution(), received);
        assert_eq!(rows[0].observed_at, received);
        assert_eq!(
            rows[0]
                .attributes
                .get("otlp.timestamp.missing")
                .map(String::as_str),
            Some("true")
        );
    }

    #[test]
    fn the_collectors_clock_is_preferred_to_this_processs() {
        // `observed_time_unix_nano` is when the collector saw it: closer to the event
        // than this process's clock, and still a real observation rather than a guess.
        let mut r = record("x");
        r.time_unix_nano = 0;
        r.observed_time_unix_nano = 1_700_000_000_000_000_000;

        let request = request(vec![r]);
        let rows = to_rows(
            &batches(&request)[0],
            &attribution(),
            at("2026-09-17T10:00:00Z"),
        );
        assert_eq!(rows[0].observed_at, at("2023-11-14T22:13:20Z"));
        assert!(!rows[0].attributes.contains_key("otlp.timestamp.missing"));
    }

    #[test]
    fn scopes_are_flattened_because_identity_does_not_care_which_library_emitted() {
        // A scope is the instrumentation library. Keeping the grouping would mean
        // resolving identity once per library per resource — the same answer several
        // times.
        let request = vec![ResourceLogs {
            resource: Some(Resource::default()),
            scope_logs: vec![
                ScopeLogs {
                    log_records: vec![record("from http")],
                    ..ScopeLogs::default()
                },
                ScopeLogs {
                    log_records: vec![record("from sql")],
                    ..ScopeLogs::default()
                },
            ],
            ..ResourceLogs::default()
        }];

        let batches = batches(&request);
        assert_eq!(batches.len(), 1, "one resource is one batch");
        assert_eq!(batches[0].records.len(), 2);
    }

    #[test]
    fn a_resource_that_identifies_nothing_is_recognised() {
        // Still ingested — the resolver mints a provisional, which is rule 1 — but worth
        // counting: a collector misconfigured this way sends every record in the estate
        // to one resource, and the only symptom is a resource with impossible traffic.
        let empty = std::collections::BTreeMap::new();
        assert!(is_anonymous(&empty));

        let named: std::collections::BTreeMap<String, String> =
            [(semconv::HOST_NAME.to_owned(), "app-01".to_owned())]
                .into_iter()
                .collect();
        assert!(!is_anonymous(&named));

        let blank: std::collections::BTreeMap<String, String> =
            [(semconv::HOST_NAME.to_owned(), String::new())]
                .into_iter()
                .collect();
        assert!(is_anonymous(&blank), "an empty name identifies nothing");
    }

    #[test]
    fn a_trace_id_survives_into_the_row() {
        // The join the Investigation Workspace will make. A log without its trace id is
        // a log that cannot be correlated with the request that produced it.
        let mut r = record("x");
        r.trace_id = vec![0x4b, 0xf9, 0x2f, 0x35];
        r.span_id = vec![0x00, 0xf0];

        let request = request(vec![r]);
        let rows = to_rows(
            &batches(&request)[0],
            &attribution(),
            at("2026-09-17T10:00:00Z"),
        );
        assert_eq!(rows[0].trace_id, "4bf92f35");
        assert_eq!(rows[0].span_id, "00f0");
    }
}
