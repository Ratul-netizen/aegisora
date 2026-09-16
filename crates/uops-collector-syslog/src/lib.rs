//! The syslog daemon — SPEC §M3's collector.
//!
//! Everything from the wire to a row exists elsewhere and is tested there:
//! `uops-syslog` parses, frames and receives; `uops-pipeline` resolves, enriches and
//! batches. This crate is the process that joins them, and the two decisions that only a
//! process can make.
//!
//! **Which tenant a message belongs to**, which is [`config`]: one listener per tenant,
//! because a syslog message carries no tenant and cannot be made to, and the binding is
//! the one thing a sender cannot influence.
//!
//! **Who waits for whom**, which is [`run`]: a bounded channel at every hop, so a slow
//! `ClickHouse` reaches back through the batcher and the workers to the TCP receive
//! window — and stops at UDP, which has no back channel and so drops and counts instead.

pub mod config;
pub mod run;
pub mod shutdown;

pub use config::Config;
