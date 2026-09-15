//! Table selection — which physical table answers this query.
//!
//! The caller never names a table (SPEC §M0.6). It says "logs, this window, grouped
//! into five-minute buckets" and the planner decides whether that is the base table,
//! the pre-aggregate, or a rollup. This is where the two W1 fixes actually take effect:
//!
//! | W1 finding | what the planner does |
//! |---|---|
//! | Explorer histogram 1 066 ms, and the `p_by_time` projection does **not** help — a histogram over the retention window touches every row whatever the sort order | routes count-only bucket queries to `logs_counts_5m` |
//! | `GROUP BY attributes['host.name']` was the slowest query in the suite at 2 252 ms | rewrites materialised semconv keys to their real columns, and warns on the ones that are not |
//!
//! The tail's fix — the `p_by_time` projection — needs nothing here: `ClickHouse`
//! chooses a projection itself when the `ORDER BY` matches. That is why
//! [`crate::compile_tail`] is a separate entry point rather than a separate table.

use uops_core::attr::semconv;

use crate::ast::{AggFunc, Aggregation, Field, Query, SignalType, SortKey, fields_of};
use crate::error::{Error, Result};
use crate::warning::QueryWarning;

/// Longest window still answered from raw metric points. Beyond this the raw rows are
/// past their 30-day TTL for part of the range, so a raw query would quietly return a
/// truncated series — worse than downsampling, because it looks complete.
pub const RAW_METRIC_SPAN: chrono::TimeDelta = chrono::TimeDelta::hours(6);
/// Beyond this, the 5-minute rollup is itself past retention and the hourly one serves.
pub const FIVE_MINUTE_SPAN: chrono::TimeDelta = chrono::TimeDelta::days(30);

/// The bucket width of `logs_counts_5m` and `metrics_5m`. A histogram finer than this
/// cannot be served from them.
pub const PREAGGREGATE_BUCKET_SECONDS: u32 = 300;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TableKind {
    /// The base `MergeTree` table. Every column is available.
    Base,
    /// `logs_counts_5m` — `AggregatingMergeTree`, counts only.
    LogCounts,
    /// `metrics_5m` / `metrics_1h` — `AggregatingMergeTree` states, not raw values.
    MetricRollup,
}

/// Which table, and the handful of facts codegen needs about its shape.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TablePlan {
    pub table: &'static str,
    pub kind: TableKind,
    pub signal: SignalType,
    /// The time column to filter and bucket on: `observed_at`, or `bucket` on a
    /// pre-aggregate.
    pub time_col: &'static str,
    /// Name of the map column holding attributes on this signal.
    pub attr_map: &'static str,
    /// Bucket width already baked into the stored rows, if any.
    pub stored_bucket_seconds: u32,
}

/// A resolved column reference.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Col {
    /// A real column. Always from a fixed set — never caller text.
    Plain(&'static str),
    /// A map lookup. The key is bound as a parameter.
    Attr { map: &'static str, key: String },
    /// `toStartOfInterval` over the plan's time column.
    Bucket { seconds: u32, base: &'static str },
}

/// Pick the table, and say what was given up to get there.
pub(crate) fn plan(q: &Query) -> Result<(TablePlan, Vec<QueryWarning>)> {
    let mut warnings = Vec::new();

    let base = |table: &'static str, attr_map: &'static str| TablePlan {
        table,
        kind: TableKind::Base,
        signal: q.signal,
        time_col: "observed_at",
        attr_map,
        stored_bucket_seconds: 0,
    };

    let plan = match q.signal {
        SignalType::Trace => return Err(Error::Unsupported("trace queries (M8)")),
        SignalType::Flow => return Err(Error::Unsupported("flow queries (M7)")),
        SignalType::Event => base("events", "attributes"),
        SignalType::State => base("states", "attributes"),
        SignalType::Log => {
            if serves_from_log_counts(q) {
                TablePlan {
                    table: "logs_counts_5m",
                    kind: TableKind::LogCounts,
                    signal: q.signal,
                    time_col: "bucket",
                    attr_map: "attributes",
                    stored_bucket_seconds: PREAGGREGATE_BUCKET_SECONDS,
                }
            } else {
                base("logs", "attributes")
            }
        }
        SignalType::Metric => match metric_rollup(q)? {
            None => base("metrics", "labels"),
            Some((table, bucket)) => {
                warnings.push(QueryWarning::Downsampled {
                    table: table.into(),
                    bucket_seconds: bucket,
                });
                TablePlan {
                    table,
                    kind: TableKind::MetricRollup,
                    signal: q.signal,
                    time_col: "bucket",
                    attr_map: "labels",
                    stored_bucket_seconds: bucket,
                }
            }
        },
    };

    Ok((plan, warnings))
}

/// W1 FIX 2. `logs_counts_5m` stores `(tenant_id, resource_id, severity, bucket)` and
/// nothing else, so it can answer the Explorer histogram and only the Explorer
/// histogram. Every condition below is a column that table does not have.
fn serves_from_log_counts(q: &Query) -> bool {
    let count_only = q.aggregations.len() == 1
        && matches!(
            q.aggregations[0],
            Aggregation {
                func: AggFunc::Count,
                field: None,
                ..
            }
        );
    if !count_only {
        return false;
    }

    let available = |f: &Field| match f {
        Field::ResourceId | Field::Severity => true,
        // A finer bucket than the stored one cannot be recovered by re-bucketing.
        Field::TimeBucket { seconds } => {
            *seconds >= PREAGGREGATE_BUCKET_SECONDS && seconds % PREAGGREGATE_BUCKET_SECONDS == 0
        }
        _ => false,
    };

    if !q.group_by.iter().all(available) {
        return false;
    }
    if let Some(f) = &q.filter {
        let mut fields = Vec::new();
        fields_of(f, &mut fields);
        if !fields.iter().all(available) {
            return false;
        }
    }
    q.order_by.iter().all(|s| match &s.key {
        SortKey::Field { field } => available(field),
        SortKey::Alias { .. } => true,
    })
}

/// Which metric rollup, if any. `None` means the raw table.
fn metric_rollup(q: &Query) -> Result<Option<(&'static str, u32)>> {
    // Rollups hold aggregate states, not points. A query asking for individual samples
    // can only be answered from raw rows, however wide its window is.
    if !q.is_aggregate() {
        return Ok(None);
    }

    let span = q.time.span();
    let (table, bucket) = if span <= RAW_METRIC_SPAN {
        return Ok(None);
    } else if span <= FIVE_MINUTE_SPAN {
        ("metrics_5m", 300)
    } else {
        ("metrics_1h", 3_600)
    };

    // AggregatingMergeTree stores the states that were declared in the materialised
    // view — min, max, avg, count. A sum cannot be recovered from an average without
    // the exact count per bucket, and a quantile cannot be recovered at all. Erroring
    // is the honest answer: the raw rows those need are past their TTL.
    for a in &q.aggregations {
        let ok = matches!(
            a.func,
            AggFunc::Count | AggFunc::Min | AggFunc::Max | AggFunc::Avg
        );
        if !ok {
            return Err(Error::RollupCannotServe {
                what: a.func.label().to_owned(),
                table,
                why: "the rollup stores min, max, avg and count states only; \
                      narrow the window to reach raw points",
            });
        }
    }

    // Labels are not carried into the rollup, by design — that is most of why it is
    // small enough to keep for three years.
    let mut referenced: Vec<Field> = q.group_by.clone();
    if let Some(f) = &q.filter {
        fields_of(f, &mut referenced);
    }
    for f in &referenced {
        let ok = matches!(
            f,
            Field::ResourceId | Field::Metric | Field::TimeBucket { .. }
        );
        if !ok {
            return Err(Error::RollupCannotServe {
                what: f.label(),
                table,
                why: "the rollup keeps resource, metric and bucket only; \
                      labels are dropped when points are aggregated",
            });
        }
    }

    Ok(Some((table, bucket)))
}

/// Resolve a field to a column on the planned table, or explain why it is not there.
pub(crate) fn column_of(f: &Field, p: &TablePlan, warnings: &mut Vec<QueryWarning>) -> Result<Col> {
    use SignalType as S;

    let unavailable = || {
        Err(Error::FieldNotAvailable {
            field: f.label(),
            signal: p.signal.as_str(),
        })
    };

    // A pre-aggregate has four columns and no more. Checked before the per-signal
    // mapping so the error names the real reason.
    if p.kind != TableKind::Base {
        return match (p.kind, f) {
            (_, Field::ResourceId) => Ok(Col::Plain("resource_id")),
            (TableKind::LogCounts, Field::Severity) => Ok(Col::Plain("severity")),
            (TableKind::MetricRollup, Field::Metric) => Ok(Col::Plain("metric")),
            // Re-bucketing rows that are already at the requested width is a function
            // call per row for no change in the result.
            (_, Field::TimeBucket { seconds }) if *seconds == p.stored_bucket_seconds => {
                Ok(Col::Plain(p.time_col))
            }
            (_, Field::TimeBucket { seconds }) => Ok(Col::Bucket {
                seconds: *seconds,
                base: p.time_col,
            }),
            (_, Field::ObservedAt) => Ok(Col::Plain(p.time_col)),
            _ => Err(Error::FieldNotAvailable {
                field: f.label(),
                signal: p.table,
            }),
        };
    }

    Ok(match f {
        Field::ResourceId => Col::Plain("resource_id"),
        Field::SiteId => Col::Plain("site_id"),
        Field::ObservedAt => Col::Plain("observed_at"),
        Field::IngestedAt => Col::Plain("ingested_at"),
        Field::TimeBucket { seconds } => Col::Bucket {
            seconds: *seconds,
            base: p.time_col,
        },

        Field::Severity => match p.signal {
            S::Log | S::Event | S::State => Col::Plain("severity"),
            _ => return unavailable(),
        },
        Field::SourceKind => match p.signal {
            S::Log | S::Event => Col::Plain("source_kind"),
            _ => return unavailable(),
        },
        Field::SourceVendor => match p.signal {
            S::Log | S::Event => Col::Plain("source_vendor"),
            _ => return unavailable(),
        },
        Field::Facility | Field::Body | Field::TraceId | Field::SpanId => match (p.signal, f) {
            (S::Log, Field::Facility) => Col::Plain("facility"),
            (S::Log, Field::Body) => Col::Plain("body"),
            (S::Log, Field::TraceId) => Col::Plain("trace_id"),
            (S::Log, Field::SpanId) => Col::Plain("span_id"),
            _ => return unavailable(),
        },
        Field::Metric | Field::Value | Field::Unit => match (p.signal, f) {
            (S::Metric, Field::Metric) => Col::Plain("metric"),
            (S::Metric, Field::Value) => Col::Plain("value"),
            (S::Metric, Field::Unit) => Col::Plain("unit"),
            _ => return unavailable(),
        },

        // A column of the rate subquery the compiler wraps the table in — see
        // `compile::rate_source`. It is only ever reachable on the base metrics table:
        // the branch above this one already refused every pre-aggregate, which is right,
        // because a rollup holds averages of a counter and the difference between two
        // averages is not a rate of anything.
        Field::Rate => match p.signal {
            S::Metric => Col::Plain("rate"),
            _ => return unavailable(),
        },
        Field::EventCategory | Field::EventType => match (p.signal, f) {
            (S::Event, Field::EventCategory) => Col::Plain("event_category"),
            (S::Event, Field::EventType) => Col::Plain("event_type"),
            _ => return unavailable(),
        },
        Field::PreviousStatus | Field::CurrentStatus => match (p.signal, f) {
            (S::State, Field::PreviousStatus) => Col::Plain("previous_status"),
            (S::State, Field::CurrentStatus) => Col::Plain("current_status"),
            _ => return unavailable(),
        },

        Field::Attr { key } => {
            if let Some(col) = materialised_column(key, p) {
                Col::Plain(col)
            } else {
                warnings.push(QueryWarning::AttributeNotMaterialised { key: key.clone() });
                Col::Attr {
                    map: p.attr_map,
                    key: key.clone(),
                }
            }
        }
    })
}

/// W1's expensive finding, in eight lines: a semconv key that the DDL materialises is a
/// real column, and must be read as one. The list lives in `uops_core` so that the
/// `ClickHouse` DDL and the query compiler cannot drift apart.
fn materialised_column(key: &str, p: &TablePlan) -> Option<&'static str> {
    // Only the log-shaped tables carry the MATERIALIZED columns; metric labels do not.
    if !matches!(p.signal, SignalType::Log | SignalType::Event) {
        return None;
    }
    match key {
        semconv::HOST_NAME => Some("host_name"),
        semconv::SERVICE_NAME => Some("service_name"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::{Expr, TimeRange};
    use chrono::{Duration, TimeZone, Utc};

    fn at(secs: i64) -> chrono::DateTime<Utc> {
        Utc.timestamp_opt(secs, 0).unwrap()
    }

    fn logs(span: Duration) -> Query {
        Query::new(
            SignalType::Log,
            TimeRange::new(at(0), at(span.num_seconds())),
        )
    }

    fn count() -> Aggregation {
        Aggregation {
            func: AggFunc::Count,
            field: None,
            alias: "c".into(),
        }
    }

    #[test]
    fn the_explorer_histogram_is_answered_from_the_pre_aggregate() {
        // W1: this exact shape re-renders on every search and every filter change, and
        // the p_by_time projection does not help it. If this test goes red, the
        // Explorer has silently regressed to a 1-second full scan per keystroke.
        let mut q = logs(Duration::days(1));
        q.aggregations = vec![count()];
        q.group_by = vec![Field::TimeBucket { seconds: 300 }];

        let (p, _) = plan(&q).unwrap();
        assert_eq!(p.table, "logs_counts_5m");
        assert_eq!(p.kind, TableKind::LogCounts);
    }

    #[test]
    fn a_histogram_finer_than_the_stored_bucket_falls_back_to_raw() {
        let mut q = logs(Duration::hours(1));
        q.aggregations = vec![count()];
        q.group_by = vec![Field::TimeBucket { seconds: 60 }];
        assert_eq!(plan(&q).unwrap().0.table, "logs");
    }

    #[test]
    fn a_histogram_filtered_on_text_cannot_use_the_pre_aggregate() {
        // The counts table has no body column. Getting this wrong would not be slow,
        // it would be wrong: counts unfiltered by the user's search term.
        let mut q = logs(Duration::days(1));
        q.aggregations = vec![count()];
        q.group_by = vec![Field::TimeBucket { seconds: 300 }];
        q.filter = Some(Expr::Text {
            field: Field::Body,
            mode: crate::ast::TextMode::AnyToken,
            terms: vec!["bgp".into()],
        });
        assert_eq!(plan(&q).unwrap().0.table, "logs");
    }

    #[test]
    fn metric_rollup_follows_the_window() {
        let mut q = Query::new(SignalType::Metric, TimeRange::new(at(0), at(0)));
        q.aggregations = vec![Aggregation {
            func: AggFunc::Avg,
            field: Some(Field::Value),
            alias: "v".into(),
        }];

        for (span, table) in [
            (Duration::hours(1), "metrics"),
            (Duration::days(7), "metrics_5m"),
            (Duration::days(90), "metrics_1h"),
        ] {
            q.time = TimeRange::new(at(0), at(span.num_seconds()));
            assert_eq!(plan(&q).unwrap().0.table, table, "span {span}");
        }
    }

    #[test]
    fn downsampling_is_never_silent() {
        let mut q = Query::new(
            SignalType::Metric,
            TimeRange::new(at(0), at(Duration::days(7).num_seconds())),
        );
        q.aggregations = vec![Aggregation {
            func: AggFunc::Avg,
            field: Some(Field::Value),
            alias: "v".into(),
        }];
        let (_, warnings) = plan(&q).unwrap();
        assert!(
            warnings.contains(&QueryWarning::Downsampled {
                table: "metrics_5m".into(),
                bucket_seconds: 300,
            }),
            "a 5-minute average plotted as if it were raw is a misread graph: {warnings:?}"
        );
    }

    #[test]
    fn raw_points_over_a_long_window_stay_on_the_raw_table() {
        // Not aggregate: there is nothing to downsample to.
        let q = Query::new(
            SignalType::Metric,
            TimeRange::new(at(0), at(Duration::days(90).num_seconds())),
        );
        assert_eq!(plan(&q).unwrap().0.table, "metrics");
    }

    #[test]
    fn a_quantile_over_a_long_window_errors_instead_of_lying() {
        let mut q = Query::new(
            SignalType::Metric,
            TimeRange::new(at(0), at(Duration::days(90).num_seconds())),
        );
        q.aggregations = vec![Aggregation {
            func: AggFunc::P95,
            field: Some(Field::Value),
            alias: "p95".into(),
        }];
        let err = plan(&q).unwrap_err();
        assert!(
            matches!(err, Error::RollupCannotServe { .. }),
            "a p95 of hourly averages is not a p95: {err}"
        );
    }

    #[test]
    fn grouping_on_a_label_over_a_long_window_errors() {
        let mut q = Query::new(
            SignalType::Metric,
            TimeRange::new(at(0), at(Duration::days(90).num_seconds())),
        );
        q.aggregations = vec![Aggregation {
            func: AggFunc::Avg,
            field: Some(Field::Value),
            alias: "v".into(),
        }];
        q.group_by = vec![Field::Attr {
            key: "interface".into(),
        }];
        assert!(matches!(
            plan(&q).unwrap_err(),
            Error::RollupCannotServe { .. }
        ));
    }

    #[test]
    fn materialised_attributes_become_real_columns() {
        let (p, _) = plan(&logs(Duration::hours(1))).unwrap();
        let mut w = Vec::new();
        let col = column_of(
            &Field::Attr {
                key: "host.name".into(),
            },
            &p,
            &mut w,
        )
        .unwrap();
        assert_eq!(col, Col::Plain("host_name"));
        assert!(w.is_empty(), "a materialised column is not slow: {w:?}");
    }

    #[test]
    fn an_unmaterialised_attribute_is_allowed_but_flagged() {
        let (p, _) = plan(&logs(Duration::hours(1))).unwrap();
        let mut w = Vec::new();
        let col = column_of(
            &Field::Attr {
                key: "custom.tag".into(),
            },
            &p,
            &mut w,
        )
        .unwrap();
        assert!(matches!(col, Col::Attr { .. }));
        assert_eq!(w.len(), 1, "the 2 252 ms case must warn");
    }

    #[test]
    fn a_field_from_another_signal_is_rejected() {
        let (p, _) = plan(&logs(Duration::hours(1))).unwrap();
        let err = column_of(&Field::Value, &p, &mut Vec::new()).unwrap_err();
        assert!(matches!(err, Error::FieldNotAvailable { .. }), "{err}");
    }

    #[test]
    fn traces_and_flows_are_declared_but_refuse_to_compile() {
        for s in [SignalType::Trace, SignalType::Flow] {
            let q = Query::new(s, TimeRange::new(at(0), at(60)));
            assert!(matches!(plan(&q).unwrap_err(), Error::Unsupported(_)));
        }
    }
}
