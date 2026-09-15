//! The poller, as a library.
//!
//! `main.rs` is the order these pieces happen in; this is the pieces. The split exists
//! so the parts that can be wrong without a network — parsing a `mgmt_ip`, choosing a
//! profile, deciding what to say about a failure — are testable, which they would not
//! be inside a `main` whose only output is a terminal.
//!
//! What lives elsewhere, and why: the *schedule* is `uops-poll`, which owns the wheel
//! and the executor and is tested without a device; the *wire* is `uops-snmp`; the
//! *rows* are `uops-store-ch`. This crate is the joins between them, and nothing else.

pub mod config;
pub mod credentials;
pub mod fleet;
pub mod poll;
pub mod run;
pub mod shutdown;
