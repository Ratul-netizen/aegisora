//! What goes in, and what comes back.
//!
//! The row types mirror the columns in `ch-migrations/` exactly, including the names.
//! A mismatch is a runtime insert failure on the ingestion path — the worst place to
//! find a typo — so the integration tests insert real rows through these types rather
//! than through hand-written JSON.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uops_core::{ResourceId, SiteId, TenantId};
use uops_query::{QueryWarning, WarningView};

use crate::client::Summary;
use crate::error::{Error, Result};

/// Serialised as `YYYY-MM-DD HH:MM:SS.mmm`, which is what `DateTime64(3)` parses.
///
/// RFC 3339 with its `T` and `Z` is *also* accepted by `ClickHouse`, but the two formats
/// round-trip differently through the query parameters the compiler emits, and having
/// one format everywhere is worth more than the flexibility.
fn clickhouse_datetime<S: serde::Serializer>(
    value: &DateTime<Utc>,
    serializer: S,
) -> std::result::Result<S::Ok, S::Error> {
    serializer.serialize_str(&value.format("%Y-%m-%d %H:%M:%S%.3f").to_string())
}

/// One row of `logs`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LogRow {
    pub tenant_id: TenantId,
    pub resource_id: ResourceId,
    pub site_id: SiteId,
    #[serde(serialize_with = "clickhouse_datetime")]
    pub observed_at: DateTime<Utc>,
    #[serde(serialize_with = "clickhouse_datetime")]
    pub ingested_at: DateTime<Utc>,
    pub source_kind: String,
    pub source_vendor: String,
    /// The Enum8 label: `trace`…`emergency`.
    pub severity: String,
    pub facility: u8,
    pub body: String,
    /// `Map(LowCardinality(String), String)`. The materialised `host_name` and
    /// `service_name` columns are computed by the server from this — they are not sent,
    /// and sending them would be an error.
    pub attributes: std::collections::BTreeMap<String, String>,
    pub trace_id: String,
    pub span_id: String,
}

/// One row of `metrics`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MetricRow {
    pub tenant_id: TenantId,
    pub resource_id: ResourceId,
    pub site_id: SiteId,
    pub metric: String,
    #[serde(serialize_with = "clickhouse_datetime")]
    pub observed_at: DateTime<Utc>,
    #[serde(serialize_with = "clickhouse_datetime")]
    pub ingested_at: DateTime<Utc>,
    pub value: f64,
    pub unit: String,
    pub labels: std::collections::BTreeMap<String, String>,
}

/// One row of `states` — an availability or status transition.
///
/// Written on a *change*, never on every check. A device polled every 30 seconds for a
/// year is a million checks and a handful of transitions, and the table is ordered and
/// retained (1 095 days, against the metrics' 30) on the assumption that it holds the
/// second. A row per check would make the availability report a scan of a million
/// identical rows to find four interesting ones.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StateRow {
    pub tenant_id: TenantId,
    pub resource_id: ResourceId,
    pub site_id: SiteId,
    #[serde(serialize_with = "clickhouse_datetime")]
    pub observed_at: DateTime<Utc>,
    #[serde(serialize_with = "clickhouse_datetime")]
    pub ingested_at: DateTime<Utc>,
    /// How loud this transition is. A device going down is an error; coming back is
    /// informational, and an operator who is paged for a recovery stops reading pages.
    pub severity: String,
    pub previous_status: String,
    pub current_status: String,
    /// What the check saw, in words. This is the sentence an operator reads first and
    /// it is the only part of the row that says *why* — "no reply to 3 ICMP echo
    /// requests within 2s" rather than "down".
    pub reason: String,
    pub attributes: std::collections::BTreeMap<String, String>,
}

/// One column of a result.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Column {
    pub name: String,
    /// The `ClickHouse` type, verbatim. The UI needs it to decide how to render a
    /// value, and a client that guesses from the JSON gets `DateTime64` wrong.
    #[serde(rename = "type")]
    pub ty: String,
}

/// What a query returned.
#[derive(Clone, Debug, Serialize)]
pub struct ResultSet {
    pub columns: Vec<Column>,
    /// Values in column order. `JSONCompact` rather than row objects: repeating every
    /// column name on every one of ten thousand rows is a lot of bandwidth spent
    /// restating the schema.
    pub rows: Vec<Vec<serde_json::Value>>,
    /// Which physical table answered. Surfaced because "which table did this actually
    /// read" is the first question asked of any slow query.
    pub table: &'static str,
    /// Correct-but-slow, reported rather than hidden — see `uops_query::QueryWarning`.
    /// Sent as [`WarningView`], which carries the rendered sentence as well as the tag,
    /// so no client has to keep its own copy of the wording.
    pub warnings: Vec<WarningView>,
    pub rows_read: u64,
    pub bytes_read: u64,
}

#[derive(Deserialize)]
struct JsonCompact {
    meta: Vec<Column>,
    data: Vec<Vec<serde_json::Value>>,
}

impl ResultSet {
    pub(crate) fn parse(
        body: &str,
        table: &'static str,
        warnings: Vec<QueryWarning>,
        summary: Summary,
    ) -> Result<Self> {
        // The one place the compiler's warnings become wire warnings.
        let warnings: Vec<WarningView> = warnings.into_iter().map(Into::into).collect();

        // An empty body is an empty result, not a protocol error: a query matching
        // nothing is the most ordinary outcome there is.
        if body.trim().is_empty() {
            return Ok(Self {
                columns: Vec::new(),
                rows: Vec::new(),
                table,
                warnings,
                rows_read: summary.rows_read,
                bytes_read: summary.bytes_read,
            });
        }

        let parsed: JsonCompact = serde_json::from_str(body)
            .map_err(|e| Error::Protocol(format!("expected JSONCompact: {e}")))?;

        Ok(Self {
            columns: parsed.meta,
            rows: parsed.data,
            table,
            warnings,
            rows_read: summary.rows_read,
            bytes_read: summary.bytes_read,
        })
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.rows.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// The index of a column by name, for a caller that wants one value.
    #[must_use]
    pub fn column(&self, name: &str) -> Option<usize> {
        self.columns.iter().position(|c| c.name == name)
    }

    /// One value, by row and column name.
    #[must_use]
    pub fn value(&self, row: usize, column: &str) -> Option<&serde_json::Value> {
        self.rows.get(row)?.get(self.column(column)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"{
        "meta": [
            {"name": "resource_id", "type": "UUID"},
            {"name": "body", "type": "String"}
        ],
        "data": [
            ["018f0000-0000-7000-8000-0000000000aa", "link down"],
            ["018f0000-0000-7000-8000-0000000000bb", "link up"]
        ],
        "rows": 2
    }"#;

    #[test]
    fn a_result_keeps_its_column_types() {
        // The UI needs them to render: a client guessing from the JSON gets DateTime64
        // wrong, because it arrives as a string.
        let set = ResultSet::parse(SAMPLE, "logs", Vec::new(), Summary::default()).unwrap();

        assert_eq!(set.len(), 2);
        assert_eq!(set.columns[0].ty, "UUID");
        assert_eq!(set.value(1, "body").unwrap(), "link up");
        assert_eq!(set.column("nonexistent"), None);
    }

    #[test]
    fn an_empty_body_is_an_empty_result_not_an_error() {
        // A query matching nothing is the most ordinary outcome there is, and some
        // statements return no body at all.
        let set = ResultSet::parse("", "logs", Vec::new(), Summary::default()).unwrap();
        assert!(set.is_empty());
        assert!(set.columns.is_empty());
    }

    #[test]
    fn a_body_that_is_not_json_is_a_protocol_error() {
        let err = ResultSet::parse(
            "Code: 62. DB::Exception",
            "logs",
            Vec::new(),
            Summary::default(),
        )
        .unwrap_err();
        assert!(matches!(err, Error::Protocol(_)), "{err}");
    }

    #[test]
    fn the_summary_travels_with_the_result() {
        // rows_read is what W1 was written around, and what the access log records.
        let summary = Summary {
            rows_read: 16_380,
            bytes_read: 1_048_576,
            rows_returned: 2,
        };
        let set = ResultSet::parse(SAMPLE, "logs", Vec::new(), summary).unwrap();
        assert_eq!(set.rows_read, 16_380);
        assert_eq!(set.bytes_read, 1_048_576);
    }

    #[test]
    fn timestamps_serialise_in_the_format_datetime64_parses() {
        // RFC 3339 with its T and Z is also accepted, but having one format everywhere
        // is worth more than the flexibility — the query compiler emits this one.
        let row = MetricRow {
            tenant_id: TenantId::nil(),
            resource_id: ResourceId::nil(),
            site_id: SiteId::nil(),
            metric: "system.cpu.utilization".into(),
            observed_at: DateTime::from_timestamp(1_700_000_000, 0).unwrap(),
            ingested_at: DateTime::from_timestamp(1_700_000_001, 0).unwrap(),
            value: 0.5,
            unit: "1".into(),
            labels: std::collections::BTreeMap::new(),
        };

        let json = serde_json::to_string(&row).unwrap();
        assert!(
            json.contains("\"observed_at\":\"2023-11-14 22:13:20.000\""),
            "{json}"
        );
        assert!(!json.contains('T'), "{json}");
    }
}
