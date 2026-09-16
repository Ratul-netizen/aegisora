//! The three OTLP/HTTP endpoints.
//!
//! # What an exporter is told, and why it matters
//!
//! OTLP defines a `partial_success` in every export response, and it is the difference
//! between a receiver that is honest and one that is not. A receiver that converted nine
//! records of ten and answered `200 {}` would be lying by omission — the exporter would
//! move on, the operator would never know, and the missing tenth would only surface as a
//! chart that is quietly wrong.
//!
//! So everything this cannot store is counted and named in the response: histograms,
//! summaries, and data points with no timestamp. An operator whose latency histograms
//! never appear learns it from their own collector's logs rather than from an absent
//! chart three weeks later.
//!
//! # Status codes
//!
//! `200` when the payload was understood, whatever fraction of it could be stored.
//! `400` when the protobuf did not decode, which is a bug in the sender and is not worth
//! retrying. `503` when the queue to the batcher is closed, which means this process is
//! shutting down and the exporter **should** retry — the OTLP specification says a 503 is
//! retryable and that is exactly the behaviour wanted.
//!
//! Notably not `429`. Backpressure here is the handler *waiting* on a bounded channel,
//! which becomes the exporter waiting on the response, which is what HTTP already does
//! well. Telling a collector to go away and come back is worse than making it wait.

use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use opentelemetry_proto::tonic::collector::logs::v1::{
    ExportLogsPartialSuccess, ExportLogsServiceRequest, ExportLogsServiceResponse,
};
use opentelemetry_proto::tonic::collector::metrics::v1::{
    ExportMetricsPartialSuccess, ExportMetricsServiceRequest, ExportMetricsServiceResponse,
};
use opentelemetry_proto::tonic::collector::trace::v1::{
    ExportTracePartialSuccess, ExportTraceServiceRequest, ExportTraceServiceResponse,
};
use prost::Message as _;

use crate::run::Listener;

/// `application/x-protobuf`, which is what the `otlphttp` exporter sends and expects.
///
/// OTLP/HTTP also defines a JSON encoding. It is not accepted here: the Collector does not
/// default to it, supporting it would mean a second decoder to keep in step with the
/// first, and the failure of a second decoder is rows that differ depending on how they
/// arrived — which is the thing this whole product is built to prevent.
const PROTOBUF: &str = "application/x-protobuf";

/// One of the three services' answers, encoded.
fn protobuf(message: &impl prost::Message) -> Response {
    let mut body = Vec::with_capacity(message.encoded_len());
    // Encoding into a Vec with enough capacity cannot fail; the Result is prost's
    // signature for writers that can.
    let _ = message.encode(&mut body);
    ([(axum::http::header::CONTENT_TYPE, PROTOBUF)], body).into_response()
}

/// A request body that was not a valid export request.
fn undecodable(what: &str, e: &prost::DecodeError) -> Response {
    // 400, not 500: the sender produced this and retrying it will produce it again.
    (
        StatusCode::BAD_REQUEST,
        format!("the {what} export request could not be decoded: {e}"),
    )
        .into_response()
}

/// The queue to the batcher has closed, which means this process is stopping.
///
/// 503 rather than 500, because OTLP says a 503 is retryable and an exporter that retries
/// into the *next* instance is the behaviour wanted during a rolling restart.
fn unavailable() -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        "the collector is shutting down; retry",
    )
        .into_response()
}

/// `POST /v1/logs`
pub async fn logs(State(listener): State<Arc<Listener>>, body: Bytes) -> Response {
    let request = match ExportLogsServiceRequest::decode(body) {
        Ok(r) => r,
        Err(e) => return undecodable("logs", &e),
    };

    match listener.ingest_logs(&request.resource_logs).await {
        Ok(rejected) => protobuf(&ExportLogsServiceResponse {
            partial_success: partial(rejected).map(|(count, message)| ExportLogsPartialSuccess {
                rejected_log_records: count,
                error_message: message,
            }),
        }),
        Err(_) => unavailable(),
    }
}

/// `POST /v1/metrics`
pub async fn metrics(State(listener): State<Arc<Listener>>, body: Bytes) -> Response {
    let request = match ExportMetricsServiceRequest::decode(body) {
        Ok(r) => r,
        Err(e) => return undecodable("metrics", &e),
    };

    match listener.ingest_metrics(&request.resource_metrics).await {
        Ok(rejected) => protobuf(&ExportMetricsServiceResponse {
            partial_success: partial(rejected).map(|(count, message)| {
                ExportMetricsPartialSuccess {
                    rejected_data_points: count,
                    error_message: message,
                }
            }),
        }),
        Err(_) => unavailable(),
    }
}

/// `POST /v1/traces` — accepted, counted, discarded.
///
/// SPEC §M3: *"trace accepts and stores nothing until M8 — accept and drop with a counter,
/// so instrumented apps don't error."*
///
/// The spans are **counted**, not the requests, and the count is reported as
/// `rejected_spans` with a message saying why. That is the honest reading of
/// `partial_success`: they were received and they were not stored, and an exporter that
/// is told so can stop sending them. A `200 {}` would claim they were kept.
pub async fn traces(State(listener): State<Arc<Listener>>, body: Bytes) -> Response {
    let request = match ExportTraceServiceRequest::decode(body) {
        Ok(r) => r,
        Err(e) => return undecodable("trace", &e),
    };

    let spans: usize = request
        .resource_spans
        .iter()
        .flat_map(|rs| rs.scope_spans.iter())
        .map(|ss| ss.spans.len())
        .sum();
    listener.count_spans(spans as u64);

    protobuf(&ExportTraceServiceResponse {
        partial_success: (spans > 0).then(|| ExportTracePartialSuccess {
            rejected_spans: i64::try_from(spans).unwrap_or(i64::MAX),
            error_message: "traces are accepted but not stored until M8; the endpoint is \
                            working and the spans were discarded"
                .to_owned(),
        }),
    })
}

/// Turn a rejected count into the `partial_success` an exporter should see.
///
/// `None` when nothing was rejected: OTLP says an empty `partial_success` means full
/// success, and sending one populated with zeros is a message some collectors log as a
/// warning every batch.
fn partial(rejected: Rejected) -> Option<(i64, String)> {
    if rejected.count == 0 {
        return None;
    }
    Some((
        i64::try_from(rejected.count).unwrap_or(i64::MAX),
        rejected.why,
    ))
}

/// What could not be stored, and why — in the exporter's words rather than ours.
#[derive(Debug, Default, Clone)]
pub struct Rejected {
    pub count: u64,
    pub why: String,
}

impl Rejected {
    #[must_use]
    pub fn new(count: u64, why: impl Into<String>) -> Self {
        Self {
            count,
            why: why.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nothing_rejected_is_no_partial_success_at_all() {
        // OTLP says an empty partial_success means full success, and one populated with
        // zeros is a message some collectors log as a warning every batch.
        assert!(partial(Rejected::default()).is_none());
        assert!(partial(Rejected::new(0, "unused")).is_none());
    }

    #[test]
    fn something_rejected_is_reported_with_a_reason() {
        // The difference between a receiver that is honest and one that converted nine
        // records of ten and answered 200 {}.
        let (count, why) =
            partial(Rejected::new(3, "histograms are not stored yet")).expect("a partial success");
        assert_eq!(count, 3);
        assert!(why.contains("histograms"));
    }
}
