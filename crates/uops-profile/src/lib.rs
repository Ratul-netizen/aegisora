//! Monitoring profiles — SPEC §M2.
//!
//! A profile says, declaratively, what to walk on a device, what each discovered row
//! becomes, what to poll and how often. The alternative is `if vendor == "Cisco"`
//! spreading through the poller until nobody can change it, and SPEC names that as the
//! thing this exists to prevent.
//!
//! Four pieces, and the interesting work is in the first two:
//!
//! * [`oid`] — an OID as arcs rather than a string, because profile selection is a
//!   prefix match and a textual prefix picks the wrong vendor.
//! * [`interval`] — `60s` rather than `60`, with bounds, because an interval outside
//!   them is a mistake and mistakes in a profile are caught at load or not at all.
//! * [`profile`] — the document, and the validation serde cannot express.
//! * [`resolve`] — explicit → `sysObjectID` → generic fallback.
//!
//! The five shipped profiles are in [`builtin`], embedded from `profiles/*.yaml`.
//!
//! ```
//! use uops_profile::{builtin, resolve};
//!
//! let profiles = builtin::all().unwrap();
//! let sysobjectid = "1.3.6.1.4.1.14988.1.1.1".parse().unwrap();
//! let chosen = resolve::resolve(&profiles, None, Some(&sysobjectid)).unwrap();
//!
//! assert_eq!(chosen.profile.id, "mikrotik-routeros");
//! assert_eq!(chosen.reason, resolve::Reason::SysObjectId);
//! ```

pub mod builtin;
pub mod interval;
pub mod oid;
pub mod profile;
pub mod resolve;

pub use interval::{Interval, Timeout};
pub use oid::{Oid, OidError};
pub use profile::{
    Availability, CheckKind, Creates, Discovery, DiscoveryKind, IdentifierSource, Match, Metric,
    MetricKind, Profile, ProfileError, Relationship, Scope,
};
pub use resolve::{Reason, Resolved};
