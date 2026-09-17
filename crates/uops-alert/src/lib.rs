//! The alert evaluator — SPEC §M4.
//!
//! ```text
//!   schedule   a wheel, one entry per rule, jittered        scheduler.rs
//!   plan       which query, over which window               plan.rs
//!   evaluate   run it, decide each series' phase, write it  engine.rs
//!   run        the loop that does those on a clock          run.rs
//! ```
//!
//! The decision itself is not here. [`uops_core::alert::step`] is a pure function with no
//! database and no clock, and the flapping tests live beside it — this crate is
//! everything that has to happen for `step` to be called with the right arguments.
//!
//! # Why a wheel rather than a task per rule
//!
//! SPEC describes the engine as "one tokio task per rule, jittered". The jitter is the
//! part that matters and a task per rule is one way to get it; [`uops_poll::Wheel`] is
//! another, it already exists, and it already has the argument SPEC §M2 made about a
//! fleet that starts together polling in lockstep. A thousand rules on a sixty-second
//! interval is a thousand tasks that sleep, wake at the same instant and open a thousand
//! `ClickHouse` connections at `:00`; the same thousand in a wheel are spread across the
//! whole minute by construction, because their first firing is placed anywhere in
//! `[0, interval)` from a seed derived from the rule's id.
//!
//! The wheel is generic over its key and says so in its own docs. Scheduling a rule is a
//! different thing to schedule, not a different way of scheduling.
//!
//! # What this crate does not do yet
//!
//! Deliver anything. A decision to notify comes out of [`Engine::evaluate`] as a flag on
//! a [`Decision`]; SMTP and webhooks, their rate limits and the per-tenant notification
//! budget are the next piece. Building the channels before the engine that feeds them
//! would have meant guessing at what a notification contains.

pub mod engine;
pub mod plan;
pub mod run;
pub mod scheduler;

pub use engine::{Cycle, Decision, Engine, RuleOutcome};
pub use plan::{MAX_SERIES, Reading, Series, evaluation_query};
pub use run::{IN_FLIGHT, Window, evaluate_and_deliver, run};
pub use scheduler::{Due, Scheduler, TICK};
