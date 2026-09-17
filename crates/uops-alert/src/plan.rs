//! What to ask `ClickHouse`, and how to read the answer.
//!
//! Pure, so the two things most likely to be quietly wrong are testable without a
//! database: the window a rule evaluates over, and which column of a result set is the
//! number being compared to the threshold.
//!
//! # The stored window's *span* is the rule; its position is provenance
//!
//! A `Query`'s time range is absolute — the AST has no relative windows, deliberately, so
//! that a compiled query is reproducible after the fact. A rule stored last Tuesday
//! therefore carries last Tuesday's window, and evaluating it literally would ask about
//! an afternoon that is over and never fire again.
//!
//! But the span is not incidental: *"avg over five minutes"* is how much data the
//! average is of, and it is the difference between a rule that reacts to a spike and one
//! that smooths it away. So evaluation keeps the span and moves the window to end at
//! *now*. That is the whole of [`evaluation_query`], and it is why a rule saved over
//! fifteen minutes behaves differently from the same rule saved over one.

use chrono::{DateTime, Duration, Utc};
use uops_core::ResourceId;
use uops_core::alert::Condition;
use uops_query::{AggFunc, Aggregation, Field, Query, SignalType, TimeRange};
use uops_store_ch::ResultSet;

/// The alias the evaluator gives the number it compares.
///
/// Fixed here rather than taken from the rule: a caller-chosen alias would be one more
/// string to validate as an identifier, and nothing downstream displays it.
pub const VALUE: &str = "v";

/// The shortest window any evaluation reads.
///
/// A rule stored with a degenerate window — zero span, or an absence of five seconds —
/// would otherwise compile to a range containing nothing at all: a rule that never fires
/// and never says why. One minute is the shortest window that can contain a sample from
/// anything this product polls, and it is the same floor for both kinds of rule so there
/// is one number to reason about rather than two that disagree.
pub const MIN_WINDOW: Duration = Duration::minutes(1);

/// [`MIN_WINDOW`] as seconds, for the arithmetic that compares ages.
pub const MIN_WINDOW_SECONDS: f64 = 60.0;

/// How many series one evaluation will read.
///
/// SPEC's scale target is a rule matching 5 000 resources. Ten thousand is the
/// compiler's own ceiling and is what a rule over a large estate with per-interface
/// labels will hit first; a rule that produces more series than this is one whose
/// grouping is wrong, and it will say so as a truncated evaluation rather than as a
/// silently partial one — see [`Series::truncated`].
pub const MAX_SERIES: u32 = uops_query::MAX_LIMIT;

/// One row of an evaluation: a resource, its labels, and the number to compare.
#[derive(Clone, Debug, PartialEq)]
pub struct Series {
    pub resource: ResourceId,
    /// Everything else the rule grouped by, in the order the columns arrived.
    pub labels: Vec<(String, String)>,
    pub value: f64,
}

/// What one evaluation read.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Reading {
    pub series: Vec<Series>,
    /// Whether the result set filled [`MAX_SERIES`].
    ///
    /// Reported rather than swallowed: an evaluation that silently read the first ten
    /// thousand of twelve thousand series is a rule that never fires for the other two
    /// thousand, and the only symptom is an alert that did not arrive.
    pub truncated: bool,
}

/// The query one evaluation runs.
///
/// **Threshold**: the rule's own query, grouped by `resource_id` in addition to whatever
/// it already grouped by, over a window of the same span ending now.
///
/// **Absence**: the rule's selector and signal, asking only when each resource was last
/// heard from. The rule's own aggregation — if somebody wrote one — is replaced, because
/// the question an absence rule asks is not about values at all.
#[must_use]
pub fn evaluation_query(query: &Query, condition: Condition, now: DateTime<Utc>) -> Query {
    match condition {
        Condition::Threshold { .. } => {
            // A rule with no aggregation of its own alerts on how many rows its search
            // returns. That is what a saved Log Explorer search *is* — "the rows matching
            // this" — and supplying the count here is what lets one convert to a rule
            // without being edited first. A rule that brought its own aggregate keeps it.
            let aggregations = if query.aggregations.is_empty() {
                vec![Aggregation {
                    func: AggFunc::Count,
                    field: None,
                    alias: VALUE.to_owned(),
                }]
            } else {
                query.aggregations.clone()
            };

            let mut group_by = vec![Field::ResourceId];
            // The rule's own grouping comes after the resource, so the label columns keep
            // the order the rule wrote them in and a reader of the result set can match
            // them up by position.
            group_by.extend(
                query
                    .group_by
                    .iter()
                    .filter(|f| **f != Field::ResourceId)
                    .cloned(),
            );

            Query {
                time: window(query.time.span(), now),
                aggregations,
                group_by,
                order_by: Vec::new(),
                limit: MAX_SERIES,
                offset: 0,
                ..query.clone()
            }
        }

        Condition::Absence { after_seconds } => {
            let after = Duration::seconds(i64::from(after_seconds));
            Query {
                time: window(after, now),
                // The newest timestamp per resource. `max(observed_at)` over the window
                // is the cheapest form of "is anything still arriving" the schema can
                // answer, and it reads the sort key rather than any column of data.
                aggregations: vec![Aggregation {
                    func: AggFunc::Max,
                    field: Some(Field::ObservedAt),
                    alias: VALUE.to_owned(),
                }],
                group_by: vec![Field::ResourceId],
                order_by: Vec::new(),
                limit: MAX_SERIES,
                offset: 0,
                ..query.clone()
            }
        }
    }
}

fn window(span: Duration, now: DateTime<Utc>) -> TimeRange {
    TimeRange::new(now - span.max(MIN_WINDOW), now)
}

/// Read a threshold evaluation's result set.
///
/// The column layout is the compiler's: `group_by` columns in order, then the
/// aggregations. So column 0 is the resource, the last is the value, and everything
/// between is a label. Reading by position rather than by name because an aggregation's
/// alias is the only name here that a rule could have chosen, and the positions are what
/// `uops-query`'s golden tests pin.
///
/// Rows whose value is null are skipped: `avg()` over a window with no samples is null,
/// which means "no data" rather than zero — and comparing a nonexistent value to a
/// threshold is how an interface that stopped reporting becomes an interface at 0%.
#[must_use]
pub fn read_threshold(result: &ResultSet) -> Reading {
    let width = result.columns.len();
    let mut series = Vec::with_capacity(result.rows.len());

    for row in &result.rows {
        if width < 2 || row.len() < width {
            continue;
        }
        let Some(resource) = uuid(&row[0]) else {
            continue;
        };
        let Some(value) = number(&row[width - 1]) else {
            continue;
        };

        let labels = (1..width - 1)
            .map(|i| {
                (
                    result.columns[i].name.clone(),
                    text(&row[i]).unwrap_or_default(),
                )
            })
            .collect();

        series.push(Series {
            resource: ResourceId::from_uuid(resource),
            labels,
            value,
        });
    }

    Reading {
        truncated: result.rows.len() >= MAX_SERIES as usize,
        series,
    }
}

/// Read an absence evaluation, as the age in seconds of each resource's newest sample.
///
/// The value is an age rather than a timestamp because that is what the condition
/// compares — `Condition::Absence` tests "older than `after_seconds`" — and converting
/// here keeps the comparison in one place.
///
/// Resources that produced no row at all are **not** in the result: nothing arrived from
/// them in the whole window, which is the definition of absent. The caller supplies the
/// list of resources that were expected and treats the difference as absent; this
/// function cannot, because it has never seen that list.
#[must_use]
pub fn read_absence(result: &ResultSet, now: DateTime<Utc>) -> Reading {
    let mut series = Vec::with_capacity(result.rows.len());

    for row in &result.rows {
        if row.len() < 2 {
            continue;
        }
        let Some(resource) = uuid(&row[0]) else {
            continue;
        };
        let Some(last_seen) = timestamp(&row[1]) else {
            continue;
        };

        series.push(Series {
            resource: ResourceId::from_uuid(resource),
            labels: Vec::new(),
            // Clamped at zero: a device whose clock is ahead produces a sample in the
            // future, and a negative age would read as "seen in a moment". Milliseconds
            // rather than seconds because a sub-second age is the common case for a
            // healthy device, and rounding it to zero would be fine — but rounding a
            // 59.6-second age down to 59 is how an absence rule fires a second late for
            // no reason anybody can see.
            value: f64::from(
                u32::try_from((now - last_seen).num_milliseconds().max(0)).unwrap_or(u32::MAX),
            ) / 1000.0,
        });
    }

    Reading {
        truncated: result.rows.len() >= MAX_SERIES as usize,
        series,
    }
}

/// Which signal a rule reads, for the error messages the evaluator writes.
#[must_use]
pub const fn signal_of(query: &Query) -> SignalType {
    query.signal
}

fn uuid(value: &serde_json::Value) -> Option<uuid::Uuid> {
    value.as_str()?.parse().ok()
}

fn text(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::String(s) => Some(s.clone()),
        serde_json::Value::Null => None,
        other => Some(other.to_string()),
    }
}

/// A number, however `ClickHouse` chose to encode it.
///
/// 64-bit integers arrive quoted when `output_format_json_quote_64bit_integers` is on,
/// which is the default, and unquoted otherwise — and which of the two you get depends on
/// the aggregate's result type. Reading only one of them makes every value absent, which
/// looks exactly like an empty table.
fn number(value: &serde_json::Value) -> Option<f64> {
    value
        .as_f64()
        .or_else(|| value.as_str().and_then(|s| s.parse().ok()))
}

/// A `DateTime64(3)` as `ClickHouse` sends it: `2026-09-17 10:00:00.000`.
fn timestamp(value: &serde_json::Value) -> Option<DateTime<Utc>> {
    let text = value.as_str()?;
    chrono::NaiveDateTime::parse_from_str(text, "%Y-%m-%d %H:%M:%S%.f")
        .ok()
        .map(|naive| naive.and_utc())
}

#[cfg(test)]
// These compare values that were parsed from a literal and never arithmetic'd, so the
// bit patterns are exactly the ones written down. The lint is right in general and
// approximate comparison here would only hide a parser that returned the wrong number.
#[allow(clippy::float_cmp)]
mod tests {
    use super::*;
    use uops_core::alert::Comparison;
    use uops_query::ResourceSelector;
    use uops_store_ch::Column;

    fn at(text: &str) -> DateTime<Utc> {
        text.parse().expect("a timestamp")
    }

    fn rule_query(span_minutes: i64) -> Query {
        let end = at("2026-09-01T12:00:00Z");
        Query {
            aggregations: vec![Aggregation {
                func: AggFunc::Avg,
                field: Some(Field::Value),
                alias: VALUE.to_owned(),
            }],
            ..Query::new(
                SignalType::Metric,
                TimeRange::new(end - Duration::minutes(span_minutes), end),
            )
        }
    }

    const THRESHOLD: Condition = Condition::Threshold {
        op: Comparison::Gt,
        value: 90.0,
        hold_seconds: 300,
    };

    #[test]
    fn evaluation_keeps_the_rules_span_and_moves_its_window_to_now() {
        // The span is the rule — "avg over five minutes" is how much data the average is
        // of. The position is provenance: a rule stored last Tuesday must not keep asking
        // about last Tuesday afternoon.
        let now = at("2026-09-17T09:30:00Z");
        let q = evaluation_query(&rule_query(5), THRESHOLD, now);

        assert_eq!(q.time.end, now);
        assert_eq!(q.time.span(), Duration::minutes(5));
    }

    #[test]
    fn an_evaluation_groups_by_resource_so_every_device_is_its_own_alert() {
        // Without this, a rule over a hundred routers produces one number and one alert,
        // and the operator is told "something is above 90" about an estate.
        let q = evaluation_query(&rule_query(5), THRESHOLD, at("2026-09-17T09:30:00Z"));
        assert_eq!(q.group_by, vec![Field::ResourceId]);
    }

    #[test]
    fn a_rules_own_grouping_survives_and_stays_in_order() {
        // Per-interface rules are the common case, and the label columns are read by
        // position — so the resource leads and the rule's own grouping follows it
        // unchanged.
        let mut rule = rule_query(5);
        rule.group_by = vec![
            Field::Attr {
                key: "interface".into(),
            },
            Field::ResourceId,
        ];

        let q = evaluation_query(&rule, THRESHOLD, at("2026-09-17T09:30:00Z"));
        assert_eq!(
            q.group_by,
            vec![
                Field::ResourceId,
                Field::Attr {
                    key: "interface".into()
                }
            ],
            "the resource is added once, not twice"
        );
    }

    #[test]
    fn an_absence_evaluation_asks_when_each_resource_was_last_heard_from() {
        let now = at("2026-09-17T09:30:00Z");
        let mut rule = rule_query(60);
        rule.resources = ResourceSelector::Kind {
            kind: uops_core::ResourceKind::Device,
        };

        let q = evaluation_query(&rule, Condition::Absence { after_seconds: 300 }, now);

        // The rule's aggregation is replaced: the question is not about values.
        assert_eq!(q.aggregations.len(), 1);
        assert_eq!(q.aggregations[0].func, AggFunc::Max);
        assert_eq!(q.aggregations[0].field, Some(Field::ObservedAt));
        // The window is the absence itself, not the rule's stored span.
        assert_eq!(q.time.span(), Duration::seconds(300));
        assert_eq!(q.resources, rule.resources, "the selector is the rule's");
    }

    #[test]
    fn an_absurdly_small_absence_window_is_floored() {
        // "Nothing for five seconds" is not answerable by a store whose samples arrive
        // every thirty. One floor for both kinds of rule, so there is one number to
        // reason about rather than two that disagree.
        let q = evaluation_query(
            &rule_query(5),
            Condition::Absence { after_seconds: 1 },
            at("2026-09-17T09:30:00Z"),
        );
        assert_eq!(q.time.span(), MIN_WINDOW);
    }

    #[test]
    fn a_rule_with_a_degenerate_window_still_reads_something() {
        // A stored window of zero would compile to an empty range: a rule that never
        // fires and never says why.
        let instant = at("2026-09-01T12:00:00Z");
        let mut rule = rule_query(5);
        rule.time = TimeRange::new(instant, instant);

        let q = evaluation_query(&rule, THRESHOLD, at("2026-09-17T09:30:00Z"));
        assert_eq!(q.time.span(), Duration::minutes(1));
    }

    fn result(columns: &[&str], rows: Vec<Vec<serde_json::Value>>) -> ResultSet {
        ResultSet {
            columns: columns
                .iter()
                .map(|name| Column {
                    name: (*name).to_owned(),
                    ty: "String".to_owned(),
                })
                .collect(),
            rows,
            table: "metrics",
            warnings: Vec::new(),
            rows_read: 0,
            bytes_read: 0,
        }
    }

    const RTR: &str = "018f0000-0000-7000-8000-0000000000aa";

    #[test]
    fn a_threshold_result_reads_as_resource_labels_and_value() {
        let reading = read_threshold(&result(
            &["resource_id", "interface", "v"],
            vec![vec![RTR.into(), "Gi0/1".into(), serde_json::json!(94.5)]],
        ));

        assert_eq!(reading.series.len(), 1);
        assert_eq!(reading.series[0].value, 94.5);
        assert_eq!(
            reading.series[0].labels,
            vec![("interface".to_owned(), "Gi0/1".to_owned())]
        );
        assert!(!reading.truncated);
    }

    #[test]
    fn a_quoted_integer_is_still_a_number() {
        // ClickHouse quotes 64-bit integers by default. Reading only the unquoted form
        // makes every value absent, which looks exactly like an empty table.
        let reading = read_threshold(&result(
            &["resource_id", "v"],
            vec![vec![RTR.into(), "42".into()]],
        ));
        assert_eq!(reading.series[0].value, 42.0);
    }

    #[test]
    fn a_null_value_is_no_data_rather_than_zero() {
        // `avg()` over a window with no samples is null. Comparing that to a threshold is
        // how an interface that stopped reporting becomes an interface at 0% — and `< 10`
        // rules fire for every device that went quiet.
        let reading = read_threshold(&result(
            &["resource_id", "v"],
            vec![
                vec![RTR.into(), serde_json::Value::Null],
                vec![RTR.into(), serde_json::json!(5.0)],
            ],
        ));
        assert_eq!(reading.series.len(), 1, "{:?}", reading.series);
        assert_eq!(reading.series[0].value, 5.0);
    }

    #[test]
    fn an_absence_result_reads_as_an_age_in_seconds() {
        let now = at("2026-09-17T09:30:00Z");
        let reading = read_absence(
            &result(
                &["resource_id", "v"],
                vec![vec![RTR.into(), "2026-09-17 09:25:00.000".into()]],
            ),
            now,
        );

        assert_eq!(reading.series[0].value, 300.0);
    }

    #[test]
    fn a_device_whose_clock_is_ahead_is_not_seen_in_a_moment() {
        let now = at("2026-09-17T09:30:00Z");
        let reading = read_absence(
            &result(
                &["resource_id", "v"],
                vec![vec![RTR.into(), "2026-09-17 09:35:00.000".into()]],
            ),
            now,
        );

        assert_eq!(reading.series[0].value, 0.0);
    }

    #[test]
    fn a_search_with_no_aggregation_alerts_on_how_many_rows_it_returns() {
        // What makes "a saved search converts to an alert rule with no edits" literally
        // true. A Log Explorer search is "the rows matching this"; alerting on it means
        // alerting on how many there are.
        let rows = Query::new(
            SignalType::Log,
            TimeRange::new(at("2026-09-01T11:45:00Z"), at("2026-09-01T12:00:00Z")),
        );

        let q = evaluation_query(&rows, THRESHOLD, at("2026-09-17T09:30:00Z"));
        assert_eq!(q.aggregations.len(), 1);
        assert_eq!(q.aggregations[0].func, AggFunc::Count);
        assert_eq!(q.aggregations[0].field, None);
        assert_eq!(q.group_by, vec![Field::ResourceId], "one alert per device");
    }

    #[test]
    fn a_rule_that_brought_its_own_aggregate_keeps_it() {
        let q = evaluation_query(&rule_query(5), THRESHOLD, at("2026-09-17T09:30:00Z"));
        assert_eq!(q.aggregations, rule_query(5).aggregations);
    }
}
