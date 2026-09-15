//! The seam between the walk and the wire.
//!
//! One trait with one method. It exists so that SPEC §M2's first acceptance criterion —
//! *"1 000 simulated SNMP agents polled at 60s with p95 poll latency < 5 s and no missed
//! cycles"* — is a test that runs in CI rather than a lab somebody has to build. A
//! thousand real agents is a room full of hardware; a thousand [`crate::sim::Agent`]s is
//! a `Vec`.
//!
//! It also makes the failures testable, which matters more. Every interesting thing a
//! poller has to cope with is a device behaving badly, and behaving badly on demand is
//! something only a simulator does reliably: an agent that never answers, one that
//! answers `tooBig` to everything, one that returns the same OID forever. Waiting for a
//! real switch to do those is not a test strategy.

use uops_profile::Oid;

use crate::bulk::Repetitions;

/// Where to send a request, and as whom.
///
/// The credential is deliberately not here. v3 authentication needs material from
/// `SecretStore`, and a `Target` that carried it would be a struct that must never be
/// logged, cloned carelessly, or held longer than a request — which is a lot to ask of
/// a value that is otherwise just an address. See `credential.rs`.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Target {
    pub address: std::net::SocketAddr,
}

/// One OID and what the agent said it holds.
#[derive(Clone, Debug, PartialEq)]
pub struct VarBind {
    pub oid: Oid,
    pub value: Value,
}

/// An SNMP value, reduced to what a profile can ask for.
///
/// Not every ASN.1 type snmp2 can decode — the ones a metric or an identifier is
/// actually read from. Anything else arrives as [`Value::Other`] and is skipped with a
/// note rather than guessed at.
#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Integer(i64),
    /// `Counter32`, `Gauge32`, `TimeTicks`, `Unsigned32` — all unsigned, and the width
    /// is the profile's business rather than the wire's.
    Unsigned(u64),
    Counter64(u64),
    /// `OCTET STRING`. Bytes, not a `String`: `ifPhysAddress` is six raw bytes and
    /// `ifName` is text, and which one it is depends on the OID, not the encoding.
    Bytes(Vec<u8>),
    ObjectId(Oid),
    /// The agent has nothing at or after this OID. The end of a walk.
    EndOfMibView,
    /// The agent has nothing at this OID but the walk continues.
    NoSuchInstance,
    /// Decoded, but not a type any profile reads.
    Other,
}

/// Why a request did not produce varbinds.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum TransportError {
    /// The agent says the response would not fit. Handled by halving — see
    /// [`crate::bulk`] — and the one error here that is not a failure.
    #[error("the agent cannot fit that many repetitions in one response")]
    TooBig,

    #[error("no response within the timeout")]
    Timeout,

    #[error("the agent refused the credentials")]
    AuthFailed,

    #[error("unreachable: {0}")]
    Unreachable(String),

    #[error("the agent sent something that is not a well-formed response: {0}")]
    Protocol(String),
}

/// Something that can ask a device for a run of varbinds.
///
/// `GETBULK` only. A poller that also needed `GET` would need a second method; it does
/// not — a scalar is a `GETBULK` of one repetition, and having one code path means the
/// `tooBig` handling covers scalars too rather than being a thing only walks get.
#[async_trait::async_trait]
pub trait Transport: Send + Sync {
    /// Ask for up to `max_repetitions` varbinds following `after`.
    ///
    /// `after` is exclusive, as `GETBULK` is: the first varbind returned is the one
    /// lexicographically after it.
    async fn get_bulk(
        &self,
        target: &Target,
        after: &Oid,
        max_repetitions: Repetitions,
    ) -> Result<Vec<VarBind>, TransportError>;
}
