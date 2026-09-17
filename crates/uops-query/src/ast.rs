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

/// How far back a tail's `observed_at` bracket reaches behind the ingest window.
///
/// The tail's exact predicate is on `ingested_at` — see [`follow`] — but partitions are
/// `toYYYYMMDD(observed_at)`, so without a bracket on that column every poll would scan
/// every partition in the retention period. An hour is the compromise: a WAL segment
/// replayed after a `ClickHouse` restart, or a device whose clock is a few minutes slow,
/// still appears in the tail; a log line stamped last Tuesday appears in a search
/// instead. That is a real limit, and it is written down rather than discovered.
pub const TAIL_LOOKBACK: chrono::Duration = chrono::Duration::hours(1);

/// How far ahead of the poll instant the same bracket reaches.
///
/// Device clocks run fast. A switch five minutes ahead of the collector writes rows with
/// an `observed_at` in the future, and a bracket ending at *now* would hold them out of
/// the tail until their own timestamp came round — which is to say, it would show them
/// five minutes late, in a view whose entire purpose is to be live.
pub const TAIL_SKEW: chrono::Duration = chrono::Duration::minutes(5);

/// The same query, restricted to what arrived since the last poll.
///
/// This is what makes the live tail a *stream* rather than a repeated search. The
/// window is half-open on **`ingested_at`**, `[since, now)`, so consecutive polls
/// partition the rows exactly: every row that reaches storage is delivered once, and
/// none is delivered twice. Windowing on `observed_at` instead — the obvious spelling —
/// loses every row whose ingest lagged its timestamp past the watermark, which during
/// the incident that makes somebody open a tail is precisely when lag is worst.
///
/// `observed_at` still gets a bracket, because that is what the table is partitioned on;
/// see [`TAIL_LOOKBACK`] and [`TAIL_SKEW`] for how wide it is and what falls outside it.
///
/// Aggregation, grouping, ordering and paging are dropped rather than rejected: a caller
/// asks to follow *a search*, and the search it is following may perfectly well be the
/// histogram's. [`crate::compile_tail`] refuses the ones that would change what a tail
/// means; this removes them first so that following any saved search is possible at all.
#[must_use]
pub fn follow(q: &Query, since: DateTime<Utc>, now: DateTime<Utc>) -> Query {
    let arrived = Expr::And {
        of: vec![
            Expr::Compare {
                field: Field::IngestedAt,
                cmp: CompareOp::Gte,
                value: Value::Timestamp(since),
            },
            Expr::Compare {
                field: Field::IngestedAt,
                cmp: CompareOp::Lt,
                value: Value::Timestamp(now),
            },
        ],
    };

    Query {
        time: TimeRange::new(since - TAIL_LOOKBACK, now + TAIL_SKEW),
        filter: Some(match q.filter.clone() {
            // Flattened into the caller's `and` rather than nested inside another one.
            // A tail's compiled SQL is read by whoever is explaining a slow poll, and
            // three levels of parentheses around two timestamps helps nobody.
            Some(Expr::And { mut of }) => {
                of.push(arrived);
                Expr::And { of }
            }
            Some(other) => Expr::And {
                of: vec![other, arrived],
            },
            None => arrived,
        }),
        aggregations: Vec::new(),
        group_by: Vec::new(),
        order_by: Vec::new(),
        offset: 0,
        ..q.clone()
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
    /// The three properties a stream has to have, checked on the shape rather than on
    /// the SQL: consecutive polls partition the rows, the ingest predicate is present,
    /// and the caller's own filter survives.
    #[test]
    fn consecutive_tail_windows_are_contiguous_and_half_open() {
        let t0 = DateTime::from_timestamp(1_000_000, 0).unwrap();
        let t1 = t0 + chrono::Duration::seconds(2);
        let t2 = t1 + chrono::Duration::seconds(2);

        let base = Query::new(SignalType::Log, TimeRange::new(t0, t1));
        let first = follow(&base, t0, t1);
        let second = follow(&base, t1, t2);

        // `[t0, t1)` then `[t1, t2)`: a row ingested exactly at t1 is in the second poll
        // and only the second. Every other spelling either duplicates it or loses it.
        assert_eq!(ingest_bounds(&first), (t0, t1));
        assert_eq!(ingest_bounds(&second), (t1, t2));
    }

    #[test]
    fn a_tail_brackets_observed_at_around_the_ingest_window() {
        let since = DateTime::from_timestamp(1_000_000, 0).unwrap();
        let now = since + chrono::Duration::seconds(2);
        let q = follow(
            &Query::new(SignalType::Log, TimeRange::new(since, now)),
            since,
            now,
        );

        // Wide enough for a replayed WAL segment and a fast device clock; bounded, so a
        // poll never asks ClickHouse for every partition in the retention period.
        assert_eq!(q.time.start, since - TAIL_LOOKBACK);
        assert_eq!(q.time.end, now + TAIL_SKEW);
    }

    #[test]
    fn a_tail_keeps_the_search_it_is_following() {
        let since = DateTime::from_timestamp(1_000_000, 0).unwrap();
        let now = since + chrono::Duration::seconds(2);

        let searching =
            Query::new(SignalType::Log, TimeRange::new(since, now)).with_filter(Expr::Text {
                field: Field::Body,
                mode: TextMode::AnyToken,
                terms: vec!["timeout".into()],
            });

        let Some(Expr::And { of }) = follow(&searching, since, now).filter else {
            panic!("a followed search is its own filter AND the ingest window");
        };
        assert!(matches!(of.first(), Some(Expr::Text { .. })), "{of:?}");
        assert_eq!(of.len(), 2);
    }

    #[test]
    fn following_an_aggregate_follows_its_rows() {
        // The histogram is a Query too, and "tail this" has an obvious meaning: the rows
        // the bars are counting. compile_tail refuses an aggregate, so dropping the
        // aggregation here is what makes that request answerable at all.
        let since = DateTime::from_timestamp(1_000_000, 0).unwrap();
        let now = since + chrono::Duration::seconds(2);

        let mut histogram = Query::new(SignalType::Log, TimeRange::new(since, now));
        histogram.aggregations = vec![Aggregation {
            func: AggFunc::Count,
            field: None,
            alias: "n".into(),
        }];
        histogram.group_by = vec![Field::TimeBucket { seconds: 60 }];
        histogram.offset = 500;

        let tail = follow(&histogram, since, now);
        assert!(!tail.is_aggregate() && tail.group_by.is_empty());
        assert_eq!(tail.offset, 0, "a stream has no stable offset to page from");
    }

    /// The `[start, end)` the ingest predicate actually asks for.
    fn ingest_bounds(q: &Query) -> (DateTime<Utc>, DateTime<Utc>) {
        let mut bounds = Vec::new();
        let mut fields = Vec::new();
        let filter = q.filter.clone().expect("a tail always filters on ingest");
        fields_of(&filter, &mut fields);
        assert!(fields.iter().all(|f| *f == Field::IngestedAt), "{fields:?}");

        collect_timestamps(&filter, &mut bounds);
        assert_eq!(bounds.len(), 2, "{bounds:?}");
        (bounds[0], bounds[1])
    }

    fn collect_timestamps(e: &Expr, out: &mut Vec<DateTime<Utc>>) {
        match e {
            Expr::And { of } | Expr::Or { of } => {
                for sub in of {
                    collect_timestamps(sub, out);
                }
            }
            Expr::Compare {
                value: Value::Timestamp(t),
                ..
            } => out.push(*t),
            _ => {}
        }
    }
}
