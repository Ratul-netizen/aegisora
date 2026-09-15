//! Turning what a device said into what gets stored.
//!
//! An SNMP response is OIDs and ASN.1 values; a metric row is a name, a number, a unit
//! and some labels. The gap between them is where three decisions live.
//!
//! # A counter is stored raw
//!
//! SPEC §M2: *"Store the raw counter; compute rates at query time from the raw series.
//! Storing pre-computed rates makes re-interpretation impossible."* So a counter's value
//! goes in as the number the agent returned — not a delta, not a rate. The rate is
//! [`crate::counter`]'s job and it happens when somebody asks a question, which is also
//! the only time anyone knows what window they want it over.
//!
//! # A value that is not a number is dropped, not coerced
//!
//! `ifName` is an `OCTET STRING` and is not a metric. Neither is an OID, nor
//! `NoSuchInstance`. A conversion that turned those into zero would put a real-looking
//! number on a graph for a metric the device does not implement — which is worse than a
//! gap, because a gap is visible and a zero is not. They are skipped, and the caller is
//! told how many, so "this profile collects nothing on this device" is answerable.
//!
//! # The interface index becomes a label, not part of the name
//!
//! `network.io.receive` on interface 3 is the same metric as on interface 4, with a
//! different label. Encoding the index into the name — `network.io.receive.3` — would
//! make every interface its own series, break every aggregation across a device, and
//! produce a cardinality explosion in the name rather than in a place `ClickHouse` is
//! built to handle.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use uops_core::{ResourceId, SiteId, TenantId};
use uops_profile::Oid;
use uops_store_ch::MetricRow;

use crate::plan::MetricRequest;

/// What a device is, for the purpose of labelling its samples.
#[derive(Clone, Copy, Debug)]
pub struct Subject {
    pub tenant: TenantId,
    pub resource: ResourceId,
    /// `resource.site_id` is nullable and `MetricRow.site_id` is not. The nil uuid is
    /// what "no site" means on the telemetry side; converting here rather than at every
    /// call site keeps one answer to it.
    pub site: Option<SiteId>,
}

impl Subject {
    fn site_or_nil(self) -> SiteId {
        self.site
            .unwrap_or_else(|| SiteId::from_uuid(uuid::Uuid::nil()))
    }
}

/// One reading, as the transport gave it.
#[derive(Clone, Debug, PartialEq)]
pub struct Reading {
    pub oid: Oid,
    pub value: Numeric,
}

/// A value that can be a metric.
///
/// Deliberately not every SNMP type — the ones a number can be read from. The transport
/// decides which of its values qualify; this is what survives that.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Numeric {
    Signed(i64),
    Unsigned(u64),
}

impl Numeric {
    /// As `f64`, which is what `MetricRow` holds.
    ///
    /// A `Counter64` beyond 2^53 loses low bits here. That is real and it is the right
    /// trade: the alternative is a second storage column for a case that arises on
    /// counters which have been running for years, and the rate computed from two such
    /// samples is unaffected because the *difference* is small.
    #[must_use]
    #[allow(clippy::cast_precision_loss)]
    pub fn as_f64(self) -> f64 {
        match self {
            Self::Signed(n) => n as f64,
            Self::Unsigned(n) => n as f64,
        }
    }
}

/// What a batch of readings produced.
#[derive(Debug, Default)]
pub struct Batch {
    pub rows: Vec<MetricRow>,
    /// Readings that matched a metric but held nothing numeric. Counted rather than
    /// silently dropped — see the module docs.
    pub skipped: usize,
}

/// Convert scalar readings into rows.
///
/// A scalar metric's OID is exact, except that agents commonly answer `x.0` for a
/// request for `x`. Both are matched, because a profile author writes whichever the MIB
/// document shows and neither is wrong.
#[must_use]
pub fn scalars(
    subject: Subject,
    metrics: &[MetricRequest],
    readings: &[Reading],
    observed_at: DateTime<Utc>,
) -> Batch {
    let mut batch = Batch::default();

    for metric in metrics {
        let found = readings
            .iter()
            .find(|r| r.oid == metric.oid || is_instance_zero(&r.oid, &metric.oid));

        match found {
            Some(reading) => batch.rows.push(MetricRow {
                tenant_id: subject.tenant,
                resource_id: subject.resource,
                site_id: subject.site_or_nil(),
                metric: metric.name.clone(),
                observed_at,
                ingested_at: Utc::now(),
                value: reading.value.as_f64(),
                unit: metric.unit.clone(),
                labels: BTreeMap::new(),
            }),
            // Not an error: a profile is written for a family of devices and any one of
            // them may not implement every OID in it.
            None => batch.skipped += 1,
        }
    }

    batch
}

/// True when `candidate` is `base` with a `.0` instance suffix.
fn is_instance_zero(candidate: &Oid, base: &Oid) -> bool {
    candidate.len() == base.len() + 1
        && candidate.starts_with(base)
        && candidate.arcs().last() == Some(&0)
}

/// Convert an interface column walk into rows, one per interface.
///
/// `names` maps an interface index to its name, from the discovery walk. An index with
/// no name still produces a row — labelled by index alone — because a counter from an
/// interface whose name could not be read is still a real measurement, and dropping it
/// would make a device look partly dead.
#[must_use]
pub fn interface_columns(
    subject: Subject,
    metric: &MetricRequest,
    readings: &[Reading],
    names: &BTreeMap<Vec<u32>, String>,
    observed_at: DateTime<Utc>,
) -> Batch {
    let mut batch = Batch::default();
    let ingested_at = Utc::now();

    for reading in readings {
        let Some(index) = suffix(&reading.oid, &metric.oid) else {
            // A reading from outside the column asked for. The walk should not produce
            // these — see uops_snmp::walk's boundary stop — so counting it is a check on
            // that rather than an expected case.
            batch.skipped += 1;
            continue;
        };

        let mut labels = BTreeMap::new();
        labels.insert(
            "network.interface.index".to_owned(),
            index
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("."),
        );
        if let Some(name) = names.get(&index) {
            // OTel semconv. The name is what a person reads on a graph; the index is
            // what joins it back to the device.
            labels.insert("network.interface.name".to_owned(), name.clone());
        }

        batch.rows.push(MetricRow {
            tenant_id: subject.tenant,
            resource_id: subject.resource,
            site_id: subject.site_or_nil(),
            metric: metric.name.clone(),
            observed_at,
            ingested_at,
            value: reading.value.as_f64(),
            unit: metric.unit.clone(),
            labels,
        });
    }

    batch
}

/// The arcs of `oid` beneath `column`, or `None` if it is not beneath it.
fn suffix(oid: &Oid, column: &Oid) -> Option<Vec<u32>> {
    if !oid.starts_with(column) || oid.len() == column.len() {
        return None;
    }
    Some(oid.arcs()[column.len()..].to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn oid(s: &str) -> Oid {
        s.parse().unwrap()
    }

    fn subject() -> Subject {
        Subject {
            tenant: TenantId::new(),
            resource: ResourceId::new(),
            site: None,
        }
    }

    fn metric(name: &str, o: &str, unit: &str, counter: bool) -> MetricRequest {
        MetricRequest {
            name: name.to_owned(),
            oid: oid(o),
            unit: unit.to_owned(),
            counter,
        }
    }

    fn now() -> DateTime<Utc> {
        Utc::now()
    }

    #[test]
    fn a_scalar_becomes_one_row() {
        let m = metric("system.uptime", "1.3.6.1.2.1.1.3.0", "s", false);
        let readings = vec![Reading {
            oid: oid("1.3.6.1.2.1.1.3.0"),
            value: Numeric::Unsigned(123_456),
        }];

        let batch = scalars(subject(), std::slice::from_ref(&m), &readings, now());
        assert_eq!(batch.rows.len(), 1);
        assert_eq!(batch.skipped, 0);
        assert!((batch.rows[0].value - 123_456.0).abs() < f64::EPSILON);
        assert_eq!(batch.rows[0].unit, "s");
        assert_eq!(batch.rows[0].metric, "system.uptime");
    }

    #[test]
    fn an_agent_answering_x_dot_zero_for_x_still_matches() {
        // A profile author writes whichever form the MIB document shows, and agents
        // answer the instance. Neither is wrong and a strict comparison would silently
        // collect nothing.
        let m = metric("system.uptime", "1.3.6.1.2.1.1.3", "s", false);
        let readings = vec![Reading {
            oid: oid("1.3.6.1.2.1.1.3.0"),
            value: Numeric::Unsigned(7),
        }];
        assert_eq!(scalars(subject(), &[m], &readings, now()).rows.len(), 1);
    }

    #[test]
    fn a_metric_the_device_does_not_implement_is_counted_not_invented() {
        // A profile covers a family of devices and any one of them may lack an OID. A
        // zero here would be a real-looking number on a graph for something that was
        // never measured.
        let metrics = vec![
            metric("system.uptime", "1.3.6.1.2.1.1.3.0", "s", false),
            metric(
                "hardware.temperature",
                "1.3.6.1.4.1.14988.1.1.3.10.0",
                "Cel",
                false,
            ),
        ];
        let readings = vec![Reading {
            oid: oid("1.3.6.1.2.1.1.3.0"),
            value: Numeric::Unsigned(1),
        }];

        let batch = scalars(subject(), &metrics, &readings, now());
        assert_eq!(batch.rows.len(), 1);
        assert_eq!(batch.skipped, 1, "the missing one must be counted");
        assert!(
            batch
                .rows
                .iter()
                .all(|r| r.metric != "hardware.temperature"),
            "no row for an OID the device did not answer"
        );
    }

    #[test]
    fn an_interface_column_becomes_one_row_per_interface_with_the_index_as_a_label() {
        // The decision: the index labels the series, it does not name it. Encoding it
        // into the name would make every interface its own metric and break every
        // aggregation across a device.
        let m = metric("network.io.receive", "1.3.6.1.2.1.31.1.1.1.6", "By", true);
        let readings = vec![
            Reading {
                oid: oid("1.3.6.1.2.1.31.1.1.1.6.1"),
                value: Numeric::Unsigned(100),
            },
            Reading {
                oid: oid("1.3.6.1.2.1.31.1.1.1.6.2"),
                value: Numeric::Unsigned(200),
            },
        ];
        let mut names = BTreeMap::new();
        names.insert(vec![1u32], "lo".to_owned());
        names.insert(vec![2u32], "eth0".to_owned());

        let batch = interface_columns(subject(), &m, &readings, &names, now());
        assert_eq!(batch.rows.len(), 2);
        assert!(
            batch.rows.iter().all(|r| r.metric == "network.io.receive"),
            "one metric name, two series"
        );
        assert_eq!(
            batch.rows[0]
                .labels
                .get("network.interface.name")
                .map(String::as_str),
            Some("lo")
        );
        assert_eq!(
            batch.rows[1]
                .labels
                .get("network.interface.index")
                .map(String::as_str),
            Some("2")
        );
    }

    #[test]
    fn an_interface_with_no_name_still_produces_a_sample() {
        // The counter is a real measurement. Dropping it because ifName was unreadable
        // would make a device look partly dead for a cosmetic reason.
        let m = metric("network.io.receive", "1.3.6.1.2.1.31.1.1.1.6", "By", true);
        let readings = vec![Reading {
            oid: oid("1.3.6.1.2.1.31.1.1.1.6.7"),
            value: Numeric::Unsigned(5),
        }];

        let batch = interface_columns(subject(), &m, &readings, &BTreeMap::new(), now());
        assert_eq!(batch.rows.len(), 1);
        assert_eq!(
            batch.rows[0]
                .labels
                .get("network.interface.index")
                .map(String::as_str),
            Some("7")
        );
        assert!(!batch.rows[0].labels.contains_key("network.interface.name"));
    }

    #[test]
    fn a_reading_from_outside_the_column_is_not_attributed_to_it() {
        // walk()'s boundary stop should prevent this reaching here at all, so a nonzero
        // count is a signal about that rather than an expected case.
        let m = metric("network.io.receive", "1.3.6.1.2.1.31.1.1.1.6", "By", true);
        let readings = vec![Reading {
            oid: oid("1.3.6.1.2.1.31.1.1.1.10.1"),
            value: Numeric::Unsigned(9),
        }];

        let batch = interface_columns(subject(), &m, &readings, &BTreeMap::new(), now());
        assert!(batch.rows.is_empty());
        assert_eq!(batch.skipped, 1);
    }

    #[test]
    fn a_counter_is_stored_as_the_number_the_agent_returned() {
        // SPEC §M2: raw, never a rate. A pre-computed rate cannot be recomputed over a
        // different window and hides counter wrap.
        let m = metric("network.io.receive", "1.3.6.1.2.1.31.1.1.1.6", "By", true);
        let readings = vec![Reading {
            oid: oid("1.3.6.1.2.1.31.1.1.1.6.1"),
            value: Numeric::Unsigned(4_294_967_295),
        }];

        let batch = interface_columns(subject(), &m, &readings, &BTreeMap::new(), now());
        assert!(
            (batch.rows[0].value - 4_294_967_295.0).abs() < 1.0,
            "the raw counter, not a delta: {}",
            batch.rows[0].value
        );
    }

    #[test]
    fn a_device_with_no_site_still_produces_storable_rows() {
        // resource.site_id is nullable; MetricRow.site_id is not. One answer to that,
        // here, rather than a different one at each call site.
        let m = metric("system.uptime", "1.3.6.1.2.1.1.3.0", "s", false);
        let readings = vec![Reading {
            oid: oid("1.3.6.1.2.1.1.3.0"),
            value: Numeric::Unsigned(1),
        }];
        let batch = scalars(subject(), &[m], &readings, now());
        assert_eq!(batch.rows[0].site_id.into_uuid(), uuid::Uuid::nil());
    }

    #[test]
    fn observed_at_is_shared_across_a_batch_and_ingested_at_is_not_faked() {
        // Every reading in one response was observed at one moment; pretending
        // otherwise would make a device's metrics un-joinable at the same timestamp.
        let at = now();
        let metrics = vec![
            metric("system.uptime", "1.3.6.1.2.1.1.3.0", "s", false),
            metric(
                "system.cpu.utilization",
                "1.3.6.1.2.1.25.3.3.1.2.1",
                "%",
                false,
            ),
        ];
        let readings = vec![
            Reading {
                oid: oid("1.3.6.1.2.1.1.3.0"),
                value: Numeric::Unsigned(1),
            },
            Reading {
                oid: oid("1.3.6.1.2.1.25.3.3.1.2.1"),
                value: Numeric::Unsigned(2),
            },
        ];

        let batch = scalars(subject(), &metrics, &readings, at);
        assert_eq!(batch.rows.len(), 2);
        assert_eq!(batch.rows[0].observed_at, at);
        assert_eq!(batch.rows[1].observed_at, at);
        assert!(batch.rows[0].ingested_at >= at);
    }
}
