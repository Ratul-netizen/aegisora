//! Turning counters into rates, and knowing when not to.
//!
//! SPEC §M2, and it is one of the acceptance criteria:
//!
//! > **Counter wrap handling is mandatory.** 32-bit `ifInOctets` wraps in ~34 seconds on
//! > a 1 Gbps link. Prefer 64-bit `ifHC*` counters; when only 32-bit exists, detect wrap
//! > by `current < previous && delta_t < interval * 2`, and discard rather than emit a
//! > negative rate. Store the raw counter; compute rates at query time from the raw
//! > series. Storing pre-computed rates makes re-interpretation impossible.
//!
//! # Why a wrap cannot be corrected, only detected
//!
//! The obvious repair is to add the counter's width back: `2^32 - previous + current`.
//! It is right exactly once — when the counter wrapped exactly once. On a 10 Gbps link a
//! 32-bit byte counter wraps every 3.4 seconds, so a 60-second interval sees seventeen
//! wraps and the "repair" reports 1/17th of the traffic with total confidence.
//!
//! Nothing in the sample says how many times it went round. So a wrap is a **gap**, not
//! a number: the sample is stored (it is what the device said) and no rate is produced
//! for that pair. A missing point on a graph is a thing an operator can see and ask
//! about. A wrong one is not.
//!
//! # Why rates are computed here and not stored
//!
//! They are not stored at all — this module is what the *query* layer will call, and
//! the raw counter is what goes into `ClickHouse`. A stored rate cannot be recomputed
//! over a different window, cannot be re-derived after a bug is found in this file, and
//! silently bakes in whatever wrap handling was current when it was written.

use std::time::Duration;

/// A counter's width, as the MIB declares it.
///
/// The distinction is the whole point: a 64-bit counter at any plausible line rate does
/// not wrap within the lifetime of the hardware, and a 32-bit one wraps constantly.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Width {
    /// SNMP `Counter32`. Wraps at 2^32.
    Bits32,
    /// SNMP `Counter64` — the `ifHC*` columns. Treated as never wrapping.
    Bits64,
}

impl Width {
    /// The value this counter wraps at.
    #[must_use]
    pub const fn modulus(self) -> u128 {
        match self {
            Self::Bits32 => 1 << 32,
            Self::Bits64 => 1 << 64,
        }
    }
}

/// One reading.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Sample {
    pub value: u64,
    /// Nanoseconds since any fixed origin. Monotonic within a series.
    pub at_nanos: u128,
}

/// What a pair of samples produced.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Delta {
    /// A usable rate, in units per second.
    Rate(f64),

    /// The counter went backwards in a way consistent with wrapping. No rate: see the
    /// module docs on why it cannot be repaired.
    Wrapped,

    /// The counter went backwards further than a wrap explains — an agent restart, a
    /// device reboot, a counter reset, or a re-indexed table where `ifIndex` 3 is now a
    /// different interface. Also no rate, and deliberately a different answer from
    /// `Wrapped` so the two are countable separately.
    Reset,

    /// Two samples at the same instant, or out of order. A division by zero waiting to
    /// happen, and never a real rate.
    NoTime,
}

/// The rate between two samples, or why there isn't one.
///
/// `interval` is what the profile asked for. It is used only for the wrap test SPEC
/// specifies — `delta_t < interval * 2` — which distinguishes "the counter wrapped
/// between two consecutive polls" from "we have not heard from this device in an hour
/// and the number is lower, which tells us nothing".
#[must_use]
pub fn delta(previous: Sample, current: Sample, width: Width, interval: Duration) -> Delta {
    if current.at_nanos <= previous.at_nanos {
        return Delta::NoTime;
    }
    let elapsed_nanos = current.at_nanos - previous.at_nanos;

    if current.value >= previous.value {
        let diff = u128::from(current.value - previous.value);
        return rate(diff, elapsed_nanos);
    }

    // It went backwards. Everything below decides between a wrap and a reset, and
    // neither produces a number.
    //
    // SPEC's condition: a wrap is only a plausible explanation if the samples are close
    // enough together that the counter could have gone round once and not twice. Two
    // intervals of slack, because a poll that was late is normal and a missed cycle
    // should not be reported as a device reset.
    let within_window = elapsed_nanos < interval.as_nanos().saturating_mul(2);
    if !within_window {
        return Delta::Reset;
    }

    // A 64-bit counter does not wrap in two intervals at any rate that exists. If one
    // goes backwards, something restarted it.
    if width == Width::Bits64 {
        return Delta::Reset;
    }

    Delta::Wrapped
}

fn rate(diff: u128, elapsed_nanos: u128) -> Delta {
    // f64 has 53 bits of mantissa and a Counter64 has 64, so a very large delta loses
    // precision here. It is a rate in units per second being put on a graph, and the
    // error is under one part in 2^53 — far below the sampling error of the interval
    // itself. Naming it beats a reader wondering.
    #[allow(clippy::cast_precision_loss)]
    let per_second = (diff as f64) / (elapsed_nanos as f64 / 1e9);
    Delta::Rate(per_second)
}

/// The rate of a whole series, skipping the pairs that have none.
///
/// Returns `(timestamp, rate)` for every consecutive pair that produced one. Points
/// where the counter wrapped or reset are simply absent — a gap in the line, which is
/// what an operator should see.
#[must_use]
pub fn rates(samples: &[Sample], width: Width, interval: Duration) -> Vec<(u128, f64)> {
    samples
        .windows(2)
        .filter_map(|pair| match delta(pair[0], pair[1], width, interval) {
            Delta::Rate(r) => Some((pair[1].at_nanos, r)),
            _ => None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECOND: u128 = 1_000_000_000;

    fn s(value: u64, secs: u128) -> Sample {
        Sample {
            value,
            at_nanos: secs * SECOND,
        }
    }

    fn min() -> Duration {
        Duration::from_secs(60)
    }

    #[test]
    fn an_ordinary_increase_is_a_rate() {
        // 6000 bytes over 60 seconds.
        let d = delta(s(1_000, 0), s(7_000, 60), Width::Bits32, min());
        assert_eq!(d, Delta::Rate(100.0));
    }

    #[test]
    fn a_counter_that_did_not_move_is_a_rate_of_zero() {
        // An idle interface. Zero is a real answer and must not be confused with a gap.
        assert_eq!(
            delta(s(500, 0), s(500, 60), Width::Bits32, min()),
            Delta::Rate(0.0)
        );
    }

    #[test]
    fn a_wrap_produces_no_rate_at_all() {
        // The acceptance criterion: "a 32-bit counter wrap produces no negative rate in
        // any query". Not a corrected one — see the module docs. No rate.
        let previous = s(u64::from(u32::MAX) - 1_000, 0);
        let current = s(500, 60);
        assert_eq!(
            delta(previous, current, Width::Bits32, min()),
            Delta::Wrapped
        );
    }

    #[test]
    fn no_pair_anywhere_can_produce_a_negative_rate() {
        // The criterion stated as a property rather than an example. Every ordering of
        // a handful of interesting counter values, at every plausible spacing.
        let values = [
            0u64,
            1,
            1_000,
            u64::from(u32::MAX) / 2,
            u64::from(u32::MAX) - 1,
            u64::from(u32::MAX),
            u64::from(u32::MAX) + 1,
            u64::MAX / 2,
            u64::MAX,
        ];
        for width in [Width::Bits32, Width::Bits64] {
            for &a in &values {
                for &b in &values {
                    for gap in [1u128, 30, 60, 120, 3600] {
                        if let Delta::Rate(r) = delta(s(a, 0), s(b, gap), width, min()) {
                            assert!(r >= 0.0, "{a} → {b} over {gap}s ({width:?}) gave {r}");
                            assert!(r.is_finite(), "{a} → {b} over {gap}s gave {r}");
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn going_backwards_after_a_long_silence_is_a_reset_not_a_wrap() {
        // SPEC's `delta_t < interval * 2`. An hour with no samples and a lower number
        // tells us nothing: the counter may have gone round fifty times, or the device
        // may have rebooted. Calling it a wrap would imply we knew which.
        let previous = s(u64::from(u32::MAX) - 1_000, 0);
        let current = s(500, 3_600);
        assert_eq!(delta(previous, current, Width::Bits32, min()), Delta::Reset);
    }

    #[test]
    fn a_late_poll_is_still_a_wrap() {
        // One missed cycle is ordinary. Reporting it as a device reset would make every
        // busy poller look like a fleet of rebooting switches.
        let previous = s(u64::from(u32::MAX) - 1_000, 0);
        let current = s(500, 110);
        assert_eq!(
            delta(previous, current, Width::Bits32, min()),
            Delta::Wrapped
        );
    }

    #[test]
    fn a_64_bit_counter_going_backwards_is_always_a_reset() {
        // At 100 Gbps a Counter64 of bytes wraps about every 47 years. If one goes
        // backwards inside two minutes, the agent restarted — and saying "wrapped"
        // would put it in the same bucket as a 34-second ifInOctets rollover, which is
        // a completely different operational problem.
        let previous = s(u64::MAX - 1_000, 0);
        let current = s(500, 60);
        assert_eq!(delta(previous, current, Width::Bits64, min()), Delta::Reset);
    }

    #[test]
    fn two_samples_at_one_instant_are_not_a_rate() {
        assert_eq!(delta(s(1, 5), s(2, 5), Width::Bits32, min()), Delta::NoTime);
        // And out of order, which a merge of two collectors can produce.
        assert_eq!(delta(s(1, 9), s(2, 5), Width::Bits32, min()), Delta::NoTime);
    }

    #[test]
    fn a_series_gaps_where_it_wrapped_and_continues_after() {
        // What the graph looks like: a missing point, not a spike and not a hole in the
        // rest of the series.
        let samples = [
            s(1_000, 0),
            s(7_000, 60),                        // +6000 → 100/s
            s(u64::from(u32::MAX) - 1_000, 120), // a big jump, still forwards
            s(5_000, 180),                       // wrapped
            s(11_000, 240),                      // +6000 → 100/s again
        ];
        let out = rates(&samples, Width::Bits32, min());

        assert_eq!(out.len(), 3, "one pair of five should be dropped: {out:?}");
        assert!((out[0].1 - 100.0).abs() < f64::EPSILON);
        assert!((out[2].1 - 100.0).abs() < f64::EPSILON);
        assert!(out.iter().all(|(_, r)| *r >= 0.0));
        // The wrapped pair ends at t=180; nothing is reported there.
        assert!(!out.iter().any(|(t, _)| *t == 180 * SECOND));
    }

    #[test]
    fn a_single_sample_produces_nothing_rather_than_panicking() {
        assert!(rates(&[s(1, 0)], Width::Bits32, min()).is_empty());
        assert!(rates(&[], Width::Bits32, min()).is_empty());
    }

    #[test]
    fn the_naive_repair_would_be_wrong_and_this_is_why() {
        // Documenting the trade in a test, because the "fix" is the first thing anyone
        // proposes. A 10 Gbps link moves 1.25 GB/s; a 32-bit byte counter holds 4.29 GB
        // and so wraps every 3.4 seconds. Over a 60-second interval it goes round about
        // seventeen times.
        //
        // The repair `2^32 - previous + current` assumes exactly one. It would report
        // one seventeenth of the traffic, with no indication that it had guessed.
        let bytes_per_second = 1.25e9_f64;
        #[allow(clippy::cast_precision_loss)]
        let modulus = Width::Bits32.modulus() as f64;
        let wraps_per_minute = bytes_per_second * 60.0 / modulus;
        assert!(
            wraps_per_minute > 16.0,
            "the premise of this test no longer holds: {wraps_per_minute}"
        );

        // And what the code actually does with it.
        let previous = s(4_000_000_000, 0);
        let current = s(3_000_000_000, 60);
        assert_eq!(
            delta(previous, current, Width::Bits32, min()),
            Delta::Wrapped
        );
    }
}
