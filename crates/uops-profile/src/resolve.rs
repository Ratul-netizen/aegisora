//! Which profile a device gets.
//!
//! SPEC §M2 gives the order: explicit `resource.profile_id` → `sysObjectID` match →
//! generic fallback. Two things about it are worth stating, because both are decisions
//! rather than mechanics.
//!
//! # Longest prefix wins
//!
//! SPEC says "`sysObjectID` match" and leaves the tie unresolved. A device reporting
//! `1.3.6.1.4.1.9.1.516` matches Cisco's enterprise arc `1.3.6.1.4.1.9`, and would also
//! match a profile written for that exact model. Anything other than longest-prefix
//! makes shipping a model-specific profile impossible without deleting the vendor one,
//! and makes which profile you get depend on iteration order — that is, on nothing.
//!
//! # The fallback is the point
//!
//! SPEC: "an unknown vendor should still get interfaces and availability, not nothing."
//! A device we have never seen is the *normal* case in a network somebody actually
//! runs. `generic-snmp` walks `IF-MIB` and pings, which is most of the value, and the
//! operator gets a working device instead of a support ticket.

use crate::oid::Oid;
use crate::profile::Profile;

/// How a device came to have the profile it has.
///
/// Recorded so that "why is this polling every 30 seconds" has an answer that is not
/// "read the code".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reason {
    /// `resource.profile_id` was set. A human decided; nothing else is consulted.
    Explicit,
    /// The device's `sysObjectID` sits under this profile's prefix.
    SysObjectId,
    /// Nothing matched. See the module docs.
    Fallback,
}

/// The profile chosen for a device, and why.
#[derive(Clone, Debug)]
pub struct Resolved<'a> {
    pub profile: &'a Profile,
    pub reason: Reason,
}

/// The key of the profile every unmatched device falls back to.
pub const FALLBACK_KEY: &str = "generic-snmp";

/// Choose a profile.
///
/// `explicit` is the key from `resource.profile_id`, already looked up. `sysobjectid` is
/// what the device reported, if it was asked.
///
/// Returns `None` only when the candidate set contains no fallback — which is a
/// deployment with no built-ins loaded, not a device problem.
#[must_use]
pub fn resolve<'a>(
    candidates: &'a [Profile],
    explicit: Option<&str>,
    sysobjectid: Option<&Oid>,
) -> Option<Resolved<'a>> {
    // 1. A human said so. Not overridden by anything, including a better prefix match —
    //    an operator who pinned a profile did it because the automatic choice was wrong.
    if let Some(key) = explicit
        && let Some(profile) = candidates.iter().find(|p| p.id == key)
    {
        return Some(Resolved {
            profile,
            reason: Reason::Explicit,
        });
    }

    // 2. Longest matching sysObjectID prefix. See the module docs for why length and
    //    not first-found.
    if let Some(oid) = sysobjectid {
        let best = candidates
            .iter()
            .filter_map(|p| {
                let prefix = p.matches.sysobjectid_prefix.as_ref()?;
                oid.starts_with(prefix).then(|| (prefix.len(), p))
            })
            // max_by_key returns the *last* maximum, which would make a tie depend on
            // ordering. Ties are resolved by profile id so the answer is stable, and a
            // tie means two profiles claim the same prefix, which validation elsewhere
            // should be catching.
            .max_by_key(|(len, p)| (*len, std::cmp::Reverse(p.id.as_str())));

        if let Some((_, profile)) = best {
            return Some(Resolved {
                profile,
                reason: Reason::SysObjectId,
            });
        }
    }

    // 3. Interfaces and availability beat nothing.
    candidates
        .iter()
        .find(|p| p.id == FALLBACK_KEY)
        .map(|profile| Resolved {
            profile,
            reason: Reason::Fallback,
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::builtin;

    fn builtins() -> Vec<Profile> {
        builtin::all().expect("the shipped profiles must load")
    }

    #[test]
    fn an_explicit_profile_beats_a_matching_sysobjectid() {
        let all = builtins();
        // A MikroTik that somebody has pinned to linux-snmp, because it runs a Linux
        // agent they care about more than the RouterOS MIB.
        let mikrotik: Oid = "1.3.6.1.4.1.14988.1".parse().unwrap();
        let chosen = resolve(&all, Some("linux-snmp"), Some(&mikrotik)).unwrap();
        assert_eq!(chosen.profile.id, "linux-snmp");
        assert_eq!(chosen.reason, Reason::Explicit);
    }

    #[test]
    fn an_explicit_key_that_does_not_exist_falls_through_rather_than_failing() {
        // A profile deleted after being assigned. Polling the device with the generic
        // profile beats not polling it, and the reason says it was not the pinned one.
        let all = builtins();
        let chosen = resolve(&all, Some("no-such-profile"), None).unwrap();
        assert_eq!(chosen.profile.id, FALLBACK_KEY);
        assert_eq!(chosen.reason, Reason::Fallback);
    }

    #[test]
    fn a_known_vendor_is_matched_by_sysobjectid() {
        let all = builtins();
        let mikrotik: Oid = "1.3.6.1.4.1.14988.1.1.1".parse().unwrap();
        let chosen = resolve(&all, None, Some(&mikrotik)).unwrap();
        assert_eq!(chosen.profile.id, "mikrotik-routeros");
        assert_eq!(chosen.reason, Reason::SysObjectId);
    }

    #[test]
    fn the_longest_prefix_wins() {
        // The decision SPEC leaves open. Without it, shipping a model-specific profile
        // means deleting the vendor one.
        let mut all = builtins();
        all.push(
            Profile::from_yaml(
                r#"
id: cisco-c9300
version: 1
name: Cisco Catalyst 9300
match:
  sysobjectid_prefix: "1.3.6.1.4.1.9.1.2494"
metrics:
  - name: system.cpu.utilization
    oid: "1.3.6.1.4.1.9.9.109.1.1.1.1.8.1"
    kind: gauge
    unit: "%"
    interval: 60s
"#,
            )
            .unwrap(),
        );

        let a_9300: Oid = "1.3.6.1.4.1.9.1.2494".parse().unwrap();
        assert_eq!(
            resolve(&all, None, Some(&a_9300)).unwrap().profile.id,
            "cisco-c9300"
        );

        // Another Cisco still gets the vendor profile.
        let other: Oid = "1.3.6.1.4.1.9.1.1745".parse().unwrap();
        assert_eq!(
            resolve(&all, None, Some(&other)).unwrap().profile.id,
            "cisco-ios"
        );
    }

    #[test]
    fn an_unknown_vendor_gets_interfaces_and_availability() {
        // SPEC's words. The normal case in a network somebody actually runs.
        let all = builtins();
        let unknown: Oid = "1.3.6.1.4.1.99999.1".parse().unwrap();
        let chosen = resolve(&all, None, Some(&unknown)).unwrap();

        assert_eq!(chosen.profile.id, FALLBACK_KEY);
        assert_eq!(chosen.reason, Reason::Fallback);
        assert!(
            !chosen.profile.discovery.is_empty(),
            "the fallback must still discover interfaces"
        );
        assert!(
            !chosen.profile.availability.is_empty(),
            "the fallback must still check availability"
        );
    }

    #[test]
    fn a_device_that_was_never_asked_still_gets_a_profile() {
        // No sysObjectID: the agent refused, or SNMP is not configured and only ICMP
        // is. It still needs to be polled.
        let all = builtins();
        assert_eq!(resolve(&all, None, None).unwrap().profile.id, FALLBACK_KEY);
    }

    #[test]
    fn a_string_prefix_would_pick_the_wrong_vendor() {
        // Enterprise 1499881 is not MikroTik (14988), and would be under a textual
        // prefix check. This is the test that fails if Oid::starts_with ever becomes
        // string-based.
        let all = builtins();
        let impostor: Oid = "1.3.6.1.4.1.1499881.1".parse().unwrap();
        assert_eq!(
            resolve(&all, None, Some(&impostor)).unwrap().profile.id,
            FALLBACK_KEY
        );
    }

    #[test]
    fn no_candidates_at_all_is_none_rather_than_a_panic() {
        assert!(resolve(&[], None, None).is_none());
    }
}
