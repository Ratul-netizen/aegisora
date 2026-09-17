//! Finding devices — M5, specified in `docs/M5-discovery.md`.
//!
//! An operator who installs this has to type their estate in by hand. Discovery is the
//! answer to that, and it is three things:
//!
//! * **sweep** — probe the addresses in a range an operator wrote down ([`sweep`],
//!   [`probe`]);
//! * **classify** — turn the answer into a vendor, a model and a monitoring profile
//!   (`uops_profile::resolve`, which already existed and is not repeated here);
//! * **walk neighbours** — read LLDP, CDP and ARP on a device that is already known, to
//!   find the ones adjacent to it.
//!
//! The third matters most and is the easiest to underrate: an operator enters one core
//! switch and neighbour discovery finds the estate from it. A sweep is the fallback for
//! the parts of a network nothing points at.
//!
//! # What this crate will not do
//!
//! **Guess a credential.** A job names what it may try. There is no wordlist and no
//! `public` fallback. That is not fastidiousness — a product that ships with a list of
//! likely community strings is indistinguishable from an attack in the customer's own IDS
//! logs, locks `SNMPv3` accounts on several platforms, and teaches its users that guessing
//! credentials is normal. An address that answers nothing is recorded as unreachable,
//! which is a fact an operator can act on.
//!
//! **Scan a port.** No service fingerprinting, no OS detection from TCP behaviour. This
//! finds what answers SNMP and what its neighbours say about it. A network scanner is a
//! different product with a different security posture, and the difference is what a
//! customer's security team will ask about.
//!
//! **Reach outside the ranges it was given.** [`Sweep`] is the only way to obtain an
//! address to probe, and constructing one is the bounds check.

pub mod neighbour;
pub mod probe;
pub mod run;
pub mod sweep;

pub use neighbour::{Neighbour, Neighbours, Protocol, neighbours};
pub use probe::{Answer, Sighting, probe};
pub use run::{Findings, run};
pub use sweep::{
    IN_FLIGHT, MAX_ADDRESSES, PROBE_TIMEOUT, PROBES_PER_SECOND, Range, Sweep, SweepError,
    WIDEST_PREFIX,
};
