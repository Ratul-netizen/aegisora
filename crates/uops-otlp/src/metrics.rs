//! `ExportMetricsServiceRequest` → [`MetricRow`].
//!
//! # What is converted, and what is not
//!
//! | OTLP type | here |
//! |---|---|
//! | Gauge | ✅ one row per data point |
//! | Sum | ✅ one row per data point, monotonicity recorded |
//! | Histogram, `ExponentialHistogram`, Summary | ⬜ counted and dropped |
//!
//! Gauge and Sum are what the `OTel` Collector's `hostmetrics` receiver actually emits, and
//! `hostmetrics` is what SPEC §M3's acceptance criterion names — *"`OTel` Collector on Linux
//! and Windows sends host metrics + logs end to end"*. So the two that are converted are
//! the two the criterion needs.
//!
//! The three that are not need a decision first, and it is a storage decision rather than
//! a parsing one: the `metrics` table holds one `value: f64` per row, and a histogram is
//! a bucket array plus a sum plus a count. Flattening one into `metric.bucket.le_0_5`
//! rows is what Prometheus does and it works, but it multiplies the row count by the
//! bucket count and it makes the rollup in `metrics_1h` wrong — averaging a bucket
//! boundary is meaningless. That belongs with M4's dashboards, where somebody will
//! actually query it.
//!
//! Dropping them **quietly** would be the mistake. They are counted, so an operator whose
//! latency histograms are missing finds a number rather than an absence.
//!
//! # Counters are stored as they arrive
//!
//! A Sum arrives as a cumulative counter and is stored as one. Not converted to a rate
//! here, for the same reason the SNMP path does not: a rate computed at ingest is wrong
//! across a restart, wrong at the first sample, and impossible to re-derive at a different
//! window. `uops-query` computes rates in `ClickHouse` at query time over whatever window
//! was asked for, and that is where the counter-wrap guard already lives.

use std::collections::BTreeMap;

use opentelemetry_proto::tonic::metrics::v1::number_data_point::Value as PointValue;
use opentelemetry_proto::tonic::metrics::v1::{ResourceMetrics, metric};
use uops_pipeline::Attribution;
use uops_store_ch::MetricRow;

use crate::{attributes, instant, resource_attributes};

/// What one request produced, and what it could not.
#[derive(Debug, Default)]
pub struct Converted {
    pub rows: Vec<MetricRow>,
    /// Data points in a type that is not converted yet — histograms and summaries.
    ///
    /// Counted rather than silently skipped: an operator whose latency histograms never
    /// appear should find a number, not an absence.
    pub unsupported: u64,
}

/// One resource's metrics, before resolution.
#[derive(Debug)]
pub struct Batch<'a> {
    pub resource: BTreeMap<String, String>,
    pub metrics: Vec<&'a opentelemetry_proto::tonic::metrics::v1::Metric>,
}

/// Group a request by resource, which is the unit identity resolution works on.
#[must_use]
pub fn batches(request: &[ResourceMetrics]) -> Vec<Batch<'_>> {
    request
        .iter()
        .map(|rm| Batch {
            resource: resource_attributes(rm.resource.as_ref()),
            metrics: rm
                .scope_metrics
                .iter()
                .flat_map(|sm| sm.metrics.iter())
                .collect(),
        })
        .collect()
}

/// Turn one resource's metrics into rows.
#[must_use]
pub fn to_rows(
    batch: &Batch<'_>,
    attribution: &Attribution,
    received_at: chrono::DateTime<chrono::Utc>,
) -> Converted {
    let mut out = Converted::default();

    for metric in &batch.metrics {
        let points = match metric.data.as_ref() {
            Some(metric::Data::Gauge(g)) => &g.data_points,
            Some(metric::Data::Sum(s)) => &s.data_points,
            // Counted, not dropped quietly. See the module docs.
            Some(
                metric::Data::Histogram(_)
                | metric::Data::ExponentialHistogram(_)
                | metric::Data::Summary(_),
            )
            | None => {
                out.unsupported += 1;
                continue;
            }
        };

        let monotonic = match metric.data.as_ref() {
            Some(metric::Data::Sum(s)) => Some(s.is_monotonic),
            _ => None,
        };

        for point in points {
            let Some(observed_at) = instant(point.time_unix_nano) else {
                // A metric with no timestamp is not a metric. Unlike a log, where the
                // text is still worth keeping, a data point whose instant is unknown
                // cannot be plotted, rated or compared — and putting it at the receipt
                // time would invent a measurement.
                out.unsupported += 1;
                continue;
            };

            let value = match point.value {
                Some(PointValue::AsDouble(d)) => d,
                #[allow(clippy::cast_precision_loss)]
                Some(PointValue::AsInt(i)) => i as f64,
                None => continue,
            };

            // The data point's own attributes are the series labels — `cpu`, `state`,
            // `device`. The resource's are not: they identify the *resource*, which is
            // already the row's `resource_id`, and repeating them in every label set
            // would multiply the cardinality of the sort key by the size of the estate.
            let mut labels = attributes(&point.attributes);
            if let Some(monotonic) = monotonic {
                // Which the rate computation needs: a non-monotonic Sum is a value that
                // may legitimately go down, and the counter-wrap guard must not treat
                // that as a wrap.
                labels.insert("otlp.monotonic".to_owned(), monotonic.to_string());
            }

            out.rows.push(MetricRow {
                tenant_id: attribution.tenant_id,
                resource_id: attribution.resource_id,
                site_id: attribution.site_id,
                metric: metric.name.clone(),
                observed_at,
                ingested_at: received_at,
                value,
                unit: metric.unit.clone(),
                labels,
            });
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use opentelemetry_proto::tonic::common::v1::{AnyValue, KeyValue, any_value};
    use opentelemetry_proto::tonic::metrics::v1::{
        Gauge, Histogram, HistogramDataPoint, Metric, NumberDataPoint, ScopeMetrics, Sum,
    };
    use opentelemetry_proto::tonic::resource::v1::Resource;

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

    fn point(value: f64, labels: &[(&str, &str)]) -> NumberDataPoint {
        NumberDataPoint {
            time_unix_nano: 1_700_000_000_000_000_000,
            value: Some(PointValue::AsDouble(value)),
            attributes: labels
                .iter()
                .map(|(k, v)| KeyValue {
                    key: (*k).to_owned(),
                    value: Some(AnyValue {
                        value: Some(any_value::Value::StringValue((*v).to_owned())),
                    }),
                    ..KeyValue::default()
                })
                .collect(),
            ..NumberDataPoint::default()
        }
    }

    fn request(metrics: Vec<Metric>) -> Vec<ResourceMetrics> {
        vec![ResourceMetrics {
            resource: Some(Resource {
                attributes: vec![KeyValue {
                    key: uops_core::semconv::HOST_NAME.to_owned(),
                    value: Some(AnyValue {
                        value: Some(any_value::Value::StringValue("app-01".to_owned())),
                    }),
                    ..KeyValue::default()
                }],
                ..Resource::default()
            }),
            scope_metrics: vec![ScopeMetrics {
                metrics,
                ..ScopeMetrics::default()
            }],
            ..ResourceMetrics::default()
        }]
    }

    #[test]
    fn a_gauge_becomes_one_row_per_data_point() {
        // What hostmetrics emits, and what SPEC's acceptance criterion needs.
        let request = request(vec![Metric {
            name: "system.cpu.utilization".to_owned(),
            unit: "1".to_owned(),
            data: Some(metric::Data::Gauge(Gauge {
                data_points: vec![
                    point(0.42, &[("cpu", "0"), ("state", "user")]),
                    point(0.17, &[("cpu", "1"), ("state", "user")]),
                ],
            })),
            ..Metric::default()
        }]);

        let converted = to_rows(
            &batches(&request)[0],
            &attribution(),
            at("2026-09-17T10:00:00Z"),
        );
        assert_eq!(converted.rows.len(), 2);
        assert_eq!(converted.unsupported, 0);
        assert_eq!(converted.rows[0].metric, "system.cpu.utilization");
        assert!((converted.rows[0].value - 0.42).abs() < f64::EPSILON);
        assert_eq!(converted.rows[0].unit, "1");
        assert_eq!(
            converted.rows[0].labels.get("cpu").map(String::as_str),
            Some("0")
        );
    }

    #[test]
    fn the_resources_attributes_are_not_repeated_as_labels() {
        // They identify the resource, which the row already carries as resource_id.
        // Repeating them would multiply the cardinality of the ClickHouse sort key by the
        // size of the estate — the exact mistake the W1 benchmark warned about for
        // high-cardinality attribute grouping.
        let request = request(vec![Metric {
            name: "system.memory.usage".to_owned(),
            data: Some(metric::Data::Gauge(Gauge {
                data_points: vec![point(1.0, &[("state", "used")])],
            })),
            ..Metric::default()
        }]);

        let converted = to_rows(
            &batches(&request)[0],
            &attribution(),
            at("2026-09-17T10:00:00Z"),
        );
        assert_eq!(converted.rows[0].labels.len(), 1);
        assert!(
            !converted.rows[0]
                .labels
                .contains_key(uops_core::semconv::HOST_NAME)
        );
    }

    #[test]
    fn a_counter_is_stored_as_a_counter_and_says_whether_it_can_go_down() {
        // Not converted to a rate here: a rate computed at ingest is wrong across a
        // restart, wrong at the first sample, and impossible to re-derive at another
        // window. uops-query does it in ClickHouse, where the counter-wrap guard is --
        // and that guard needs to know that a non-monotonic Sum going down is not a wrap.
        let request = request(vec![Metric {
            name: "system.network.io".to_owned(),
            unit: "By".to_owned(),
            data: Some(metric::Data::Sum(Sum {
                data_points: vec![point(4_294_967_295.0, &[("direction", "receive")])],
                is_monotonic: true,
                ..Sum::default()
            })),
            ..Metric::default()
        }]);

        let converted = to_rows(
            &batches(&request)[0],
            &attribution(),
            at("2026-09-17T10:00:00Z"),
        );
        assert_eq!(converted.rows.len(), 1);
        assert!((converted.rows[0].value - 4_294_967_295.0).abs() < f64::EPSILON);
        assert_eq!(
            converted.rows[0]
                .labels
                .get("otlp.monotonic")
                .map(String::as_str),
            Some("true")
        );
    }

    #[test]
    fn a_histogram_is_counted_rather_than_dropped_quietly() {
        // The table holds one f64 per row and a histogram is a bucket array. Flattening
        // it is an M4 decision about the rollup, not a parsing one -- but an operator
        // whose latency histograms never appear must find a number rather than an
        // absence.
        let request = request(vec![Metric {
            name: "http.server.duration".to_owned(),
            data: Some(metric::Data::Histogram(Histogram {
                data_points: vec![HistogramDataPoint::default()],
                ..Histogram::default()
            })),
            ..Metric::default()
        }]);

        let converted = to_rows(
            &batches(&request)[0],
            &attribution(),
            at("2026-09-17T10:00:00Z"),
        );
        assert!(converted.rows.is_empty());
        assert_eq!(converted.unsupported, 1);
    }

    #[test]
    fn a_data_point_with_no_timestamp_is_not_invented() {
        // Unlike a log, where the text is still worth keeping, a data point whose instant
        // is unknown cannot be plotted, rated or compared. Putting it at the receipt time
        // would invent a measurement.
        let mut p = point(1.0, &[]);
        p.time_unix_nano = 0;

        let request = request(vec![Metric {
            name: "x".to_owned(),
            data: Some(metric::Data::Gauge(Gauge {
                data_points: vec![p],
            })),
            ..Metric::default()
        }]);

        let converted = to_rows(
            &batches(&request)[0],
            &attribution(),
            at("2026-09-17T10:00:00Z"),
        );
        assert!(converted.rows.is_empty());
        assert_eq!(converted.unsupported, 1);
    }

    #[test]
    fn an_integer_point_is_the_same_row_as_a_double_one() {
        // hostmetrics sends both, and a consumer should not have to know which.
        let mut p = point(0.0, &[]);
        p.value = Some(PointValue::AsInt(42));

        let request = request(vec![Metric {
            name: "system.filesystem.usage".to_owned(),
            data: Some(metric::Data::Gauge(Gauge {
                data_points: vec![p],
            })),
            ..Metric::default()
        }]);

        let converted = to_rows(
            &batches(&request)[0],
            &attribution(),
            at("2026-09-17T10:00:00Z"),
        );
        assert_eq!(converted.rows.len(), 1);
        assert!((converted.rows[0].value - 42.0).abs() < f64::EPSILON);
    }
}
