//! What a device says about the devices next to it — M5 §1, §2.5, §2.6.
//!
//! The most valuable third of discovery and the easiest to underrate. An operator enters
//! one core switch; neighbour discovery finds the estate from it. A CIDR sweep is the
//! fallback for the parts of a network nothing points at.
//!
//! Three sources, in descending order of how much they prove:
//!
//! | | reports | worth |
//! |---|---|---|
//! | **LLDP** | chassis ID, port, name, description | a standard, and a chassis ID is tier-1 |
//! | **CDP** | device ID, port, platform, address | Cisco only, but carries a management address |
//! | **ARP** | an IP and a MAC | that something answered on a subnet, and nothing else |
//!
//! ARP is in the list because operators expect it and because it finds devices that speak
//! no discovery protocol at all. It is not evidence of adjacency: the ARP table of a
//! router with a thousand laptops behind it has a thousand entries, and none of them is a
//! neighbour in any sense worth drawing. §2.5's rule does the work — an ARP sighting
//! becomes a candidate and never an edge.
//!
//! # Why the encoding matters more than the walk
//!
//! `lldpRemChassisId` is an `OCTET STRING` whose meaning is given by a *different column*,
//! `lldpRemChassisIdSubtype`. Read it as text and a MAC-address chassis ID — subtype 4,
//! which is what most switches use — becomes six bytes of binary rendered as mojibake.
//! That value then goes into `discovery_candidate.chassis_id`, gets fingerprinted, and
//! becomes an identifier that matches nothing and can never be matched against. The
//! device is found and is permanently unidentifiable.
//!
//! So every identifier here is decoded by its subtype, and [`render`] is where that
//! happens. It is the part of this module most worth reading.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use uops_profile::Oid;
use uops_snmp::Tuning;
use uops_snmp::transport::{Target, Transport, Value};
use uops_snmp::walk::{self, WalkError};

/// `lldpRemEntry` — IEEE 802.1AB, under the IEEE's own arc rather than the internet's.
const LLDP_REM: &str = "1.0.8802.1.1.2.1.4.1.1";
/// `cdpCacheEntry` — `CISCO-CDP-MIB`.
const CDP_CACHE: &str = "1.3.6.1.4.1.9.9.23.1.2.1.1";
/// `ipNetToMediaEntry` — the ARP table. Deprecated in favour of `ipNetToPhysicalTable`
/// and still the one every device actually populates.
const ARP: &str = "1.3.6.1.2.1.4.22.1";

/// Which protocol said so.
///
/// `Default` is LLDP because it is the standard one. The value is overwritten by every
/// parser before a neighbour leaves this module; the derive exists only so that a
/// [`Neighbour`] can be built up column by column while a table is being walked.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Protocol {
    #[default]
    Lldp,
    Cdp,
    Arp,
}

impl Protocol {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Lldp => "lldp",
            Self::Cdp => "cdp",
            Self::Arp => "arp",
        }
    }
}

/// One thing a device reported next to it.
///
/// Every field is optional because which ones are present is exactly what distinguishes
/// the three protocols, and a `NOT NULL` anywhere here would force one of them to store
/// something it does not know.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Neighbour {
    /// Which table this came out of.
    ///
    /// Carried through to `discovery_candidate.source`, because "where did this come
    /// from" is the question an operator asks before deciding whether to trust an
    /// unexpected device -- and an LLDP neighbour and an ARP entry deserve very
    /// different amounts of trust.
    pub protocol: Protocol,
    pub chassis_id: Option<String>,
    pub port_id: Option<String>,
    pub platform: Option<String>,
    pub sys_name: Option<String>,
    pub sys_descr: Option<String>,
    pub address: Option<IpAddr>,
    pub mac: Option<String>,
}

impl Neighbour {
    /// Whether this is worth recording at all.
    ///
    /// A row with neither an address nor a chassis ID is not a neighbour, it is an empty
    /// index — and the schema refuses it, because its fingerprint would collide with
    /// every other empty row. Agents do produce these: a `lldpRemTable` row whose
    /// neighbour has aged out mid-walk has an index and no columns.
    #[must_use]
    pub const fn is_addressable(&self) -> bool {
        self.address.is_some() || self.chassis_id.is_some()
    }
}

/// What one device's neighbour tables said.
#[derive(Clone, Debug, Default)]
pub struct Neighbours {
    pub lldp: Vec<Neighbour>,
    pub cdp: Vec<Neighbour>,
    pub arp: Vec<Neighbour>,
    /// Which tables could not be read, and why.
    ///
    /// Not an error. A switch that does not run LLDP is not a failure, it is a switch
    /// that does not run LLDP, and a walk that gave up on the first empty table would
    /// find nothing on a Cisco estate that speaks only CDP.
    pub unreadable: Vec<(Protocol, String)>,
}

impl Neighbours {
    /// Everything found, with duplicates collapsed.
    ///
    /// A Cisco switch typically runs LLDP *and* CDP, so the same neighbour appears twice.
    /// LLDP wins: it is the standard, its chassis ID is a tier-1 identifier, and CDP's
    /// `cdpCacheDeviceId` is frequently a hostname with a domain stapled on — a tier-4
    /// identifier wearing a tier-1 field's name.
    ///
    /// CDP entries survive when they carry something LLDP did not, which in practice is
    /// the management address: `lldpRemManAddrTable` is a separate table that many agents
    /// leave empty, and an address is what turns a neighbour into something probeable.
    #[must_use]
    pub fn merged(&self) -> Vec<Neighbour> {
        let mut out: Vec<Neighbour> = self.lldp.clone();

        for cdp in &self.cdp {
            // Matched on the strongest thing both protocols carry. A CDP device ID that
            // equals an LLDP chassis ID or system name is the same device saying hello
            // twice.
            if let Some(existing) = out.iter_mut().find(|n| same_device(n, cdp)) {
                // Fill the gaps rather than overwrite: CDP's address is usually the only
                // one there is, and LLDP's identifiers are the ones worth keeping.
                if existing.address.is_none() {
                    existing.address = cdp.address;
                }
                if existing.platform.is_none() {
                    existing.platform.clone_from(&cdp.platform);
                }
                continue;
            }
            out.push(cdp.clone());
        }

        // ARP last and never merged into anything: an ARP entry proves an address is in
        // use, not that it belongs to the device LLDP just described. Folding one into an
        // LLDP neighbour would attach an address on the strength of a coincidence.
        out.extend(self.arp.iter().cloned());
        out.retain(Neighbour::is_addressable);
        out
    }
}

/// Whether two sightings are the same device.
///
/// Deliberately narrow. A shared `sys_name` is *not* enough — that is the "core-01 in
/// every building" case §2.4 is about — so this matches only on the chassis ID, or on a
/// name when one side has no chassis ID to compare. Both sightings come from the same
/// device's own tables, which is what makes the weaker test safe here and unsafe across
/// devices.
fn same_device(a: &Neighbour, b: &Neighbour) -> bool {
    match (&a.chassis_id, &b.chassis_id) {
        (Some(x), Some(y)) => x.eq_ignore_ascii_case(y),
        _ => match (&a.sys_name, &b.sys_name) {
            (Some(x), Some(y)) => x.eq_ignore_ascii_case(y),
            _ => false,
        },
    }
}

/// Read every neighbour table this device has.
///
/// Never fails as a whole: a table that cannot be read is recorded in
/// [`Neighbours::unreadable`] and the others are still walked. A device that runs one
/// protocol and not the others is the normal case, not an error.
pub async fn neighbours<T: Transport + ?Sized>(
    transport: &T,
    target: &Target,
    tuning: &mut Tuning,
) -> Neighbours {
    let mut found = Neighbours::default();

    match lldp(transport, target, tuning).await {
        Ok(rows) => found.lldp = rows,
        Err(e) => found.unreadable.push((Protocol::Lldp, e.to_string())),
    }
    match cdp(transport, target, tuning).await {
        Ok(rows) => found.cdp = rows,
        Err(e) => found.unreadable.push((Protocol::Cdp, e.to_string())),
    }
    match arp(transport, target, tuning).await {
        Ok(rows) => found.arp = rows,
        Err(e) => found.unreadable.push((Protocol::Arp, e.to_string())),
    }

    found
}

/// `lldpRemTable`.
///
/// # Errors
///
/// A transport failure, or an agent whose table does not end.
pub async fn lldp<T: Transport + ?Sized>(
    transport: &T,
    target: &Target,
    tuning: &mut Tuning,
) -> Result<Vec<Neighbour>, WalkError> {
    let table = oid(LLDP_REM);
    let rows = walk::walk(transport, target, &table, tuning).await?;

    // The index is (timeMark, localPortNum, remIndex) — three arcs, and all three are
    // needed to tell two neighbours on one port apart.
    let mut by_index: std::collections::BTreeMap<Vec<u32>, Neighbour> =
        std::collections::BTreeMap::new();
    // Subtypes arrive as their own columns and are needed to decode two others, so they
    // are collected first rather than relied on to arrive in a helpful order.
    let mut chassis_subtype: std::collections::BTreeMap<Vec<u32>, i64> =
        std::collections::BTreeMap::new();
    let mut port_subtype: std::collections::BTreeMap<Vec<u32>, i64> =
        std::collections::BTreeMap::new();

    for vb in &rows {
        let Some(index) = walk::index_of(&vb.oid, &table) else {
            continue;
        };
        // The column is the first arc of what follows the table OID; the rest is the row
        // index. `lldpRemEntry` is already the entry, so column comes first.
        let Some((&column, row)) = index.split_first() else {
            continue;
        };
        let row = row.to_vec();
        match (column, &vb.value) {
            (4, Value::Integer(n)) => {
                chassis_subtype.insert(row, *n);
            }
            (6, Value::Integer(n)) => {
                port_subtype.insert(row, *n);
            }
            _ => {}
        }
    }

    for vb in &rows {
        let Some(index) = walk::index_of(&vb.oid, &table) else {
            continue;
        };
        let Some((&column, row)) = index.split_first() else {
            continue;
        };
        let row = row.to_vec();
        let entry = by_index.entry(row.clone()).or_default();

        match column {
            5 => {
                entry.chassis_id = bytes(&vb.value)
                    .and_then(|b| render(chassis_subtype.get(&row).copied(), b, Kind::Chassis));
            }
            7 => {
                entry.port_id = bytes(&vb.value)
                    .and_then(|b| render(port_subtype.get(&row).copied(), b, Kind::Port));
            }
            9 => entry.sys_name = text(&vb.value),
            10 => entry.sys_descr = text(&vb.value),
            _ => {}
        }
    }

    Ok(stamp(by_index, Protocol::Lldp))
}

/// `cdpCacheTable`.
///
/// # Errors
///
/// A transport failure, or an agent whose table does not end.
pub async fn cdp<T: Transport + ?Sized>(
    transport: &T,
    target: &Target,
    tuning: &mut Tuning,
) -> Result<Vec<Neighbour>, WalkError> {
    let table = oid(CDP_CACHE);
    let rows = walk::walk(transport, target, &table, tuning).await?;

    let mut by_index: std::collections::BTreeMap<Vec<u32>, Neighbour> =
        std::collections::BTreeMap::new();

    for vb in &rows {
        let Some(index) = walk::index_of(&vb.oid, &table) else {
            continue;
        };
        let Some((&column, row)) = index.split_first() else {
            continue;
        };
        let entry = by_index.entry(row.to_vec()).or_default();

        match column {
            // cdpCacheAddress. Four raw bytes for IPv4 — *not* a dotted string, which is
            // the mistake that turns 10.1.2.3 into four unprintable characters.
            4 => entry.address = bytes(&vb.value).and_then(packed_address),
            // cdpCacheDeviceId. A hostname on most platforms and a serial number on some,
            // which is why it lands in `sys_name` rather than in `chassis_id`: calling it
            // a chassis ID would give a tier-4 value a tier-1 field's authority.
            6 => entry.sys_name = text(&vb.value),
            7 => entry.port_id = text(&vb.value),
            8 => entry.platform = text(&vb.value),
            _ => {}
        }
    }

    Ok(stamp(by_index, Protocol::Cdp))
}

/// `ipNetToMediaTable` — the ARP cache.
///
/// Produces candidates, never edges. See the module docs: a router's ARP table is mostly
/// laptops, and adjacency is not what it records.
///
/// # Errors
///
/// A transport failure, or an agent whose table does not end.
pub async fn arp<T: Transport + ?Sized>(
    transport: &T,
    target: &Target,
    tuning: &mut Tuning,
) -> Result<Vec<Neighbour>, WalkError> {
    let table = oid(ARP);
    let rows = walk::walk(transport, target, &table, tuning).await?;

    let mut by_index: std::collections::BTreeMap<Vec<u32>, Neighbour> =
        std::collections::BTreeMap::new();

    for vb in &rows {
        let Some(index) = walk::index_of(&vb.oid, &table) else {
            continue;
        };
        let Some((&column, row)) = index.split_first() else {
            continue;
        };
        let entry = by_index.entry(row.to_vec()).or_default();

        match column {
            2 => entry.mac = bytes(&vb.value).and_then(mac),
            // ipNetToMediaNetAddress. Also available from the row index, which is
            // (ifIndex, a, b, c, d) -- but read from the column, because an agent that
            // indexes the table unusually is a real thing and the column is authoritative.
            3 => entry.address = address_value(&vb.value),
            _ => {}
        }
    }

    Ok(stamp(by_index, Protocol::Arp)
        .into_iter()
        // An incomplete ARP entry -- an address with no MAC, which is what a pending
        // resolution looks like -- says only that something tried to reach it.
        .filter(|n| n.address.is_some() && n.mac.is_some())
        .filter(is_not_broadcast)
        .collect())
}

/// Finish a table: stamp the protocol and drop the rows that identify nothing.
fn stamp(
    rows: std::collections::BTreeMap<Vec<u32>, Neighbour>,
    protocol: Protocol,
) -> Vec<Neighbour> {
    rows.into_values()
        .filter(Neighbour::is_addressable)
        .map(|mut n| {
            n.protocol = protocol;
            n
        })
        .collect()
}

/// Which encoding a subtype column is describing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Chassis,
    Port,
}

/// Decode an LLDP identifier according to its subtype column.
///
/// The subtypes are not shared between the two fields, which is the trap: subtype 4 is a
/// MAC address for a chassis ID and a *network* address for a port ID. A single table
/// would be wrong half the time, so [`Kind`] chooses.
///
/// An unknown subtype falls back to text. That is right rather than lazy: the enumeration
/// is extensible by specification, and rendering an unrecognised value as its bytes is
/// better than dropping a neighbour entirely.
fn render(subtype: Option<i64>, raw: &[u8], kind: Kind) -> Option<String> {
    let is_mac = match kind {
        // lldpChassisIdSubtype: 4 = macAddress.
        Kind::Chassis => subtype == Some(4),
        // lldpPortIdSubtype: 3 = macAddress.
        Kind::Port => subtype == Some(3),
    };
    if is_mac {
        return mac(raw);
    }

    let is_address = match kind {
        // 5 = networkAddress for a chassis, 4 for a port.
        Kind::Chassis => subtype == Some(5),
        Kind::Port => subtype == Some(4),
    };
    if is_address {
        // A network address here is prefixed with its IANA address-family number, which
        // is why it is not simply four or sixteen bytes.
        return family_address(raw).map(|a| a.to_string());
    }

    let text = String::from_utf8_lossy(raw).trim().to_owned();
    (!text.is_empty()).then_some(text)
}

/// Six bytes as `00:1b:21:3c:4d:5e`.
///
/// Lower case, colon-separated, which is what `macaddr` renders and therefore what
/// comparing against the database will see.
fn mac(raw: &[u8]) -> Option<String> {
    (raw.len() == 6).then(|| {
        raw.iter()
            .map(|b| format!("{b:02x}"))
            .collect::<Vec<_>>()
            .join(":")
    })
}

/// An IANA-family-prefixed address, as LLDP writes them.
fn family_address(raw: &[u8]) -> Option<IpAddr> {
    match raw.split_first() {
        // 1 = ipV4, 2 = ipV6, per the IANA address-family registry. One arm, because
        // the family only says which of the two to expect and the length already
        // distinguishes them -- the byte is checked so that a third family is refused
        // rather than guessed at by length.
        Some((1 | 2, rest)) => packed_address(rest),
        _ => None,
    }
}

/// Four or sixteen raw bytes as an address.
fn packed_address(raw: &[u8]) -> Option<IpAddr> {
    match raw.len() {
        4 => Some(IpAddr::V4(Ipv4Addr::new(raw[0], raw[1], raw[2], raw[3]))),
        16 => {
            let mut octets = [0u8; 16];
            octets.copy_from_slice(raw);
            Some(IpAddr::V6(Ipv6Addr::from(octets)))
        }
        _ => None,
    }
}

/// An address from whichever type the agent chose to answer with.
///
/// `ipNetToMediaNetAddress` is `IpAddress`, which snmp2 may surface as raw bytes; some
/// agents answer with an `OCTET STRING` holding the text instead. Both are accepted,
/// because a neighbour lost to an encoding disagreement is a neighbour lost.
fn address_value(value: &Value) -> Option<IpAddr> {
    let raw = bytes(value)?;
    packed_address(raw).or_else(|| String::from_utf8_lossy(raw).trim().parse().ok())
}

/// Reject the addresses that are in every ARP table and are not devices.
fn is_not_broadcast(n: &Neighbour) -> bool {
    // ff:ff:ff:ff:ff:ff is the broadcast entry, and a multicast MAC (low bit of the first
    // octet set) belongs to a group rather than to a machine. Neither is something to go
    // and probe.
    match n.mac.as_deref() {
        Some(mac) => {
            let first = u8::from_str_radix(mac.get(0..2).unwrap_or("00"), 16).unwrap_or(0);
            first & 1 == 0
        }
        None => true,
    }
}

fn bytes(value: &Value) -> Option<&[u8]> {
    match value {
        Value::Bytes(b) => Some(b),
        _ => None,
    }
}

fn text(value: &Value) -> Option<String> {
    let text = String::from_utf8_lossy(bytes(value)?).trim().to_owned();
    (!text.is_empty()).then_some(text)
}

fn oid(s: &str) -> Oid {
    s.parse().expect("a constant OID must parse")
}
