//! The OTLP receiver — SPEC §M3's second collector.
//!
//! Everything from a protobuf body to a row exists elsewhere and is tested there:
//! `uops-otlp` decodes and converts, `uops-pipeline` resolves, enriches and batches. This
//! crate is the process that joins them, and it is deliberately the same shape as
//! `uops-collector-syslog` — one listener per tenant, one batcher per signal, bounded
//! channels at every hop.
//!
//! # OTLP/HTTP, not gRPC
//!
//! `uops-otlp`'s crate docs have the reasoning: `gen-tonic` would pull tonic, h2 and
//! tower, and OTLP/HTTP carries byte-identical protobuf bodies. The OpenTelemetry
//! Collector's `otlphttp` exporter needs no translation to talk to this.
//!
//! # The routes, and why one of them stores nothing
//!
//! | route | what happens |
//! |---|---|
//! | `POST /v1/logs` | converted, resolved, batched into `logs` |
//! | `POST /v1/metrics` | converted, resolved, batched into `metrics` |
//! | `POST /v1/traces` | **accepted, counted, discarded** |
//!
//! Traces are SPEC's explicit instruction and the reasoning is worth keeping: *"trace
//! accepts and stores nothing until M8 — accept and drop with a counter, so instrumented
//! apps don't error."* An exporter that gets a 404 retries, backs off, and logs an error
//! every batch forever. "Traces are not stored yet" and "the endpoint is broken" must not
//! look the same from the outside, and only one of them is true.
//!
//! # Partial success is a real OTLP response and this uses it
//!
//! `ExportLogsServiceResponse` carries a `partial_success` with a rejected count and an
//! error message. A receiver that converted nine records of ten and answered `200 {}`
//! would be lying by omission. Histograms, unresolvable data points and unsupported
//! metric types are all reported that way, which is how an operator whose latency
//! histograms never appear finds out from their own collector's logs rather than from a
//! missing chart.

pub mod config;
pub mod routes;
pub mod run;
pub mod shutdown;

pub use config::Config;
