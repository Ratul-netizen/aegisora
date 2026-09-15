//! An SNMP object identifier.
//!
//! A newtype over the arcs, not a `String`, for one reason that matters more than
//! tidiness: **prefix matching**. Profile resolution picks a profile by `sysObjectID`
//! prefix, and string prefixes get it wrong in a way nobody notices until a device
//! matches the wrong vendor —
//!
//! ```text
//! "1.3.6.1.4.1.9"   starts_with   "1.3.6.1.4.1.9"     Cisco       ✓
//! "1.3.6.1.4.1.911" starts_with   "1.3.6.1.4.1.9"     not Cisco   ✗ but true
//! ```
//!
//! Arc-wise, `[1,3,6,1,4,1,911]` does not start with `[1,3,6,1,4,1,9]`, and the
//! question answers itself.
//!
//! Parsing is strict. A profile with a typo in an OID must fail when it is loaded, on a
//! developer's machine or at deploy time, and not at three in the morning when a poller
//! sends a malformed request to a device and gets silence back.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// An object identifier, as arcs.
///
/// `u32` per arc: SMI allows arcs up to 2^32-1, and `ifIndex` values on a large chassis
/// comfortably exceed what a `u16` would hold.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Oid(Vec<u32>);

/// Why an OID string was refused.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum OidError {
    #[error("an OID cannot be empty")]
    Empty,

    #[error("an OID needs at least two arcs, got {0}")]
    TooShort(usize),

    #[error("`{0}` is not a number — an OID is numeric, not a MIB name")]
    NotNumeric(String),

    #[error("arc `{0}` does not fit in 32 bits")]
    ArcTooLarge(String),

    /// `1.3.6.1..2` and `.1.3.6` are both this.
    #[error("an empty arc — check for a doubled or leading dot")]
    EmptyArc,

    #[error("the first arc must be 0, 1 or 2, got {0}")]
    BadRoot(u32),

    #[error("with a first arc of {first}, the second must be 0-39, got {second}")]
    BadSecondArc { first: u32, second: u32 },
}

impl Oid {
    /// The arcs.
    #[must_use]
    pub fn arcs(&self) -> &[u32] {
        &self.0
    }

    /// True when `self` is `prefix`, or sits beneath it.
    ///
    /// The whole reason this type exists. See the module docs.
    #[must_use]
    pub fn starts_with(&self, prefix: &Self) -> bool {
        self.0.len() >= prefix.0.len() && self.0[..prefix.0.len()] == prefix.0[..]
    }

    /// How many arcs, for ranking two matching prefixes against each other.
    ///
    /// A device matching both `1.3.6.1.4.1.9` (Cisco) and `1.3.6.1.4.1.9.1.516` (a
    /// specific model) must get the model. Longest prefix wins, and this is the length.
    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// `self` with `suffix` appended — an instance of a table column, usually.
    #[must_use]
    pub fn child(&self, suffix: u32) -> Self {
        let mut arcs = self.0.clone();
        arcs.push(suffix);
        Self(arcs)
    }
}

impl FromStr for Oid {
    type Err = OidError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        // A leading dot is how OIDs are often written (`.1.3.6.1`) and means the same
        // thing. Accepted and dropped; a *trailing* dot is a typo and is not.
        let body = s.strip_prefix('.').unwrap_or(s);
        if body.is_empty() {
            return Err(OidError::Empty);
        }

        let mut arcs = Vec::new();
        for part in body.split('.') {
            if part.is_empty() {
                return Err(OidError::EmptyArc);
            }
            if !part.bytes().all(|b| b.is_ascii_digit()) {
                return Err(OidError::NotNumeric(part.to_owned()));
            }
            let arc: u32 = part
                .parse()
                .map_err(|_| OidError::ArcTooLarge(part.to_owned()))?;
            arcs.push(arc);
        }

        if arcs.len() < 2 {
            return Err(OidError::TooShort(arcs.len()));
        }

        // X.690: the first two arcs are encoded as one byte, 40*first + second, which
        // is only reversible under these constraints. An OID that violates them cannot
        // be put on the wire, so refusing it here beats discovering that in a poller.
        let first = arcs[0];
        if first > 2 {
            return Err(OidError::BadRoot(first));
        }
        let second = arcs[1];
        if first < 2 && second > 39 {
            return Err(OidError::BadSecondArc { first, second });
        }

        Ok(Self(arcs))
    }
}

impl fmt::Display for Oid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut first = true;
        for arc in &self.0 {
            if !first {
                f.write_str(".")?;
            }
            write!(f, "{arc}")?;
            first = false;
        }
        Ok(())
    }
}

impl Serialize for Oid {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Oid {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        // As a string, so that YAML's number parsing never sees `1.3` and helpfully
        // makes it a float.
        let raw = String::deserialize(d)?;
        raw.parse().map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_well_formed_oid_round_trips() {
        let oid: Oid = "1.3.6.1.2.1.2.2.1".parse().unwrap();
        assert_eq!(oid.arcs(), &[1, 3, 6, 1, 2, 1, 2, 2, 1]);
        assert_eq!(oid.to_string(), "1.3.6.1.2.1.2.2.1");
    }

    #[test]
    fn a_leading_dot_is_the_same_oid() {
        assert_eq!(
            ".1.3.6.1".parse::<Oid>().unwrap(),
            "1.3.6.1".parse::<Oid>().unwrap()
        );
    }

    #[test]
    fn prefix_matching_is_arc_wise_not_textual() {
        // The bug this type exists to prevent. Enterprise 911 is not Cisco, and a
        // string prefix check says it is.
        let cisco: Oid = "1.3.6.1.4.1.9".parse().unwrap();
        let a_cisco: Oid = "1.3.6.1.4.1.9.1.516".parse().unwrap();
        let not_cisco: Oid = "1.3.6.1.4.1.911.2".parse().unwrap();

        assert!(a_cisco.starts_with(&cisco));
        assert!(!not_cisco.starts_with(&cisco));
        assert!(
            not_cisco.to_string().starts_with(&cisco.to_string()),
            "if this ever fails the test above is no longer proving anything"
        );

        // A prefix matches itself.
        assert!(cisco.starts_with(&cisco));
        // And nothing shorter matches something longer.
        assert!(!cisco.starts_with(&a_cisco));
    }

    #[test]
    fn a_typo_is_refused_at_load_and_says_which_one() {
        let cases: [(&str, OidError); 7] = [
            ("", OidError::Empty),
            ("1", OidError::TooShort(1)),
            ("1.3.6..1", OidError::EmptyArc),
            ("1.3.6.1.", OidError::EmptyArc),
            (
                "1.3.6.1.ifIndex",
                OidError::NotNumeric("ifIndex".to_owned()),
            ),
            (
                "1.3.6.1.99999999999",
                OidError::ArcTooLarge("99999999999".to_owned()),
            ),
            ("3.6.1", OidError::BadRoot(3)),
        ];
        for (input, expected) in cases {
            assert_eq!(input.parse::<Oid>().unwrap_err(), expected, "for {input:?}");
        }
    }

    #[test]
    fn the_x690_first_two_arc_rule_is_enforced() {
        // 40*first + second must be reversible, so second is capped below 40 unless
        // first is 2. An OID that breaks this cannot be encoded at all.
        assert!("1.40.1".parse::<Oid>().is_err());
        assert!("1.39.1".parse::<Oid>().is_ok());
        // The joint-iso-itu-t arc has no such cap.
        assert!("2.999.1".parse::<Oid>().is_ok());
    }

    #[test]
    fn child_appends_an_instance() {
        let col: Oid = "1.3.6.1.2.1.2.2.1.6".parse().unwrap();
        assert_eq!(col.child(3).to_string(), "1.3.6.1.2.1.2.2.1.6.3");
    }

    #[test]
    fn an_unquoted_oid_keeps_every_arc() {
        // The failure this guards against: a YAML scalar that looks like a float being
        // read as one, where 1.30 and 1.3 are the same number and an arc disappears.
        //
        // It does not happen, because deserializing through String hands the visitor
        // the raw scalar text rather than a parsed value — but that is a property of
        // *how* Deserialize is written here, and rewriting it to go through
        // serde_yaml_ng::Value would silently reintroduce it.
        let quoted: Oid = serde_yaml_ng::from_str("\"1.3.6.1\"").unwrap();
        let bare: Oid = serde_yaml_ng::from_str("1.3.6.1").unwrap();
        assert_eq!(quoted, bare);

        // The pair that would collide if an OID ever went through a float.
        let short: Oid = serde_yaml_ng::from_str("1.3").unwrap();
        let padded: Oid = serde_yaml_ng::from_str("1.30").unwrap();
        assert_ne!(short, padded, "1.30 must not be read as 1.3");
        assert_eq!(padded.arcs(), &[1, 30]);
    }
}
