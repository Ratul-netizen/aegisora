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

/// Something that can ask a device for varbinds.
///
/// `GETBULK` only, in two shapes — a run following one OID, and one successor each for a
/// set of them. Both are the same PDU: `GETBULK` carries a *non-repeaters* count saying
/// how many of its varbinds are wanted once rather than walked, so a walk is
/// `non_repeaters = 0` and a set of scalars is `non_repeaters = n, max_repetitions = 0`.
/// There is no `GET` here because there does not need to be: the second shape fetches a
/// scalar set in one round trip, and having one PDU means the `tooBig` handling covers
/// scalars rather than being a thing only walks get.
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

    /// Each of `oids`, read as a scalar, in one request.
    ///
    /// Profiles write a scalar either way — `1.3.6.1.2.1.1.3` because that is the object
    /// the MIB document names, or `1.3.6.1.2.1.1.3.0` because that is the instance a
    /// manager reads — and neither is wrong. [`instance`] resolves both to the instance,
    /// so the answer is at `x.0` whichever was written and
    /// `uops_poll::sample::scalars` matches it either way.
    ///
    /// The default implementation sends one request per OID, as `GETNEXT` of the object:
    /// correct, and what a transport with no batching can do. [`crate::UdpTransport`]
    /// overrides it with a single `GET`, which is the difference between five round trips
    /// per device per poll and one.
    async fn get_scalars(
        &self,
        target: &Target,
        oids: &[Oid],
    ) -> Result<Vec<VarBind>, TransportError> {
        let mut out = Vec::with_capacity(oids.len());
        for oid in oids {
            // GETNEXT of the *object*, which lands on its instance. Asking after the
            // instance would skip the thing being asked for and return whatever the
            // agent holds next, which is a different metric's value under this metric's
            // name — a failure that produces plausible numbers rather than an error.
            out.extend(
                self.get_bulk(target, &object(oid), Repetitions::new(1))
                    .await?,
            );
        }
        Ok(out)
    }
}

/// A scalar object's instance OID.
///
/// A scalar has exactly one instance and its sub-identifier is `0` — that is SMI, not a
/// convention this code invented — so `sysUpTime` and `sysUpTime.0` name the same thing
/// and this returns the second for both.
#[must_use]
pub fn instance(oid: &Oid) -> Oid {
    if oid.arcs().last() == Some(&0) {
        oid.clone()
    } else {
        oid.child(0)
    }
}

/// A scalar instance's object OID — the inverse of [`instance`].
///
/// For the `GETNEXT` spelling of a scalar read, whose answer is the successor of the
/// object and therefore the instance.
#[must_use]
pub fn object(oid: &Oid) -> Oid {
    match oid.arcs().last() {
        Some(&0) => oid.parent().unwrap_or_else(|| oid.clone()),
        _ => oid.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_scalar_is_the_same_object_written_either_way() {
        let object_form: Oid = "1.3.6.1.2.1.1.3".parse().unwrap();
        let instance_form: Oid = "1.3.6.1.2.1.1.3.0".parse().unwrap();

        assert_eq!(instance(&object_form), instance_form);
        assert_eq!(instance(&instance_form), instance_form);
        assert_eq!(object(&instance_form), object_form);
        assert_eq!(object(&object_form), object_form);
    }

    #[test]
    fn the_shortest_oid_does_not_lose_its_last_arc() {
        // Two arcs is the shortest an OID may be — they are encoded as one byte — so
        // `object` has nothing to strip. Returning it unchanged is what stops a
        // malformed profile turning into a request for the whole MIB.
        let shortest: Oid = "1.0".parse().unwrap();
        assert_eq!(object(&shortest), shortest);
    }
}
