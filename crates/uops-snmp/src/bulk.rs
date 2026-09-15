//! `GETBULK`, and what to do when the device says the answer is too big.
//!
//! SPEC §M2:
//!
//! > **GETBULK with `max-repetitions` tuning**, and handle `tooBig` by halving and
//! > retrying (`async-snmp` does this automatically; `snmp2` needs it written).
//!
//! This is that. It is about forty lines and it is worth the space, because the obvious
//! implementation is wrong in two ways that only show up on a real network.
//!
//! # Halving has to terminate
//!
//! A device that answers `tooBig` to *everything* — some agents do this when they are
//! overloaded rather than when the response genuinely would not fit — turns "halve and
//! retry" into a loop that ends at 1 and then either spins or gives up without saying
//! so. [`Repetitions::halve`] returns `None` at 1, and the walk reports the device as
//! failing rather than retrying forever. A dead device must not consume a poller slot
//! indefinitely; SPEC says that about the scheduler and it is the same rule here.
//!
//! # The working value has to be remembered
//!
//! Halving discovers a device's limit in a few round trips. Forgetting it means paying
//! those round trips again on the next poll, and the next, forever — on a 60-second
//! interval that is a permanent tax of several wasted requests per device per minute,
//! on exactly the devices that already told you they are struggling.
//!
//! So the discovered value is kept per device and used as the starting point next time,
//! and it recovers upward slowly: a device that was overloaded for ten minutes should
//! not be stuck at `max-repetitions: 1` until the process restarts.

/// A `max-repetitions` value for `GETBULK`.
///
/// Never zero. A `GETBULK` asking for zero repetitions is a `GETNEXT` with extra steps,
/// and an off-by-one that produces one is a walk that never advances.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Repetitions(u32);

/// What to ask for before a device has told us otherwise.
///
/// 25 rows per request. Large enough that an `ifTable` on a 48-port switch is two or
/// three round trips rather than fifty; small enough to fit a 1500-byte MTU for typical
/// row widths, which is what most agents are actually limited by.
pub const DEFAULT: Repetitions = Repetitions(25);

/// The floor. One repetition is a `GETNEXT`, and every agent can answer that.
pub const MIN: Repetitions = Repetitions(1);

impl Repetitions {
    /// Clamped to at least 1.
    #[must_use]
    pub const fn new(n: u32) -> Self {
        Self(if n == 0 { 1 } else { n })
    }

    #[must_use]
    pub const fn get(self) -> u32 {
        self.0
    }

    /// Half of this, or `None` at the floor.
    ///
    /// `None` is the signal to stop retrying. A device that refuses one repetition is
    /// not going to answer a smaller request, because there isn't one.
    #[must_use]
    pub const fn halve(self) -> Option<Self> {
        if self.0 <= 1 {
            None
        } else {
            Some(Self(self.0 / 2))
        }
    }

    /// One step back towards [`DEFAULT`], for a device that has recovered.
    ///
    /// Additive rather than doubling: the cost of being one step too high is a wasted
    /// round trip and another halving, and doubling walks into that repeatedly. This is
    /// the standard shape — back off fast, recover slowly — for the same reason.
    #[must_use]
    pub const fn recover(self) -> Self {
        if self.0 >= DEFAULT.0 {
            DEFAULT
        } else {
            Self(self.0 + 1)
        }
    }
}

impl Default for Repetitions {
    fn default() -> Self {
        DEFAULT
    }
}

/// What a device has taught us about how much it can answer at once.
///
/// One per device, held across polls. See the module docs on why forgetting it is a
/// permanent tax on the devices least able to afford it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Tuning {
    current: Repetitions,
    /// Consecutive successful requests since the last `tooBig`. Recovery waits for a
    /// few of these so that one lucky response does not undo the backoff.
    clean_runs: u32,
}

/// How many clean requests before trying one more repetition.
const RECOVER_AFTER: u32 = 10;

impl Default for Tuning {
    fn default() -> Self {
        Self {
            current: DEFAULT,
            clean_runs: 0,
        }
    }
}

impl Tuning {
    /// Start from a remembered value — what a poller restores for a known device.
    #[must_use]
    pub const fn starting_at(n: Repetitions) -> Self {
        Self {
            current: n,
            clean_runs: 0,
        }
    }

    /// What to ask for now.
    #[must_use]
    pub const fn current(self) -> Repetitions {
        self.current
    }

    /// Record a `tooBig`, returning what to retry with — or `None` to give up.
    #[must_use]
    pub fn too_big(&mut self) -> Option<Repetitions> {
        self.clean_runs = 0;
        let smaller = self.current.halve()?;
        self.current = smaller;
        Some(smaller)
    }

    /// Record a successful request.
    pub fn succeeded(&mut self) {
        if self.current >= DEFAULT {
            return;
        }
        self.clean_runs += 1;
        if self.clean_runs >= RECOVER_AFTER {
            self.current = self.current.recover();
            self.clean_runs = 0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn halving_stops_at_one_rather_than_looping() {
        // The device that answers tooBig to everything. Without a floor this is a loop;
        // with one it is a failed poll, which is a thing an operator can see.
        let mut t = Tuning::default();
        let mut steps = 0;
        while t.too_big().is_some() {
            steps += 1;
            assert!(steps < 100, "halving did not terminate");
        }
        assert_eq!(t.current(), MIN);
    }

    #[test]
    fn halving_reaches_one_quickly() {
        // 25 → 12 → 6 → 3 → 1. Five requests to find the limit of a device that can
        // only answer one, not twenty-five.
        let mut t = Tuning::default();
        let mut seen = vec![t.current().get()];
        while let Some(n) = t.too_big() {
            seen.push(n.get());
        }
        assert_eq!(seen, vec![25, 12, 6, 3, 1]);
    }

    #[test]
    fn a_device_never_goes_below_one_repetition() {
        // Zero repetitions is a GETNEXT with extra steps, and a walk that asks for it
        // never advances.
        assert_eq!(Repetitions::new(0), MIN);
        assert_eq!(MIN.halve(), None);
        let mut t = Tuning::starting_at(MIN);
        assert_eq!(t.too_big(), None);
        assert_eq!(t.current(), MIN);
    }

    #[test]
    fn the_discovered_value_is_what_the_next_poll_starts_from() {
        // The tax this type exists to avoid: rediscovering the limit every 60 seconds,
        // forever, on the devices that are already struggling.
        let mut t = Tuning::default();
        let _ = t.too_big();
        let _ = t.too_big();
        let learned = t.current();
        assert_eq!(learned.get(), 6);

        let next_poll = Tuning::starting_at(learned);
        assert_eq!(
            next_poll.current(),
            learned,
            "a remembered limit must be the starting point, not a fresh 25"
        );
    }

    #[test]
    fn a_recovered_device_climbs_back_slowly() {
        // A device overloaded for ten minutes must not be stuck at 1 until the process
        // restarts — but it must not jump straight back either, because being one step
        // too high costs a wasted round trip and another halving.
        let mut t = Tuning::starting_at(MIN);
        for _ in 0..RECOVER_AFTER {
            t.succeeded();
        }
        assert_eq!(t.current().get(), 2, "one step, not a doubling");

        for _ in 0..(RECOVER_AFTER * 100) {
            t.succeeded();
        }
        assert_eq!(t.current(), DEFAULT, "and it does get all the way back");
    }

    #[test]
    fn one_lucky_response_does_not_undo_the_backoff() {
        // An overloaded agent answers intermittently. Recovering on the first success
        // would oscillate: tooBig, halve, succeed, climb, tooBig, halve, forever.
        let mut t = Tuning::starting_at(Repetitions::new(4));
        t.succeeded();
        assert_eq!(t.current().get(), 4);
        let _ = t.too_big();
        assert_eq!(t.current().get(), 2);
        // And the clean run counter restarted, so the next success is not the tenth.
        for _ in 0..(RECOVER_AFTER - 1) {
            t.succeeded();
        }
        assert_eq!(t.current().get(), 2);
    }

    #[test]
    fn a_device_at_the_default_stays_there() {
        // No point counting clean runs for a device that is already asking for as much
        // as we ever ask for.
        let mut t = Tuning::default();
        for _ in 0..1_000 {
            t.succeeded();
        }
        assert_eq!(t.current(), DEFAULT);
    }
}
