//! Who made the network card.
//!
//! A MAC address begins with a block IEEE assigned to an organisation, so a device that
//! tells us nothing else still tells us its manufacturer. That is worth having on its
//! own — an inventory that says "Cisco" beats one that says nothing — and it is worth
//! more as a fallback: `resource.vendor` is otherwise filled only by a device that
//! implements ENTITY-MIB, which many do not.
//!
//! # The 24-bit prefix is not the whole story
//!
//! Everybody calls this "the OUI" and means the first three bytes. That was true until
//! IEEE began issuing smaller blocks: an MA-M assignment is 28 bits and an MA-S is 36.
//! There are 13 672 of them against 39 815 MA-L blocks, so a lookup that only ever took
//! three bytes would be blind to a quarter of the registry — every small manufacturer,
//! which in practice is a great deal of the equipment in a network that is not a
//! household name.
//!
//! It would be blind rather than wrong: IEEE reserves the 24-bit prefixes above the
//! MA-M and MA-S ranges and does not list them in the MA-L registry, so the parent
//! lookup returns nothing rather than somebody else's name. That was worth checking
//! before writing it down — the first draft of this comment asserted the opposite, and
//! the data disagreed.
//!
//! [`lookup`] tries the most specific assignment first regardless, because the registries
//! are IEEE's to reorganise and a lookup that depended on them never overlapping would be
//! depending on something nobody has promised.
//!
//! # What is deliberately not an answer
//!
//! A **locally administered** address — the second-least-significant bit of the first
//! octet — was made up by whoever configured the interface. Every VM, container veth,
//! bond and VRRP address is one, as is the `02:00:...` a hypervisor hands out. There is
//! no manufacturer to find and reporting the parent block's owner would be inventing
//! one.
//!
//! A **CID** is a company identifier for protocols that need to name a company without
//! needing a unique address, so two devices may legitimately carry the same one. It is
//! returned, with [`Registry::Cid`] saying what it is, because a caller enriching an
//! inventory wants it and a caller doing identity resolution must not treat it as
//! unique.
//!
//! # Where the data comes from
//!
//! The four public IEEE registries, flattened into one table by `scripts/oui.sh` and
//! embedded. Nothing downloads anything at build time or at run time: a poller in an
//! air-gapped network is the normal case for this product, not the exception. See
//! `data/README.md` for the provenance and the date.

#![allow(clippy::module_name_repetitions)]

use std::collections::HashMap;
use std::sync::OnceLock;

/// The table, as generated. One line per assignment: hex, registry letter, organisation.
const TABLE: &str = include_str!("../data/assignments.tsv");

/// Which IEEE registry an assignment came from, and therefore how wide it is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Registry {
    /// MA-L, 24 bits. The classic OUI.
    MaL,
    /// MA-M, 28 bits.
    MaM,
    /// MA-S, 36 bits.
    MaS,
    /// A company identifier. **Not** a guarantee of uniqueness — see the module docs.
    Cid,
}

impl Registry {
    /// How many bits of the address this registry's assignments cover.
    #[must_use]
    pub const fn bits(self) -> u8 {
        match self {
            Self::MaL | Self::Cid => 24,
            Self::MaM => 28,
            Self::MaS => 36,
        }
    }

    /// Whether an address carrying this assignment is unique by specification.
    ///
    /// The distinction identity resolution needs: a MAC from a CID may be shared by
    /// design, so matching two resources on one is not proof they are the same device.
    #[must_use]
    pub const fn is_unique(self) -> bool {
        !matches!(self, Self::Cid)
    }

    const fn from_letter(c: u8) -> Option<Self> {
        match c {
            b'L' => Some(Self::MaL),
            b'M' => Some(Self::MaM),
            b'S' => Some(Self::MaS),
            b'C' => Some(Self::Cid),
            _ => None,
        }
    }
}

/// What IEEE assigned, and to whom.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Assignment {
    /// The organisation, verbatim from the registry. Not normalised: "Cisco Systems,
    /// Inc" is how IEEE spells it, and rewriting it here would mean this crate deciding
    /// what a company is called.
    pub organisation: &'static str,
    pub registry: Registry,
}

/// The three tables, keyed by prefix width.
///
/// Three rather than one, because a 28-bit prefix and a 24-bit prefix can have the same
/// numeric value — `0x00000C` is 12 and `0x00000C0` is 192, but `0x0000000` and
/// `0x000000` are both zero. One map would have them collide silently.
struct Tables {
    bits24: HashMap<u32, Assignment>,
    bits28: HashMap<u32, Assignment>,
    bits36: HashMap<u64, Assignment>,
}

fn tables() -> &'static Tables {
    static TABLES: OnceLock<Tables> = OnceLock::new();
    TABLES.get_or_init(|| {
        let mut t = Tables {
            bits24: HashMap::with_capacity(40_000),
            bits28: HashMap::with_capacity(7_000),
            bits36: HashMap::with_capacity(8_000),
        };
        for line in TABLE.lines() {
            let mut parts = line.split('\t');
            let (Some(hex), Some(registry), Some(organisation)) =
                (parts.next(), parts.next(), parts.next())
            else {
                continue;
            };
            let Some(registry) = registry.bytes().next().and_then(Registry::from_letter) else {
                continue;
            };
            let assignment = Assignment {
                organisation,
                registry,
            };
            // The generator only emits these three widths; anything else is a corrupted
            // table and is skipped rather than guessed at.
            match hex.len() {
                6 => {
                    if let Ok(v) = u32::from_str_radix(hex, 16) {
                        t.bits24.insert(v, assignment);
                    }
                }
                7 => {
                    if let Ok(v) = u32::from_str_radix(hex, 16) {
                        t.bits28.insert(v, assignment);
                    }
                }
                9 => {
                    if let Ok(v) = u64::from_str_radix(hex, 16) {
                        t.bits36.insert(v, assignment);
                    }
                }
                _ => {}
            }
        }
        t
    })
}

/// How many assignments are loaded. For a health endpoint, and for the test that would
/// otherwise not notice the table becoming empty.
#[must_use]
pub fn len() -> usize {
    let t = tables();
    t.bits24.len() + t.bits28.len() + t.bits36.len()
}

/// Whether the table is empty. Present because clippy asks for it beside `len`.
#[must_use]
pub fn is_empty() -> bool {
    len() == 0
}

/// True when the address was made up locally rather than assigned.
///
/// Bit 1 of the first octet. Every VM, container veth, bond and VRRP address sets it.
#[must_use]
pub const fn is_locally_administered(mac: [u8; 6]) -> bool {
    mac[0] & 0b0000_0010 != 0
}

/// True when the address is a group address rather than one interface's.
///
/// Bit 0 of the first octet. Broadcast and multicast; never a device's own address, and
/// finding one in an `ifPhysAddress` means something has gone wrong upstream.
#[must_use]
pub const fn is_multicast(mac: [u8; 6]) -> bool {
    mac[0] & 0b0000_0001 != 0
}

/// Who holds the block this address falls in.
///
/// `None` for a locally administered or group address — see the module docs — and for an
/// address in a block IEEE has not assigned.
#[must_use]
pub fn lookup(mac: [u8; 6]) -> Option<Assignment> {
    if is_locally_administered(mac) || is_multicast(mac) {
        return None;
    }
    let t = tables();

    // Most specific first. An MA-S block sits inside an MA-M block, which sits inside an
    // MA-L block, and the organisation that holds the smallest one is the answer.
    let as_u64 = u64::from(mac[0]) << 40
        | u64::from(mac[1]) << 32
        | u64::from(mac[2]) << 24
        | u64::from(mac[3]) << 16
        | u64::from(mac[4]) << 8
        | u64::from(mac[5]);

    if let Some(found) = t.bits36.get(&(as_u64 >> 12)) {
        return Some(*found);
    }
    #[allow(clippy::cast_possible_truncation)]
    let top28 = (as_u64 >> 20) as u32;
    if let Some(found) = t.bits28.get(&top28) {
        return Some(*found);
    }
    #[allow(clippy::cast_possible_truncation)]
    let top24 = (as_u64 >> 24) as u32;
    t.bits24.get(&top24).copied()
}

/// Parse a MAC address written any of the ways a device or a person writes one.
///
/// `aa:bb:cc:dd:ee:ff`, `aa-bb-cc-dd-ee-ff`, `aabb.ccdd.eeff` (which is how Cisco prints
/// them) and bare hex all parse. Case is ignored.
#[must_use]
pub fn parse(mac: &str) -> Option<[u8; 6]> {
    let mut nibbles = [0u8; 12];
    let mut seen = 0;
    for c in mac.bytes() {
        match c {
            b':' | b'-' | b'.' | b' ' => continue,
            _ => {}
        }
        let value = (c as char).to_digit(16)?;
        if seen == 12 {
            // A thirteenth hex digit. Not a MAC address, and truncating would turn a
            // typo into a different device's address.
            return None;
        }
        #[allow(clippy::cast_possible_truncation)]
        {
            nibbles[seen] = value as u8;
        }
        seen += 1;
    }
    if seen != 12 {
        return None;
    }
    let mut out = [0u8; 6];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = nibbles[i * 2] << 4 | nibbles[i * 2 + 1];
    }
    Some(out)
}

/// The vendor for a MAC address written as text, if there is one.
///
/// The convenience the callers actually want: they hold an identifier's `value`, which
/// is a string.
#[must_use]
pub fn vendor_of(mac: &str) -> Option<&'static str> {
    lookup(parse(mac)?).map(|a| a.organisation)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_table_is_loaded_and_is_not_a_stub() {
        // The assertion that notices the data file becoming empty, truncated, or
        // excluded from the package. Fifty thousand is the registry's rough size; the
        // bound is loose because it grows every week.
        assert!(len() > 40_000, "only {} assignments loaded", len());
    }

    #[test]
    fn a_mac_parses_however_it_is_written() {
        let expected = [0x00, 0x00, 0x0C, 0x11, 0x22, 0x33];
        for form in [
            "00:00:0c:11:22:33",
            "00-00-0C-11-22-33",
            "0000.0c11.2233",
            "00000C112233",
            "00:00:0C:11:22:33",
        ] {
            assert_eq!(parse(form), Some(expected), "{form}");
        }
    }

    #[test]
    fn something_that_is_not_a_mac_does_not_parse() {
        // Too short, too long, and not hex. A thirteenth digit is refused rather than
        // truncated: a typo must not silently become a different device's address.
        assert_eq!(parse(""), None);
        assert_eq!(parse("00:00:0c:11:22"), None);
        assert_eq!(parse("00:00:0c:11:22:33:44"), None);
        assert_eq!(parse("00:00:0c:11:22:3g"), None);
    }

    #[test]
    fn a_well_known_prefix_resolves_to_its_owner() {
        // 00:00:0C is Cisco's, and has been since 1983. If this ever fails the table is
        // wrong rather than the test being out of date.
        let found = lookup([0x00, 0x00, 0x0C, 0x11, 0x22, 0x33]).expect("Cisco's OUI");
        assert!(
            found.organisation.contains("Cisco"),
            "{}",
            found.organisation
        );
        assert_eq!(found.registry, Registry::MaL);
        assert!(found.registry.is_unique());
    }

    #[test]
    fn a_locally_administered_address_has_no_manufacturer() {
        // The case that matters most in practice: this is what every VM, container veth
        // and hypervisor-assigned address looks like, and it is what the poller's own
        // discovery fixtures use. Returning the parent block's owner would be inventing
        // a manufacturer for an address somebody made up.
        for first in [0x02, 0x06, 0x0A, 0x0E, 0xAA] {
            let mac = [first, 0x00, 0x0C, 0x11, 0x22, 0x33];
            assert!(is_locally_administered(mac), "{first:#04x}");
            assert_eq!(lookup(mac), None, "{first:#04x}");
        }
    }

    #[test]
    fn a_group_address_is_not_a_device() {
        // Broadcast, and IPv4 multicast. Never an interface's own address.
        assert!(is_multicast([0xFF; 6]));
        assert_eq!(lookup([0xFF; 6]), None);
        assert!(is_multicast([0x01, 0x00, 0x5E, 0x00, 0x00, 0x01]));
        assert_eq!(lookup([0x01, 0x00, 0x5E, 0x00, 0x00, 0x01]), None);
    }

    #[test]
    fn a_smaller_block_resolves_and_a_24_bit_lookup_would_not() {
        // Why this crate reads all four registries rather than just the famous one.
        //
        // Takes a real MA-S assignment from the table, builds an address inside it, and
        // asserts two things: the 36-bit owner is found, and the 24-bit prefix above it
        // is *not* in the table — so a lookup that only took three bytes would return
        // nothing for this device.
        let (hex, org) = TABLE
            .lines()
            .find_map(|l| {
                let mut p = l.split('\t');
                let hex = p.next()?;
                let _registry = p.next()?;
                let org = p.next()?;
                (hex.len() == 9).then_some((hex, org))
            })
            .expect("the table must contain MA-S assignments");

        let mut mac = [0u8; 6];
        let digits: Vec<u32> = hex
            .chars()
            .chain("000".chars())
            .filter_map(|c| c.to_digit(16))
            .collect();
        for (i, byte) in mac.iter_mut().enumerate() {
            #[allow(clippy::cast_possible_truncation)]
            {
                *byte = ((digits[i * 2] << 4) | digits[i * 2 + 1]) as u8;
            }
        }

        let found = lookup(mac).expect("the MA-S block is assigned");
        assert_eq!(found.organisation, org);
        assert_eq!(found.registry, Registry::MaS);

        let parent = u32::from_str_radix(&hex[..6], 16).expect("hex");
        assert!(
            !tables().bits24.contains_key(&parent),
            "IEEE is not supposed to list the MA-S parent prefix {} as an MA-L block;              if it now does, the most-specific-first order in `lookup` is load-bearing              rather than merely prudent, and this test should say so",
            &hex[..6]
        );
    }

    #[test]
    fn a_cid_says_it_is_not_unique() {
        // Identity resolution must not treat a shared identifier as proof two resources
        // are the same device.
        assert!(!Registry::Cid.is_unique());
        assert_eq!(Registry::Cid.bits(), 24);
        for r in [Registry::MaL, Registry::MaM, Registry::MaS] {
            assert!(r.is_unique());
        }
    }

    #[test]
    fn vendor_of_takes_the_string_a_caller_actually_holds() {
        assert!(
            vendor_of("00:00:0c:11:22:33")
                .expect("Cisco")
                .contains("Cisco")
        );
        assert_eq!(vendor_of("02:00:00:00:00:01"), None);
        assert_eq!(vendor_of("not a mac"), None);
    }
}
