//! Probing a whole sweep, at a pace somebody else's network can live with.
//!
//! # Why the rate limit is not optional
//!
//! A discovery run must not be the reason a customer's network monitoring alerts. That is
//! not politeness: the first thing a security team does with a new tool is watch what it
//! puts on the wire, and a product whose inventory scan trips their own scan detector has
//! failed a test nobody told it it was taking. [`PROBES_PER_SECOND`] is about 15 KB/s of
//! SNMP — beneath notice on any link.
//!
//! [`IN_FLIGHT`] is the second limit and answers a different question. The rate cap is
//! about the estate; the concurrency cap is about the path to it, which is usually a
//! firewall holding connection state for every outstanding UDP probe and with a table
//! size. Sixty-four is comfortable for every one of them; six thousand is not.
//!
//! The two together mean a /16 takes about five and a half minutes. That is the number
//! [`MAX_ADDRESSES`] was chosen against.
//!
//! # Why nothing here fails
//!
//! [`run`] returns [`Findings`] and never an error. An empty address, a refused
//! credential and a broken socket are all *facts about the sweep* and all belong in the
//! same report — a run that aborted at address 9 000 of 65 536 would have learned nothing
//! and would have to start again. Deciding that a run went badly is the caller's, from
//! counters that are all here.
//!
//! [`MAX_ADDRESSES`]: crate::MAX_ADDRESSES

use std::net::SocketAddr;
use std::time::Duration;

use futures_util::StreamExt;
use futures_util::stream;
use tokio::sync::Mutex;
use tokio::time::Instant;
use uops_snmp::transport::Transport;

use crate::probe::{Answer, Sighting, probe};
use crate::sweep::{IN_FLIGHT, PROBES_PER_SECOND, Sweep};

/// What a sweep found.
///
/// Lists rather than counts for everything that becomes a row: a run's counters are what
/// an operator reads, and the candidates are what they act on. `silent` is only ever a
/// number because 246 empty addresses in a /24 is the normal case and storing them would
/// make the candidate list unreadable — an address with nothing on it is not a finding.
#[derive(Clone, Debug, Default)]
pub struct Findings {
    /// Addresses probed. The same as `sweep.len()` unless the run was cut short.
    pub probed: usize,
    /// Agents that answered, in address order.
    pub devices: Vec<Sighting>,
    /// Agents that refused the credentials the job named.
    ///
    /// §2.2: recorded, never retried with a guess. Each becomes a candidate in state
    /// `unreachable`, whose reason tells the operator to supply a credential.
    pub refused: Vec<SocketAddr>,
    /// Addresses where nothing answered.
    pub silent: usize,
    /// Addresses where the transport itself failed, with the reason.
    ///
    /// Distinct from `silent` and the distinction matters at scale: 254 of these is a
    /// broken run, and reporting them as empty addresses would tell an operator their
    /// estate had vanished.
    pub failed: Vec<(SocketAddr, String)>,
}

impl Findings {
    /// Addresses where something was listening.
    ///
    /// A refusal is an answer. An agent that rejected the credentials proved it exists,
    /// which is the whole reason §2.2 can afford to record it rather than guess at it —
    /// and `discovery_run.answered` counts it, so that `probed - answered` is the number
    /// of empty addresses and nothing else.
    #[must_use]
    pub fn answered(&self) -> usize {
        self.devices.len() + self.refused.len()
    }
}

/// Probe every address in `sweep`.
///
/// Concurrency and rate are capped by [`IN_FLIGHT`] and [`PROBES_PER_SECOND`], globally
/// across the run rather than per range — a job with forty ranges must not send forty
/// times the traffic of a job with one.
pub async fn run<T: Transport + ?Sized + Sync>(
    transport: &T,
    sweep: &Sweep,
    port: u16,
) -> Findings {
    let pace = Pace::new(PROBES_PER_SECOND);

    let answers = stream::iter(sweep.targets(port))
        .map(|address| {
            let pace = &pace;
            async move {
                pace.wait_for_a_turn().await;
                (address, probe(transport, address).await)
            }
        })
        .buffer_unordered(IN_FLIGHT)
        .collect::<Vec<_>>()
        .await;

    let mut findings = Findings {
        probed: answers.len(),
        ..Findings::default()
    };
    for (address, answer) in answers {
        match answer {
            Answer::Device(sighting) => findings.devices.push(*sighting),
            Answer::Refused => findings.refused.push(address),
            Answer::Silent => findings.silent += 1,
            Answer::Failed(why) => findings.failed.push((address, why)),
        }
    }

    // `buffer_unordered` yields as probes complete, so an estate where one device is slow
    // would otherwise report its findings in a different order every run. Sorted, because
    // two runs of the same range should produce the same list -- an operator comparing
    // this morning's candidates against last night's is doing it by eye.
    findings.devices.sort_by_key(|s| s.address);
    findings.refused.sort_unstable();
    findings.failed.sort_by_key(|(address, _)| *address);
    findings
}

/// A global pace for a run.
///
/// A token bucket with one token: each probe claims the next slot and waits for it, so
/// the rate is exact across the whole run rather than per-task. The alternative — every
/// task sleeping for `1/rate` before its probe — gives `IN_FLIGHT × rate` in the worst
/// case, which is 64 times the number that was agreed with the customer.
///
/// The lock is held only for the arithmetic, never across the sleep. Holding it across
/// the sleep would serialise the run to one probe at a time and quietly discard
/// [`IN_FLIGHT`] — a bug that looks like nothing at all until somebody times a /16.
#[derive(Debug)]
struct Pace {
    interval: Duration,
    next: Mutex<Instant>,
}

impl Pace {
    fn new(per_second: u32) -> Self {
        Self {
            interval: Duration::from_secs(1) / per_second.max(1),
            next: Mutex::new(Instant::now()),
        }
    }

    async fn wait_for_a_turn(&self) {
        let slot = {
            let mut next = self.next.lock().await;
            let slot = (*next).max(Instant::now());
            *next = slot + self.interval;
            slot
        };
        tokio::time::sleep_until(slot).await;
    }
}
