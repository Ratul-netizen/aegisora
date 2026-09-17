//! One question, asked of one address.
//!
//! # Why this is not a ping
//!
//! The obvious design pings first and probes what answers. It is wrong here: a device
//! that drops ICMP while answering SNMP is common — it is the default on several firewall
//! platforms and on any host with a restrictive local policy — and a sweep that skips
//! them silently discovers less than the operator's own network diagram.
//!
//! So the probe is a `GET` of three scalars in one request. Nothing answers on 161 unless
//! it is an agent, so a response *is* a device, and the same round trip that proves it is
//! alive also says what it is. ICMP stays available as a pre-filter for a large range
//! where the operator has said it is safe; it is never a prerequisite.
//!
//! # Why three OIDs and not a walk
//!
//! A walk of `system` is five more round trips for `sysContact`, `sysLocation` and
//! `sysServices`, none of which identifies anything or classifies anything. Multiply by
//! 65 536 and the difference is the sweep taking half an hour instead of five minutes.
//! What a device is gets read properly by the first poll, which is a conversation with
//! something already known to exist.

use std::net::SocketAddr;

use uops_core::identity::{IdentifierKind, ObservedIdentity};
use uops_profile::Oid;
use uops_snmp::transport::{Target, Transport, TransportError, Value};

use crate::sweep::PROBE_TIMEOUT;

/// `SNMPv2-MIB::sysObjectID`. What profile resolution matches on.
const SYSOBJECTID: &str = "1.3.6.1.2.1.1.2";
/// `SNMPv2-MIB::sysDescr`. A sentence, and the fallback when there is no profile.
const SYSDESCR: &str = "1.3.6.1.2.1.1.1";
/// `SNMPv2-MIB::sysName`. What the operator calls it.
const SYSNAME: &str = "1.3.6.1.2.1.1.5";

/// The source string recorded on every identity decision discovery makes.
///
/// It ends up in `identity_decision.source`, which is what makes "why did these two
/// become one device" answerable six months later. A sweep and a poll disagreeing about
/// a device is a real situation, and telling them apart afterwards needs this.
pub const SOURCE: &str = "discovery";

/// What one address said.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Sighting {
    pub address: SocketAddr,
    /// Which of the job's credentials this device answered.
    ///
    /// An index rather than the credential itself: this crate never holds credential
    /// material, and the store is what turns the position back into a `CredentialRef` to
    /// write on the resource. `None` when the caller probed with a single unnamed
    /// transport, which is what every test and the single-credential path do.
    ///
    /// It is the whole reason a discovered device is pollable: a resource with no
    /// credential is one nothing has proved it can talk to.
    ///
    /// Always `Some` from a sweep, including the single-transport case — index 0 is an
    /// honest answer there, because the one transport *is* the one that answered. What
    /// turns it into nothing is an empty credential list on the store side, which is the
    /// right place for that decision: this crate does not know whether its transports
    /// came from named credentials or from a test.
    pub credential: Option<usize>,
    /// `None` when the agent answered but had nothing at this OID, which some embedded
    /// stacks do. It is still a device; it is one nothing can classify.
    pub sys_object_id: Option<Oid>,
    pub sys_name: Option<String>,
    pub sys_descr: Option<String>,
}

impl Sighting {
    /// What this sighting proves about who the device is.
    ///
    /// Two identifiers at most, and both are weak: an address DHCP can move and a name
    /// that is duplicated across every site in a estate built from one template. That is
    /// deliberate and it is why the review queue exists — `uops_identity::classify` will
    /// land most sweep results between [`REVIEW_FLOOR`] and [`AUTO_MERGE_THRESHOLD`], and
    /// a human decides.
    ///
    /// The tier-1 identifiers — `entPhysicalSerialNum`, the `SNMPv3` engine ID — are not
    /// here because this probe cannot see them. A serial is a table column and needs an
    /// index to `GET`; an engine ID belongs to the v3 session rather than the MIB. Both
    /// arrive with the first poll and *upgrade* the resolution then, which is the right
    /// order: a device is worth identifying precisely once it is worth polling.
    ///
    /// [`REVIEW_FLOOR`]: uops_core::identity::REVIEW_FLOOR
    /// [`AUTO_MERGE_THRESHOLD`]: uops_core::identity::AUTO_MERGE_THRESHOLD
    #[must_use]
    pub fn observed(&self) -> ObservedIdentity {
        let mut identity = ObservedIdentity::new(SOURCE)
            .with(IdentifierKind::MgmtIp, self.address.ip().to_string());
        if let Some(name) = &self.sys_name {
            identity = identity.with(IdentifierKind::Hostname, name.clone());
        }
        identity
    }
}

/// What happened at one address.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Answer {
    /// Something is there, and this is what it said.
    Device(Box<Sighting>),

    /// An agent is there and refused the credentials this job was given.
    ///
    /// Distinct from [`Answer::Silent`], and the distinction is the whole of §2.2. This
    /// is the outcome that would tempt a scanner into trying another community string,
    /// and it is recorded as a fact instead: a candidate in state `unreachable`, with a
    /// reason an operator can act on by supplying the right credential.
    Refused,

    /// Nothing answered within the timeout. An empty address, a dropped route, or a
    /// device with no SNMP.
    Silent,

    /// The transport itself failed — no route, no socket, a local problem.
    ///
    /// Separate from `Silent` because it says nothing about the address: 254 of these in
    /// a row is a broken run, not an empty network, and reporting them as silent hosts
    /// would tell an operator their estate had vanished.
    Failed(String),
}

/// Ask one address what it is.
///
/// Never returns an error: every outcome, including failure, is a fact about the address
/// that belongs in the run's counters. A sweep of 65 536 addresses in which the 9 000th
/// returns `Err` and aborts is a sweep that has learned nothing and must start again.
pub async fn probe<T: Transport + ?Sized>(transport: &T, address: SocketAddr) -> Answer {
    let oids: [Oid; 3] = [
        SYSOBJECTID.parse().expect("a constant OID must parse"),
        SYSNAME.parse().expect("a constant OID must parse"),
        SYSDESCR.parse().expect("a constant OID must parse"),
    ];
    let target = Target { address };

    // The timeout is applied here rather than left to the transport's own. A poll's
    // five seconds is right for a conversation with a device known to exist and wrong for
    // a probe, where almost every address is empty and the wait *is* the cost of the
    // sweep. See `PROBE_TIMEOUT` for the arithmetic that ties it to the concurrency cap.
    let answered = tokio::time::timeout(PROBE_TIMEOUT, transport.get_scalars(&target, &oids));
    let varbinds = match answered.await {
        Err(_elapsed) => return Answer::Silent,
        Ok(Ok(varbinds)) => varbinds,
        Ok(Err(TransportError::AuthFailed)) => return Answer::Refused,
        Ok(Err(TransportError::Timeout)) => return Answer::Silent,
        // `TooBig` from a GET of three scalars is an agent that cannot fit 60 bytes in a
        // response, which is not a size problem. Treated as an answer, because whatever
        // it is, something is there.
        Ok(Err(TransportError::TooBig)) => {
            return Answer::Device(Box::new(Sighting {
                address,
                credential: None,
                sys_object_id: None,
                sys_name: None,
                sys_descr: None,
            }));
        }
        Ok(Err(e)) => return Answer::Failed(e.to_string()),
    };

    let at = |oid: &Oid| varbinds.iter().find(|vb| vb.oid.starts_with(oid));

    Answer::Device(Box::new(Sighting {
        address,
        // Filled by `probe_each`, which is the only caller that knows which credential
        // it handed over.
        credential: None,
        sys_object_id: at(&oids[0]).and_then(|vb| match &vb.value {
            Value::ObjectId(oid) => Some(oid.clone()),
            _ => None,
        }),
        sys_name: at(&oids[1]).and_then(|vb| text(&vb.value)),
        sys_descr: at(&oids[2]).and_then(|vb| text(&vb.value)),
    }))
}

/// An `OCTET STRING` as the text it is meant to be.
///
/// `from_utf8_lossy` rather than a refusal: `sysDescr` on real equipment contains
/// copyright symbols in whatever the vendor's build machine was set to, and losing the
/// whole description over one byte would lose the only thing that identifies a device
/// with no profile. Empty becomes `None`, because an agent answering with zero bytes has
/// not told us its name.
fn text(value: &Value) -> Option<String> {
    let Value::Bytes(bytes) = value else {
        return None;
    };
    let text = String::from_utf8_lossy(bytes).trim().to_owned();
    (!text.is_empty()).then_some(text)
}

/// Probe one address with each credential in turn, stopping at the first that answers.
///
/// # Why a silent address is retried and not skipped
///
/// The obvious optimisation is to try the next credential only after a *refusal*, and it
/// would miss most of the estate. `SNMPv2c` has no way to say "wrong community": RFC 3416
/// agents drop a request they cannot authenticate, so a wrong community string is
/// indistinguishable from an empty address. [`Answer::Refused`] is very nearly an `SNMPv3`
/// phenomenon — a `usmStats` report — and an installation still running v2c would find
/// nothing at all under the optimisation.
///
/// So every credential is tried against every address that has not answered. That is the
/// design, and this is its cost:
///
/// > **A sweep takes as long as its credential list is long.** One credential over a /16
/// > is about five and a half minutes; four is twenty-two.
///
/// Which is why [`MAX_CREDENTIALS`] exists, why the list is ordered most-likely-first,
/// and why the job form shows the multiplication rather than hiding it.
///
/// Returns the answer and the index of the credential that produced it — `None` when
/// nothing answered, so there is nothing to record against the device.
pub async fn probe_each<T: Transport + ?Sized>(
    transports: &[&T],
    address: SocketAddr,
) -> (Answer, Option<usize>) {
    let mut refused_by_any = false;

    for (index, transport) in transports.iter().enumerate() {
        match probe(*transport, address).await {
            Answer::Device(mut sighting) => {
                sighting.credential = Some(index);
                return (Answer::Device(sighting), Some(index));
            }

            // An agent is there and this credential is not its. Worth recording even if a
            // later one works, because it is the difference between "nothing here" and
            // "something here we cannot talk to".
            Answer::Refused => refused_by_any = true,

            // Silence. Might be an empty address, might be the wrong community — see
            // above — so the next credential still gets a turn.
            Answer::Silent => {}

            // No route, no socket. Another credential travels the same path and will fail
            // the same way, so trying three more is three more timeouts for nothing.
            Answer::Failed(why) => return (Answer::Failed(why), None),
        }
    }

    if refused_by_any {
        (Answer::Refused, None)
    } else {
        (Answer::Silent, None)
    }
}
