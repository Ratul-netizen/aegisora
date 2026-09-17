//! What a sweep will probe, decided before anything is sent.
//!
//! Pure: no sockets, no clock, no database. A `Sweep` is the list of addresses a run is
//! permitted to touch, and building one is the last moment at which the answer to "is
//! this too big" is cheap. After this point the question costs packets.
//!
//! The limits are the ones migration 0017 also enforces. Two places, deliberately: the
//! schema stops a second caller writing a job this crate would refuse, and this stops a
//! job assembled in memory — an ad-hoc probe, a test, a future API that forgets — from
//! reaching the network without passing the same gate. Neither is redundant, because
//! neither is reached by the other's path.

use std::net::{Ipv4Addr, SocketAddr};
use std::str::FromStr;

/// The most addresses one run may probe.
///
/// 65 536 is a /16, and the number is a judgement rather than a technical ceiling: a
/// sweep of it at [`PROBES_PER_SECOND`] takes about five and a half minutes — see
/// [`IN_FLIGHT`] for why that is only true if the three constants agree — which is
/// long enough to be a scheduled job and short enough that an operator who started it by
/// hand will wait. Ten times that is a different kind of operation and should be ten
/// jobs, because ten jobs can fail, be re-run and be audited separately.
pub const MAX_ADDRESSES: u32 = 65_536;

/// The widest single range, as a prefix length. A /16.
///
/// Distinct from [`MAX_ADDRESSES`] even though one /16 is exactly that many addresses,
/// because the two refuse different mistakes and an operator deserves to be told which
/// one they made. `10.0.0.0/8` is a typo or a misunderstanding; forty /22s is somebody
/// who genuinely has forty sites and needs to be told to split the job.
pub const WIDEST_PREFIX: u8 = 16;

/// Probes per second, across the whole run.
///
/// A discovery run must not be the reason a customer's network monitoring alerts. 200/s
/// is about 15 KB/s of SNMP — beneath notice on any link, and beneath the threshold of
/// every scan detector this has been pointed at.
///
/// This is the limit that is meant to bind. [`IN_FLIGHT`] is sized so that it does.
pub const PROBES_PER_SECOND: u32 = 200;

/// How long one probe waits before the address is called silent.
///
/// Two seconds, and deliberately not `uops_snmp::udp::DEFAULT_TIMEOUT`, which is five.
/// Five is right for a *poll*: a conversation with a device known to exist, where giving
/// up early loses real telemetry. It is wrong for a probe, where the overwhelming
/// majority of addresses have nothing on them and the timeout is therefore the entire
/// cost of the sweep.
///
/// The arithmetic is the whole reason this constant exists, and getting it wrong is easy:
/// a sweep of empty addresses runs at `IN_FLIGHT / PROBE_TIMEOUT` probes per second,
/// *regardless* of [`PROBES_PER_SECOND`]. At the five-second poll timeout and 64 in
/// flight that is 13/s — so a /16 would take 85 minutes while appearing to be rate-capped
/// at 200/s. The two limits have to be chosen against each other or the smaller one binds
/// silently and the larger one is decoration.
///
/// Enforced in [`probe`](crate::probe::probe) rather than left to the transport, so that
/// it holds whatever the caller configured.
pub const PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

/// Probes in flight at once.
///
/// A backstop, not the primary limit. It is sized from the other two:
/// `PROBES_PER_SECOND × PROBE_TIMEOUT` is 400 outstanding probes in the worst case — an
/// entirely empty range — so 512 leaves headroom and lets [`PROBES_PER_SECOND`] be the
/// constraint that actually binds. A /16 then takes about five and a half minutes, which
/// is the number [`MAX_ADDRESSES`] was chosen against.
///
/// Sized *down* by one thing only: the firewall between here and the estate holds state
/// for every outstanding UDP probe and has a table size. 512 states at a 30-second UDP
/// idle timeout is nothing to any real firewall; six thousand would not be.
pub const IN_FLIGHT: usize = 512;

/// Below this prefix length a range has a network and a broadcast address.
///
/// /31 is exempt by RFC 3021: a point-to-point link has two addresses and both are
/// usable, so "skip the first and last" would skip the entire range. /32 is one host.
const HAS_NETWORK_AND_BROADCAST_BELOW: u8 = 31;

/// An IPv4 range, in CIDR.
///
/// IPv4 only, and not as a simplification. Sweeping IPv6 does not mean "the same thing,
/// slower": the smallest subnet anybody assigns is a /64, which is eighteen quintillion
/// addresses, and enumerating one is not an operation that finishes. IPv6 devices are
/// found by walking a neighbour table, which enumerates nothing.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Range {
    /// The network address, with host bits already cleared.
    network: u32,
    prefix: u8,
}

impl Range {
    /// A range from a network address and a prefix length.
    ///
    /// Host bits are cleared rather than refused: `192.168.1.40/24` is what an operator
    /// types when they mean "the network that host is on", and correcting it silently is
    /// what every router CLI does. The schema is stricter, because the `cidr` type is —
    /// and there the value arrived through an API rather than from somebody typing.
    ///
    /// # Errors
    ///
    /// A prefix longer than 32.
    pub fn new(address: Ipv4Addr, prefix: u8) -> Result<Self, SweepError> {
        if prefix > 32 {
            return Err(SweepError::PrefixTooLong { prefix });
        }
        // `u32::MAX << 32` is undefined in C and a panic in debug Rust, so a /0 gets the
        // mask written out rather than computed.
        let mask = if prefix == 0 {
            0
        } else {
            u32::MAX << (32 - prefix)
        };
        Ok(Self {
            network: u32::from(address) & mask,
            prefix,
        })
    }

    /// The addresses in the range, network and broadcast included.
    ///
    /// `u64` because a /0 is 2^32, which a `u32` cannot hold — and a /0 is exactly what
    /// somebody pastes when they mean "everything". Overflowing to zero here would make
    /// the largest possible mistake look like the smallest.
    #[must_use]
    pub const fn addresses(self) -> u64 {
        1u64 << (32 - self.prefix)
    }

    /// The addresses worth probing: everything except the network and the broadcast.
    ///
    /// Two addresses per range, which is 0.8% of a /24 and nothing at all of a /16 — the
    /// saving is not the point. The point is that the broadcast address is the one that
    /// makes every host on the segment answer at once, which looks like a discovery tool
    /// that has found a great many devices and is in fact one that is being shouted at by
    /// the same device several hundred times.
    pub fn hosts(self) -> impl Iterator<Item = Ipv4Addr> {
        let (first, last) = if self.prefix < HAS_NETWORK_AND_BROADCAST_BELOW {
            (
                self.network + 1,
                self.network + u32::try_from(self.addresses() - 2).unwrap_or(u32::MAX),
            )
        } else {
            (
                self.network,
                self.network + u32::try_from(self.addresses() - 1).unwrap_or(u32::MAX),
            )
        };
        (first..=last).map(Ipv4Addr::from)
    }

    #[must_use]
    pub const fn prefix(self) -> u8 {
        self.prefix
    }

    #[must_use]
    pub const fn network(self) -> Ipv4Addr {
        Ipv4Addr::from_bits(self.network)
    }
}

impl std::fmt::Display for Range {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", self.network(), self.prefix)
    }
}

impl FromStr for Range {
    type Err = SweepError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (address, prefix) = s
            .split_once('/')
            .ok_or_else(|| SweepError::NotARange { text: s.to_owned() })?;
        let address: Ipv4Addr = address
            .parse()
            .map_err(|_| SweepError::NotARange { text: s.to_owned() })?;
        let prefix: u8 = prefix
            .parse()
            .map_err(|_| SweepError::NotARange { text: s.to_owned() })?;
        Self::new(address, prefix)
    }
}

/// The addresses one run is permitted to probe.
///
/// Constructing one is the check. There is no way to build a `Sweep` that exceeds the
/// limits and no way to probe an address without one, which is what makes the limit a
/// property of the type rather than a rule somebody has to remember.
#[derive(Clone, Debug)]
pub struct Sweep {
    addresses: Vec<Ipv4Addr>,
}

impl Sweep {
    /// Whether these ranges could be swept, without expanding them.
    ///
    /// Separate from [`Sweep::new`] for one caller: the store validates a job at the
    /// moment somebody saves it, and it does that to give the operator [`SweepError`]'s
    /// sentence rather than the name of a CHECK constraint. Expanding a /16 to answer a
    /// yes-or-no question would allocate 65 534 addresses and sort them, on a code path
    /// that is about to throw all of them away.
    ///
    /// # Errors
    ///
    /// The same three refusals [`Sweep::new`] gives, for the same reasons.
    pub fn check(ranges: &[Range]) -> Result<(), SweepError> {
        if ranges.is_empty() {
            return Err(SweepError::NoRanges);
        }

        // Before the sum, because a /8 is refused for being wide rather than for the
        // total it contributes -- and an operator who typed one should be told that.
        if let Some(&widest) = ranges.iter().min_by_key(|r| r.prefix)
            && widest.prefix < WIDEST_PREFIX
        {
            return Err(SweepError::RangeTooWide { range: widest });
        }

        let total: u64 = ranges.iter().map(|r| r.addresses()).sum();
        if total > u64::from(MAX_ADDRESSES) {
            return Err(SweepError::TooManyAddresses { addresses: total });
        }

        Ok(())
    }

    /// Plan a sweep over `ranges`.
    ///
    /// Overlapping ranges are collapsed rather than refused. An operator who writes
    /// `10.1.0.0/16` and `10.1.5.0/24` has said something redundant, not something wrong,
    /// and probing `10.1.5.7` twice would be a bug in this crate rather than a fact about
    /// their network — it would also double-count it in `discovery_run.probed`, which is
    /// the number they use to judge whether their credentials are right.
    ///
    /// # Errors
    ///
    /// [`SweepError::NoRanges`], [`SweepError::RangeTooWide`] or
    /// [`SweepError::TooManyAddresses`] — the three ways a sweep is refused before it
    /// sends anything.
    pub fn new(ranges: &[Range]) -> Result<Self, SweepError> {
        Self::check(ranges)?;

        let mut addresses: Vec<Ipv4Addr> = ranges.iter().flat_map(|r| r.hosts()).collect();
        addresses.sort_unstable();
        addresses.dedup();

        Ok(Self { addresses })
    }

    /// How many addresses will be probed.
    ///
    /// Not the sum of the ranges: the network and broadcast addresses are gone and the
    /// overlaps are collapsed. This is the number that belongs in `discovery_run.probed`,
    /// and it is the one an operator compares against `answered`.
    #[must_use]
    pub fn len(&self) -> usize {
        self.addresses.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.addresses.is_empty()
    }

    /// Every address, as something to send a packet to.
    pub fn targets(&self, port: u16) -> impl Iterator<Item = SocketAddr> + '_ {
        self.addresses
            .iter()
            .map(move |&a| SocketAddr::from((a, port)))
    }

    #[must_use]
    pub fn addresses(&self) -> &[Ipv4Addr] {
        &self.addresses
    }
}

/// Why a sweep was refused.
///
/// Each message says what to do instead. An operator who has just been told "invalid
/// range" learns nothing; one who has been told to split the job knows their next move,
/// and these strings go to the API unchanged.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum SweepError {
    #[error("a discovery job needs at least one range to scan")]
    NoRanges,

    #[error(
        "{range} is wider than a /{WIDEST_PREFIX} — that is {} addresses, and a sweep of \
         it would take days. Split it into the subnets that are actually in use.",
        .range.addresses()
    )]
    RangeTooWide { range: Range },

    #[error(
        "{addresses} addresses is more than one job may probe ({MAX_ADDRESSES}). Split \
         this into several jobs; they run independently and can be scheduled apart."
    )]
    TooManyAddresses { addresses: u64 },

    #[error("'{text}' is not an IPv4 range — a range looks like 192.168.1.0/24")]
    NotARange { text: String },

    #[error("/{prefix} is not a prefix length; IPv4 prefixes run from 0 to 32")]
    PrefixTooLong { prefix: u8 },
}
