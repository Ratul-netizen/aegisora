//! The query AST — SPEC §M0.5.
//!
//! One AST. The UI builds it, the API accepts it, saved alerts *are* instances of it,
//! and the text query language in M6 will be a **parser onto this type** rather than a
//! second path to the database. Anything that can reach `ClickHouse` goes through
//! [`Query`], so every guarantee the compiler makes — tenant injection, limit ceilings,
//! rollup selection — holds for all of them at once.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uops_core::{ResourceGroupId, ResourceId, ResourceKind, SiteId};
use uuid::Uuid;

/// Which telemetry table family a query addresses.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SignalType {
    Metric,
    Log,
    Event,
    State,
    /// M8.
    Trace,
    /// M7.
    Flow,
}

impl SignalType {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Metric => "metric",
            Self::Log => "log",
            Self::Event => "event",
            Self::State => "state",
            Self::Trace => "trace",
            Self::Flow => "flow",
        }
    }
}

/// An absolute, half-open window: `[start, end)`.
///
/// Absolute rather than relative on purpose. "Last 15 minutes" is resolved by whoever
/// builds the query, so compilation stays a pure function of its inputs — which is what
/// makes the golden tests meaningful and a saved alert reproducible after the fact.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimeRange {
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
}

impl TimeRange {
    #[must_use]
    pub const fn new(start: DateTime<Utc>, end: DateTime<Utc>) -> Self {
        Self { start, end }
    }

    /// The window ending now. Evaluated once, here, and then absolute.
    #[must_use]
    pub fn last(d: chrono::Duration) -> Self {
        let end = Utc::now();
        Self {
            start: end - d,
            end,
        }
    }

    #[must_use]
    pub fn span(&self) -> chrono::Duration {
        self.end - self.start
    }
}

/// Which resources to read. Expanded through `resource_alias` before any SQL exists —
/// see [`crate::resolve`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "type")]
pub enum ResourceSelector {
    /// Every resource in the tenant. Still tenant-scoped — there is no "all tenants".
    All,
    Ids {
        ids: Vec<ResourceId>,
    },
    Kind {
        kind: ResourceKind,
    },
    Site {
        site: SiteId,
    },
    /// Walks `resource_dependents()`. `max_depth` is mandatory: an unbounded walk over
    /// a topology graph is a hang, and topology graphs do contain cycles.
    Descendants {
        root: ResourceId,
        max_depth: u8,
    },
    /// Everything in an operator-defined group.
    ///
    /// The one selector that names a set nothing can infer. `Site` is geography and
    /// `Descendants` is topology; a group is somebody's judgement about which resources
    /// matter together, which is what an alert rule's scope and a maintenance window's
    /// target actually need.
    Group {
        group: ResourceGroupId,
    },
    /// Everything carrying this operator tag.
    ///
    /// `environment=production`, `criticality=critical`. A containment question, which
    /// is what `resource_tags_idx` — a `jsonb_path_ops` GIN index — is built for.
    ///
    /// Deliberately one key and one value rather than a map or an expression. A tag
    /// *language* (`criticality=critical AND environment!=staging`) is a real future
    /// feature and belongs in the query parser alongside the log one, not bolted onto a
    /// selector variant where it would arrive without precedence rules or a way to
    /// explain what it matched.
    Tagged {
        key: String,
        value: String,
    },
}

/// A column, or an attribute key that may or may not be materialised as one.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "field")]
pub enum Field {
    ResourceId,
    SiteId,
    ObservedAt,
    IngestedAt,
    SourceKind,
    SourceVendor,
    Severity,
    Facility,
    /// Logs only.
    Body,
    TraceId,
    SpanId,
    /// Metrics only.
    Metric,
    /// Metrics only.
    Value,
    /// Metrics only.
    Unit,
    /// Events only.
    EventCategory,
    /// Events only.
    EventType,
    /// States only.
    PreviousStatus,
    /// States only.
    CurrentStatus,
    /// A semconv attribute (logs/events) or label (metrics). Compiles to a real column
    /// when the key is materialised — W1 measured `GROUP BY attributes['host.name']` at
    /// 2 252 ms, the slowest query in the whole suite.
    Attr {
        key: String,
    },
    /// A time bucket of `seconds`. The Explorer histogram is this plus `count`.
    TimeBucket {
        seconds: u32,
    },
    /// The per-second rate of a counter series. Metrics only, and only as an
    /// aggregation's field — see the compiler.
    ///
    /// Counters are stored raw, always: SPEC §M2 says rates are computed at query time
    /// because a stored rate cannot be recomputed over a different window, cannot be
    /// re-derived after a bug is found, and silently bakes in whatever wrap handling was
    /// current when it was written. This is that computation.
    Rate,
}

impl Field {
    /// Human-readable name, for error messages only. Never interpolated into SQL.
    #[must_use]
    pub fn label(&self) -> String {
        match self {
            Self::ResourceId => "resource_id".into(),
            Self::SiteId => "site_id".into(),
            Self::ObservedAt => "observed_at".into(),
            Self::IngestedAt => "ingested_at".into(),
            Self::SourceKind => "source_kind".into(),
            Self::SourceVendor => "source_vendor".into(),
            Self::Severity => "severity".into(),
            Self::Facility => "facility".into(),
            Self::Body => "body".into(),
            Self::TraceId => "trace_id".into(),
            Self::SpanId => "span_id".into(),
            Self::Metric => "metric".into(),
            Self::Value => "value".into(),
            Self::Unit => "unit".into(),
            Self::EventCategory => "event_category".into(),
            Self::EventType => "event_type".into(),
            Self::PreviousStatus => "previous_status".into(),
            Self::CurrentStatus => "current_status".into(),
            Self::Attr { key } => format!("attributes[{key}]"),
            Self::TimeBucket { seconds } => format!("time_bucket({seconds}s)"),
            Self::Rate => "rate".into(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompareOp {
    Eq,
    Ne,
    Lt,
    Lte,
    Gt,
    Gte,
    In,
    NotIn,
}

impl CompareOp {
    #[must_use]
    pub const fn as_sql(self) -> &'static str {
        match self {
            Self::Eq => "=",
            Self::Ne => "!=",
            Self::Lt => "<",
            Self::Lte => "<=",
            Self::Gt => ">",
            Self::Gte => ">=",
            Self::In => "IN",
            Self::NotIn => "NOT IN",
        }
    }

    #[must_use]
    pub const fn is_set_op(self) -> bool {
        matches!(self, Self::In | Self::NotIn)
    }
}

/// A literal. Every one of these becomes a bound parameter, never text in a statement.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Value {
    Bool(bool),
    Int(i64),
    Float(f64),
    Uuid(Uuid),
    Timestamp(DateTime<Utc>),
    Str(String),
    List(Vec<Value>),
}

/// How a text match is evaluated — and, crucially, whether the text index can help.
///
/// W1 settled this empirically at 100M rows: token search read 8 190 rows, while `LIKE`
/// (1 928 ms) and phrase proximity (2 378 ms) both read the entire tenant. The two slow
/// modes stay in the AST because users need them; they emit a [`crate::QueryWarning`]
/// so the UI can say so before someone waits two seconds wondering why.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TextMode {
    /// `hasAnyTokens()` — index-accelerated.
    AnyToken,
    /// `hasAllTokens()` — index-accelerated.
    AllToken,
    /// Scan. Not index-accelerated, whatever the setting names suggest.
    Substring,
    /// Tokens narrow granules, then the phrase is verified by scanning them.
    Phrase,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "op")]
pub enum Expr {
    And {
        of: Vec<Expr>,
    },
    Or {
        of: Vec<Expr>,
    },
    Not {
        of: Box<Expr>,
    },
    Compare {
        field: Field,
        cmp: CompareOp,
        value: Value,
    },
    Text {
        field: Field,
        mode: TextMode,
        terms: Vec<String>,
    },
    Exists {
        field: Field,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AggFunc {
    Count,
    CountDistinct,
    Sum,
    Min,
    Max,
    Avg,
    P50,
    P95,
    P99,
}

impl AggFunc {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Count => "count",
            Self::CountDistinct => "count_distinct",
            Self::Sum => "sum",
            Self::Min => "min",
            Self::Max => "max",
            Self::Avg => "avg",
            Self::P50 => "p50",
            Self::P95 => "p95",
            Self::P99 => "p99",
        }
    }

    /// The quantile these functions ask for, if any.
    #[must_use]
    pub const fn quantile(self) -> Option<f64> {
        match self {
            Self::P50 => Some(0.5),
            Self::P95 => Some(0.95),
            Self::P99 => Some(0.99),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Aggregation {
    pub func: AggFunc,
    /// `None` only for [`AggFunc::Count`].
    pub field: Option<Field>,
    /// Output column name. Validated as an identifier; it is the one caller-supplied
    /// string that reaches the statement text, so it is checked rather than quoted.
    pub alias: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "by")]
pub enum SortKey {
    Field {
        field: Field,
    },
    /// An [`Aggregation::alias`] declared on the same query.
    Alias {
        alias: String,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Sort {
    pub key: SortKey,
    #[serde(default)]
    pub desc: bool,
}

/// One query. The only thing that can become SQL.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Query {
    pub signal: SignalType,
    pub time: TimeRange,
    pub resources: ResourceSelector,
    #[serde(default)]
    pub filter: Option<Expr>,
    #[serde(default)]
    pub aggregations: Vec<Aggregation>,
    #[serde(default)]
    pub group_by: Vec<Field>,
    #[serde(default)]
    pub order_by: Vec<Sort>,
    /// Always set, always capped server-side — see [`crate::MAX_LIMIT`].
    pub limit: u32,
    #[serde(default)]
    pub offset: u32,
}

impl Query {
    /// The smallest useful query: one signal, one window, everything in the tenant.
    #[must_use]
    pub fn new(signal: SignalType, time: TimeRange) -> Self {
        Self {
            signal,
            time,
            resources: ResourceSelector::All,
            filter: None,
            aggregations: Vec::new(),
            group_by: Vec::new(),
            order_by: Vec::new(),
            limit: 100,
            offset: 0,
        }
    }

    #[must_use]
    pub fn with_resources(mut self, r: ResourceSelector) -> Self {
        self.resources = r;
        self
    }

    #[must_use]
    pub fn with_filter(mut self, f: Expr) -> Self {
        self.filter = Some(f);
        self
    }

    #[must_use]
    pub fn with_limit(mut self, n: u32) -> Self {
        self.limit = n;
        self
    }

    #[must_use]
    pub const fn is_aggregate(&self) -> bool {
        !self.aggregations.is_empty()
    }
}

/// Every field an expression touches, for planning and validation.
pub(crate) fn fields_of(e: &Expr, out: &mut Vec<Field>) {
    match e {
        Expr::And { of } | Expr::Or { of } => {
            for sub in of {
                fields_of(sub, out);
            }
        }
        Expr::Not { of } => fields_of(of, out),
        Expr::Compare { field, .. } | Expr::Text { field, .. } | Expr::Exists { field } => {
            out.push(field.clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_query_round_trips_through_json() {
        // The UI posts this shape verbatim and saved alerts are stored in it. If serde
        // ever reshapes the type, alerts written by an older build stop loading.
        let q = Query::new(
            SignalType::Log,
            TimeRange::new(
                DateTime::from_timestamp(0, 0).unwrap(),
                DateTime::from_timestamp(3600, 0).unwrap(),
            ),
        )
        .with_filter(Expr::And {
            of: vec![
                Expr::Compare {
                    field: Field::Severity,
                    cmp: CompareOp::Gte,
                    value: Value::Str("error".into()),
                },
                Expr::Text {
                    field: Field::Body,
                    mode: TextMode::AllToken,
                    terms: vec!["link".into(), "down".into()],
                },
            ],
        });

        let json = serde_json::to_string(&q).unwrap();
        let back: Query = serde_json::from_str(&json).unwrap();
        assert_eq!(back, q);
    }

    #[test]
    fn optional_parts_of_a_query_may_be_omitted_entirely() {
        let q: Query = serde_json::from_str(
            r#"{"signal":"log",
                "time":{"start":"1970-01-01T00:00:00Z","end":"1970-01-01T01:00:00Z"},
                "resources":{"type":"all"},"limit":50}"#,
        )
        .unwrap();
        assert_eq!(q.limit, 50);
        assert_eq!(q.offset, 0);
        assert!(q.filter.is_none() && !q.is_aggregate());
    }

    #[test]
    fn untagged_values_keep_their_type_through_json() {
        // `untagged` picks the first variant that parses, so the declaration order in
        // `Value` is load-bearing: Uuid and Timestamp must be tried before Str, or
        // every UUID comes back as a string and compiles to the wrong parameter type.
        let v: Value = serde_json::from_str("\"018f2d0e-0000-7000-8000-000000000000\"").unwrap();
        assert!(matches!(v, Value::Uuid(_)), "got {v:?}");
        let v: Value = serde_json::from_str("\"1970-01-01T00:00:00Z\"").unwrap();
        assert!(matches!(v, Value::Timestamp(_)), "got {v:?}");
        let v: Value = serde_json::from_str("\"rtr-01\"").unwrap();
        assert!(matches!(v, Value::Str(_)), "got {v:?}");
    }

    #[test]
    fn fields_of_reaches_every_leaf() {
        let e = Expr::Not {
            of: Box::new(Expr::Or {
                of: vec![
                    Expr::Exists {
                        field: Field::TraceId,
                    },
                    Expr::Compare {
                        field: Field::Attr {
                            key: "host.name".into(),
                        },
                        cmp: CompareOp::Eq,
                        value: Value::Str("rtr-01".into()),
                    },
                ],
            }),
        };
        let mut got = Vec::new();
        fields_of(&e, &mut got);
        assert_eq!(
            got.len(),
            2,
            "planning depends on seeing every field: {got:?}"
        );
    }

    #[test]
    fn set_operators_are_distinguishable_from_scalar_ones() {
        assert!(CompareOp::In.is_set_op() && CompareOp::NotIn.is_set_op());
        assert!(!CompareOp::Eq.is_set_op());
    }
}
