//! The profiles that ship with the product.
//!
//! Embedded with `include_str!` rather than read from `profiles/` at run time, for the
//! same reason the migrations are embedded in `uops-pg-migrate`: the deployed artefact
//! then cannot disagree with the code it was built alongside. There is no directory to
//! forget to copy into an image, and no chance of a binary from one release reading
//! profiles from another.
//!
//! The files are still the source. `profiles/*.yaml` is what a person edits and what a
//! diff shows; this module is how they get into the binary.
//!
//! # Five, and the SPEC says to stop there
//!
//! > Built-ins for v0.1 — resist adding more: generic-snmp, linux-snmp,
//! > mikrotik-routeros, cisco-ios, windows-snmp. Five is enough to prove the model; a
//! > profile library is a community contribution surface later, not a solo-developer
//! > task.
//!
//! [`all`] returns exactly those five and a test asserts the count, so adding a sixth
//! is a deliberate act with a failing test attached rather than a quiet afternoon.

use crate::profile::{Profile, ProfileError};

/// Every shipped profile, as `(key, yaml)`.
///
/// The key is repeated here and inside the document. They are checked against each
/// other in [`all`], because a file named one thing and declaring another is exactly
/// the sort of mistake that produces "why is this device using the wrong profile".
const SOURCES: &[(&str, &str)] = &[
    (
        "generic-snmp",
        include_str!("../../../profiles/generic-snmp.yaml"),
    ),
    (
        "linux-snmp",
        include_str!("../../../profiles/linux-snmp.yaml"),
    ),
    (
        "mikrotik-routeros",
        include_str!("../../../profiles/mikrotik-routeros.yaml"),
    ),
    (
        "cisco-ios",
        include_str!("../../../profiles/cisco-ios.yaml"),
    ),
    (
        "windows-snmp",
        include_str!("../../../profiles/windows-snmp.yaml"),
    ),
];

/// How many profiles ship. See the module docs.
pub const COUNT: usize = 5;

/// Parse and validate every shipped profile.
///
/// # Errors
///
/// Any profile that does not parse or does not validate, named. This cannot happen in a
/// released binary — `builtins_are_all_valid` runs it — but it is a `Result` rather than
/// a panic because a caller loading profiles at startup should report the problem the
/// way it reports every other startup problem.
pub fn all() -> Result<Vec<Profile>, ProfileError> {
    let mut out = Vec::with_capacity(SOURCES.len());
    for (key, yaml) in SOURCES {
        let profile = Profile::from_yaml(yaml)?;
        if profile.id != *key {
            return Err(ProfileError::BadId(format!(
                "{}.yaml declares id `{}`",
                key, profile.id
            )));
        }
        out.push(profile);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profile::{CheckKind, DiscoveryKind, MetricKind, Scope};
    use crate::resolve::FALLBACK_KEY;

    #[test]
    fn every_shipped_profile_parses_and_validates() {
        // The assertion that makes a broken profile unshippable. Every rule in
        // Profile::validate runs against every file here, at test time, so a typo'd OID
        // or an interface metric with nothing to attach to fails on a laptop rather
        // than against a customer's switch.
        let all = all().expect("the shipped profiles must all be valid");
        assert_eq!(all.len(), COUNT);
    }

    #[test]
    fn there_are_exactly_five() {
        // SPEC §M2 says five and says to resist more. If this fails because somebody
        // added one, that is the conversation the test is asking for — not a bug.
        assert_eq!(
            all().unwrap().len(),
            5,
            "SPEC §M2: five built-ins. A profile library is a contribution surface \
             later, not a solo-developer task."
        );
    }

    #[test]
    fn the_fallback_exists_and_is_never_auto_selected() {
        let all = all().unwrap();
        let fallback = all
            .iter()
            .find(|p| p.id == FALLBACK_KEY)
            .expect("the fallback profile must ship");

        // It must not carry a prefix, or it would compete with real vendor profiles
        // instead of catching what they miss.
        assert!(
            fallback.matches.sysobjectid_prefix.is_none(),
            "the fallback must not match a sysObjectID prefix"
        );

        // And it must actually do the two things SPEC promises an unknown device gets.
        assert!(
            fallback
                .discovery
                .iter()
                .any(|d| d.kind == DiscoveryKind::Interface)
        );
        assert!(
            fallback
                .availability
                .iter()
                .any(|a| a.kind == CheckKind::Icmp)
        );
    }

    #[test]
    fn no_two_profiles_claim_the_same_prefix() {
        // A tie makes resolution depend on ordering. resolve() breaks ties
        // deterministically so it is never *undefined*, but two profiles claiming one
        // vendor is a mistake in the files, not something to resolve gracefully.
        let all = all().unwrap();
        let mut prefixes: Vec<String> = all
            .iter()
            .filter_map(|p| p.matches.sysobjectid_prefix.as_ref())
            .map(ToString::to_string)
            .collect();
        prefixes.sort();
        let before = prefixes.len();
        prefixes.dedup();
        assert_eq!(before, prefixes.len(), "two profiles share a prefix");
    }

    #[test]
    fn every_profile_discovers_interfaces_and_checks_availability() {
        // Not a rule in validate(), because a profile for something with no interfaces
        // is legitimate — a UPS, say. It is true of all five of these, and a sixth that
        // breaks it should have to say why.
        for p in all().unwrap() {
            assert!(
                p.discovery
                    .iter()
                    .any(|d| d.kind == DiscoveryKind::Interface),
                "{} discovers no interfaces",
                p.id
            );
            assert!(
                !p.availability.is_empty(),
                "{} checks no availability",
                p.id
            );
        }
    }

    #[test]
    fn byte_counters_are_the_64_bit_ones() {
        // SPEC §M2: a 32-bit ifInOctets wraps in ~34 seconds on a 1 Gbps link, which is
        // faster than any interval any of these profiles use. The 64-bit ifHC*
        // counters live under ifXTable — 1.3.6.1.2.1.31.1.1.1 — and the 32-bit ones
        // under ifTable at 1.3.6.1.2.1.2.2.1. Any byte counter from the second is a
        // number that means something different every minute.
        for p in all().unwrap() {
            for m in &p.metrics {
                if m.unit != "By" || m.kind != MetricKind::Counter {
                    continue;
                }
                let oid = m.oid.to_string();
                assert!(
                    oid.starts_with("1.3.6.1.2.1.31.1.1.1."),
                    "{} polls {} for {} — that is a 32-bit counter and it wraps",
                    p.id,
                    oid,
                    m.name
                );
            }
        }
    }

    #[test]
    fn an_interface_scoped_metric_always_has_something_to_attach_to() {
        // validate() enforces this per profile; this is the assertion that it was
        // actually run against the shipped files rather than only against fixtures.
        for p in all().unwrap() {
            let discovers = p
                .discovery
                .iter()
                .any(|d| d.kind == DiscoveryKind::Interface);
            for m in p.interface_metrics() {
                assert!(
                    discovers,
                    "{} scopes {} to interfaces it never finds",
                    p.id, m.name
                );
                assert_eq!(m.scope, Scope::Interface);
            }
        }
    }
}
