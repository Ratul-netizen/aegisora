//! Polling: the scheduler, and the arithmetic that turns counters into rates.
//!
//! SPEC §M2 calls the poller "the part that determines whether this scales, and the part
//! most likely to be got wrong", and lists what it means by that. Two of those are pure
//! logic with no network in them, which is why they are here and first:
//!
//! * [`wheel`] — a time wheel rather than a task per device per interval, with the
//!   jitter that stops a fleet polling in lockstep.
//! * [`counter`] — wrap detection, because a 32-bit `ifInOctets` wraps in about 34
//!   seconds on a 1 Gbps link and a naive subtraction produces a negative rate.

pub mod counter;
pub mod wheel;

pub use wheel::{Wheel, WheelError};
