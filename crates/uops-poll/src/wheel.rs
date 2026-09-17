//! The scheduler: a time wheel.
//!
//! SPEC §M2 is unusually direct about this one:
//!
//! > **Scheduler is a time wheel, not a `tokio::spawn` per device per interval.** 10 000
//! > devices × 20 metrics × 60s means 200 k tasks/minute; spawning per-poll will thrash.
//!
//! A wheel is an array of slots, one per tick. Everything due at tick *t* sits in slot
//! `t % slots`. Advancing is "take slot *t*, hand back its contents, put each item back
//! where it is next due" — O(due) per tick, with no allocation on the steady path and no
//! ordering work at all. A `BinaryHeap` would also be correct and would pay `log n` per
//! item per cycle, on 200 000 items a minute, forever.
//!
//! # Jitter, and the part that is easy to get wrong
//!
//! SPEC: *"Jitter every schedule by ±10% of interval. Without it, everything polls at
//! `:00` and the box has a 1-second CPU spike per minute."*
//!
//! Per-schedule jitter alone does not fix that. A fleet of ten thousand devices
//! registered during one startup is ten thousand entries in one slot, and ±10% of 60s
//! spreads them over twelve seconds — a narrower spike, at a different place, every
//! minute. What actually spreads a fleet is the **initial** placement: the first poll of
//! each device goes anywhere in `[0, interval)`, deterministically derived from its key.
//!
//! So both happen, and they do different jobs:
//!
//! | | spread | purpose |
//! |---|---|---|
//! | initial placement | the whole interval | break up a fleet that started together |
//! | per-schedule jitter | ±10% | stop two devices that happen to collide from colliding forever |
//!
//! Both are derived from the item's key rather than from a random source, so a restart
//! puts a device back roughly where it was instead of reshuffling the entire fleet into
//! a new set of collisions.

use std::collections::VecDeque;
use std::time::Duration;

/// One scheduled thing: poll this metric on this device, run this check.
///
/// Generic over the key so the wheel has no opinion about what it is scheduling. The
/// poller uses `(ResourceId, metric index)`; the tests use numbers.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Entry<K> {
    key: K,
    /// In ticks. Never zero — [`Wheel::insert`] clamps it.
    interval: u32,
    /// Derived from the key once, so jitter is stable across restarts.
    seed: u64,
}

/// A time wheel.
///
/// `slot_duration` is the resolution; `slots` bounds the longest schedulable interval.
/// An interval longer than the wheel wraps around and would fire early, so [`insert`]
/// refuses it rather than silently doing the wrong thing.
///
/// [`insert`]: Wheel::insert
#[derive(Debug)]
pub struct Wheel<K> {
    slots: Vec<VecDeque<Entry<K>>>,
    slot_duration: Duration,
    /// Absolute ticks since the wheel started. Not `now % slots`: the difference
    /// matters for computing a reschedule that lands more than one revolution away.
    tick: u64,
}

/// Why something could not be scheduled.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum WheelError {
    #[error(
        "an interval of {interval:?} does not fit a wheel that covers {span:?}; it \
         would wrap and fire early"
    )]
    TooLong { interval: Duration, span: Duration },
}

impl<K: Clone> Wheel<K> {
    /// A wheel of `slots` slots, each `slot_duration` long.
    ///
    /// # Panics
    ///
    /// If `slots` is zero or `slot_duration` is zero. Both are programming errors at
    /// construction, not conditions to handle.
    #[must_use]
    pub fn new(slots: usize, slot_duration: Duration) -> Self {
        assert!(slots > 0, "a wheel needs at least one slot");
        assert!(!slot_duration.is_zero(), "a slot cannot be instantaneous");
        Self {
            slots: vec![VecDeque::new(); slots],
            slot_duration,
            tick: 0,
        }
    }

    /// The longest interval this wheel can hold.
    #[must_use]
    pub fn span(&self) -> Duration {
        self.slot_duration * u32::try_from(self.slots.len()).unwrap_or(u32::MAX)
    }

    /// How many items are scheduled.
    #[must_use]
    pub fn len(&self) -> usize {
        self.slots.iter().map(VecDeque::len).sum()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.slots.iter().all(VecDeque::is_empty)
    }

    /// Schedule `key` every `interval`.
    ///
    /// The first firing is placed anywhere in `[0, interval)`, derived from the key. See
    /// the module docs: this, not the ±10%, is what stops a fleet polling in lockstep.
    ///
    /// # Errors
    ///
    /// When `interval` is longer than the wheel's span.
    pub fn insert(&mut self, key: K, interval: Duration, seed: u64) -> Result<(), WheelError> {
        let ticks = self.ticks_for(interval)?;
        let entry = Entry {
            key,
            interval: ticks,
            seed,
        };

        // Anywhere in the interval, not just near now — and uniformly.
        //
        // This used to be `(mix % ticks).max(1)`, which folds offset 0 onto offset 1 and
        // so gives the very next tick twice the share of every other one. On a fleet that
        // reloads its schedule every minute that is a visible spike at a predictable
        // moment, which is the exact thing the wheel exists to prevent. Measured on 1 000
        // rules at a 60-second interval, it put ~33 in the busiest second where an even
        // spread is ~17.
        //
        // `+ 1` instead, so the offset is 1..=ticks: still never zero, and every tick
        // gets the same share.
        let offset = mix(seed, 0) % u64::from(ticks).max(1) + 1;
        let due = self.tick + offset;
        self.place(entry, due);
        Ok(())
    }

    fn ticks_for(&self, interval: Duration) -> Result<u32, WheelError> {
        let span = self.span();
        if interval > span {
            return Err(WheelError::TooLong { interval, span });
        }
        // Rounded to the nearest tick, and never zero: an item due every zero ticks
        // would be handed back forever inside one advance.
        let ticks = (interval.as_nanos() / self.slot_duration.as_nanos().max(1)).max(1);
        Ok(u32::try_from(ticks).unwrap_or(u32::MAX))
    }

    fn place(&mut self, entry: Entry<K>, due_tick: u64) {
        let slot = self.slot_of(due_tick);
        self.slots[slot].push_back(entry);
    }

    /// The slot a tick maps to.
    ///
    /// The cast back to `usize` is exact by construction: the remainder is below
    /// `slots.len()`, which is a `usize` already.
    fn slot_of(&self, tick: u64) -> usize {
        let len = self.slots.len() as u64;
        usize::try_from(tick % len).unwrap_or(0)
    }

    /// Advance one tick and return everything that is now due.
    ///
    /// The returned keys are handed to the executor; the wheel has already rescheduled
    /// them. That ordering is deliberate — a poll that takes ninety seconds must not
    /// delay the next tick, and the wheel is not the thing that stops it twice.
    pub fn advance(&mut self, out: &mut Vec<K>) {
        self.advance_retaining(out, |_| true);
    }

    /// Advance one tick, keeping only the entries `keep` still wants scheduled.
    ///
    /// This is how an entry leaves the wheel. There is no `remove`: entries move between
    /// slots on every revolution, so finding one means scanning, and tracking where each
    /// key currently sits would cost a hash write per due entry per tick — on a poller
    /// that is 200 000 of them a minute, forever, to make a rare removal cheap.
    ///
    /// Dropping at the moment an entry comes due costs nothing and is what both callers
    /// actually want: a device that has been decommissioned and a rule that has been
    /// disabled both stop when the wheel next reaches them.
    ///
    /// **The alternative was a leak, and was one.** Both callers used to filter the
    /// returned keys and leave the entry in the wheel, where it rescheduled itself
    /// forever — a poller with device churn accumulated an entry per retired job for the
    /// life of the process, and paid to move each one every interval. `keep` is what
    /// makes the comment those callers already had ("dropped when it next comes due")
    /// true.
    pub fn advance_retaining(&mut self, out: &mut Vec<K>, mut keep: impl FnMut(&K) -> bool) {
        self.tick += 1;
        let slot = self.slot_of(self.tick);

        // Drained into a local, because rescheduling writes back into the wheel and may
        // legitimately target this same slot — an interval that is an exact multiple of
        // the wheel's span. Iterating a slot while pushing into it would loop forever.
        let due: Vec<Entry<K>> = self.slots[slot].drain(..).collect();

        for entry in due {
            if !keep(&entry.key) {
                continue;
            }
            out.push(entry.key.clone());
            let next = self.tick + self.next_offset(&entry);
            self.place(entry, next);
        }
    }

    /// Where an entry goes next: its interval, ±10%.
    fn next_offset(&self, entry: &Entry<K>) -> u64 {
        let interval = u64::from(entry.interval);

        // ±10%, rounded down. Below ten ticks this is zero and the schedule is exact,
        // which is correct: a 5-tick interval has no meaningful ±10%, and inventing one
        // by rounding up would make a 5s poll a 4s or 6s poll.
        let spread = interval / 10;
        if spread == 0 {
            return interval.max(1);
        }

        // mix() with a per-firing counter so consecutive schedules differ, seeded by the
        // key so the sequence is the same after a restart.
        let magnitude = mix(entry.seed, self.tick) % (spread * 2 + 1);
        let jittered = interval + magnitude - spread;

        // Clamped to one full revolution. An offset larger than the wheel lands at
        // `offset - slots` ticks ahead rather than `offset` — a 10-tick interval on a
        // 10-slot wheel, jittered up to 11, fires one tick later instead of eleven.
        //
        // insert() refuses an interval longer than the span, but jitter is applied
        // afterwards and can push a legal interval over it. Exactly `slots` is fine: it
        // is the same slot, one revolution later.
        jittered.clamp(1, self.slots.len() as u64)
    }
}

/// A small deterministic mixer — `splitmix64`'s finalizer.
///
/// Not a hash of anything security-relevant and not `DefaultHasher`, whose output is
/// explicitly not stable across Rust releases. Jitter has to be reproducible: the same
/// device on the same wheel must land in the same place after a restart, or every
/// upgrade reshuffles the entire fleet into a fresh set of collisions.
fn mix(seed: u64, counter: u64) -> u64 {
    let mut z = seed
        .wrapping_add(counter.wrapping_mul(0x9E37_79B9_7F4A_7C15))
        .wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A wheel with one-second slots covering an hour — what the poller uses.
    fn wheel() -> Wheel<u32> {
        Wheel::new(3600, Duration::from_secs(1))
    }

    /// Run `ticks` ticks, returning the tick each key fired on.
    fn run(w: &mut Wheel<u32>, ticks: u64) -> Vec<(u64, u32)> {
        let mut fired = Vec::new();
        let mut out = Vec::new();
        for t in 1..=ticks {
            out.clear();
            w.advance(&mut out);
            for key in &out {
                fired.push((t, *key));
            }
        }
        fired
    }

    #[test]
    fn an_item_fires_at_about_its_interval() {
        let mut w = wheel();
        w.insert(1, Duration::from_secs(60), 12345).unwrap();

        let fired = run(&mut w, 600);
        let ticks: Vec<u64> = fired.iter().map(|(t, _)| *t).collect();
        assert!(ticks.len() >= 8, "fired {} times in 600s", ticks.len());

        for pair in ticks.windows(2) {
            let gap = pair[1] - pair[0];
            assert!(
                (54..=66).contains(&gap),
                "a 60s interval fired {gap}s apart, outside ±10%"
            );
        }
    }

    #[test]
    fn a_fleet_that_starts_together_does_not_poll_together() {
        // The property SPEC is actually asking for, and the one ±10% jitter alone does
        // not deliver. Ten thousand devices registered in one startup must not all land
        // in one slot — or in twelve.
        let mut w = wheel();
        for id in 0..10_000u32 {
            w.insert(id, Duration::from_secs(60), u64::from(id))
                .unwrap();
        }

        let mut per_tick = vec![0usize; 61];
        let mut out = Vec::new();
        for slot in per_tick.iter_mut().take(61).skip(1) {
            out.clear();
            w.advance(&mut out);
            *slot = out.len();
        }

        // About once each. Not exactly: a device whose first poll lands at second 2 and
        // whose jittered interval is 54 polls again at 56, inside the same window. The
        // property that matters is that the work is spread, not that it is quantised.
        let total: usize = per_tick.iter().sum();
        assert!(
            (10_000..=11_000).contains(&total),
            "{total} polls in the first minute for 10 000 devices on a 60s interval"
        );

        let busiest = *per_tick.iter().max().unwrap();
        let ideal = 10_000 / 60;
        assert!(
            busiest < ideal * 3,
            "busiest second had {busiest} polls against an ideal of {ideal}; the fleet \
             is still bunched"
        );
        assert!(
            per_tick[1..=60].iter().all(|n| *n > 0),
            "every second of the minute should have work: {per_tick:?}"
        );
    }

    #[test]
    fn jitter_is_the_same_after_a_restart() {
        // Derived from the key, not from a random source. Otherwise every upgrade
        // reshuffles ten thousand devices into a fresh set of collisions, and a
        // deployment produces a load spike nobody can explain.
        let first = {
            let mut w = wheel();
            for id in 0..200u32 {
                w.insert(id, Duration::from_secs(60), u64::from(id))
                    .unwrap();
            }
            run(&mut w, 120)
        };
        let second = {
            let mut w = wheel();
            for id in 0..200u32 {
                w.insert(id, Duration::from_secs(60), u64::from(id))
                    .unwrap();
            }
            run(&mut w, 120)
        };
        assert_eq!(first, second);
    }

    #[test]
    fn a_short_interval_is_exact_rather_than_wrongly_jittered() {
        // ±10% of 5 seconds is half a second, and the wheel's resolution is a second.
        // Rounding that up would turn a 5s poll into a 4s or 6s one — a 20% error
        // presented as jitter. Below ten ticks the schedule is exact.
        let mut w = wheel();
        w.insert(1, Duration::from_secs(5), 99).unwrap();

        let ticks: Vec<u64> = run(&mut w, 60).iter().map(|(t, _)| *t).collect();
        for pair in ticks.windows(2) {
            assert_eq!(pair[1] - pair[0], 5, "a 5s interval must be exactly 5s");
        }
    }

    #[test]
    fn an_interval_longer_than_the_wheel_is_refused() {
        // It would wrap and fire early — a 2-hour poll on an hour-long wheel becoming
        // an hourly poll, silently, forever.
        let mut w = wheel();
        let err = w.insert(1, Duration::from_secs(7200), 1).unwrap_err();
        assert!(matches!(err, WheelError::TooLong { .. }));
        assert!(w.is_empty(), "a refused insert must schedule nothing");
    }

    #[test]
    fn an_interval_exactly_the_wheel_span_does_not_loop() {
        // The item reschedules into the slot currently being drained. Iterating that
        // slot while pushing into it is an infinite loop; draining first is not.
        let mut w = Wheel::new(10, Duration::from_secs(1));
        w.insert(1u32, Duration::from_secs(10), 7).unwrap();

        // Every gap is the interval or less, never 1 — the wrap this test exists for.
        let ticks: Vec<u64> = run(&mut w, 100).iter().map(|(t, _)| *t).collect();
        assert!(
            ticks.len() >= 9,
            "fired only {} times in 100 ticks",
            ticks.len()
        );
        for pair in ticks.windows(2) {
            let gap = pair[1] - pair[0];
            assert!(
                (9..=10).contains(&gap),
                "a 10s interval on a 10-slot wheel fired {gap} ticks apart: {ticks:?}"
            );
        }
    }

    #[test]
    fn everything_scheduled_stays_scheduled() {
        // The wheel reschedules before handing work out, so nothing can be lost by an
        // executor that panics, hangs, or is slow.
        let mut w = wheel();
        for id in 0..500u32 {
            w.insert(id, Duration::from_secs(30), u64::from(id))
                .unwrap();
        }
        assert_eq!(w.len(), 500);

        let mut out = Vec::new();
        for _ in 0..1000 {
            out.clear();
            w.advance(&mut out);
        }
        assert_eq!(w.len(), 500, "items were lost or duplicated");
    }

    #[test]
    fn mixed_intervals_each_keep_their_own_rate() {
        let mut w = wheel();
        w.insert(30, Duration::from_secs(30), 1).unwrap();
        w.insert(60, Duration::from_secs(60), 2).unwrap();
        w.insert(300, Duration::from_secs(300), 3).unwrap();

        let fired = run(&mut w, 3000);
        for (key, expected) in [(30u32, 100usize), (60, 50), (300, 10)] {
            let n = fired.iter().filter(|(_, k)| *k == key).count();
            let slack = expected / 5 + 1;
            assert!(
                n.abs_diff(expected) <= slack,
                "key {key} fired {n} times in 3000s, expected about {expected}"
            );
        }
    }

    #[test]
    fn an_entry_the_caller_no_longer_wants_leaves_the_wheel() {
        // The leak this method exists for. Filtering the *returned* keys and leaving the
        // entry in place means it reschedules itself forever: a poller with device churn
        // accumulates an entry per retired job for the life of the process and pays to
        // move each one every interval.
        let mut w = wheel();
        for key in 0..10 {
            w.insert(key, Duration::from_secs(5), u64::from(key))
                .unwrap();
        }
        assert_eq!(w.len(), 10);

        let retired = [3, 4, 5];
        let mut fired = Vec::new();
        // Two intervals, so every entry comes due at least once.
        for _ in 0..12 {
            w.advance_retaining(&mut fired, |k| !retired.contains(k));
        }

        assert_eq!(
            w.len(),
            7,
            "the retired entries are gone, not merely filtered"
        );
        assert!(
            !fired.iter().any(|k| retired.contains(k)),
            "a retired entry must not be handed out either: {fired:?}"
        );
        assert!(
            fired.contains(&0),
            "the entries that were kept still fire: {fired:?}"
        );
    }

    #[test]
    fn keeping_everything_is_what_advance_already_did() {
        let mut a = wheel();
        let mut b = wheel();
        for key in 0..20 {
            a.insert(key, Duration::from_secs(7), u64::from(key))
                .unwrap();
            b.insert(key, Duration::from_secs(7), u64::from(key))
                .unwrap();
        }

        let (mut from_advance, mut from_retaining) = (Vec::new(), Vec::new());
        for _ in 0..50 {
            a.advance(&mut from_advance);
            b.advance_retaining(&mut from_retaining, |_| true);
        }

        assert_eq!(from_advance, from_retaining);
        assert_eq!(a.len(), b.len());
    }
}
