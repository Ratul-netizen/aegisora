//! The alert state machine — SPEC §M4.
//!
//! `ok → pending → firing → resolved`, and SPEC is blunt about which part matters:
//! *"`pending` is what stops flapping from generating notification storms, and it is the
//! single most important piece of the engine."* So it is built here, on its own, with no
//! database and no clock — every transition is a pure function of the previous state, one
//! boolean, and an instant. That is what makes the storm testable.
//!
//! # The bound `pending` buys
//!
//! A signal sitting on its threshold and crossing it every few seconds produces, without
//! `pending`, one notification per evaluation — twenty a minute on a 3-second interval,
//! and the on-call engineer turns the rule off.
//!
//! With `pending` the arithmetic changes completely, and in a way worth stating exactly:
//! **to fire, the condition must hold continuously for `hold`.** A single evaluation in
//! which it does not resets the clock. So the *most* a flapping signal can produce is one
//! firing per `hold` and one resolution after it — two notifications per `hold`, however
//! fast the underlying signal oscillates. At the default five minutes that is a rate an
//! engineer can live with, and it is why there is no separate rate limiter in this file:
//! the dwell already bounds it.
//!
//! This is also why `pending → ok` notifies **nothing**. Nothing was ever announced, so
//! there is nothing to retract, and a "resolved" for an alert nobody was told about is
//! the purest form of noise there is.
//!
//! # Resolution is immediate, and that is deliberate
//!
//! `firing + clear` resolves at once rather than dwelling for `hold` first. A symmetric
//! dwell was considered and rejected: it delays the all-clear by five minutes, and the
//! storm it would prevent is already prevented — re-firing needs another full `hold` of
//! continuous breach, so the pair cannot repeat faster than that.
//!
//! # What is not here
//!
//! Evaluation. Whether a series is breaching is a question for `ClickHouse` and whether
//! it should have been asked at all is a question for maintenance windows; this file
//! takes the answer as a `bool`. Storage is `uops_store_pg::alerts`.

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

/// How a threshold compares.
///
/// Deliberately its own small enum rather than `uops_query::CompareOp`: a condition is
/// evaluated against a number this crate already holds, not compiled into SQL, and
/// depending on the query crate to say `>` would point `uops-core` at something that
/// depends on it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Comparison {
    Gt,
    Gte,
    Lt,
    Lte,
    Eq,
    Ne,
}

impl Comparison {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Gt => ">",
            Self::Gte => ">=",
            Self::Lt => "<",
            Self::Lte => "<=",
            Self::Eq => "==",
            Self::Ne => "!=",
        }
    }

    /// Whether `value` satisfies this comparison against `bound`.
    #[must_use]
    pub fn holds(self, value: f64, bound: f64) -> bool {
        match self {
            Self::Gt => value > bound,
            Self::Gte => value >= bound,
            Self::Lt => value < bound,
            Self::Lte => value <= bound,
            // Exact equality on a float that came out of an average is almost never what
            // somebody means, but `!=` on a status code or a count is, and refusing the
            // pair would leave `!=` unreachable. The UI is where "== 0.1" gets argued
            // with; the engine does what the rule says.
            Self::Eq => (value - bound).abs() < f64::EPSILON,
            Self::Ne => (value - bound).abs() >= f64::EPSILON,
        }
    }
}

/// What a rule tests, and the parameters that go with it.
///
/// An enum rather than a flat `{ op, value, for }` struct, because an absence rule has no
/// operator and no threshold: writing one as `{ op: '>', value: 0, for: '5m' }` would
/// store two fields that mean nothing and that a UI would have to display. The tagged
/// shape makes the meaningless combination unwritable, which is the same reasoning as the
/// `CHECK` on maintenance recurrences.
///
/// Durations are seconds. A human string — `'5m'` — would need a parser, a formatter and
/// a decision about what `'1mo'` means, all to store a number.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum Condition {
    /// `avg(system.cpu.utilization) > 90 for 5m`.
    Threshold {
        op: Comparison,
        value: f64,
        /// How long the comparison has to hold before the rule fires.
        ///
        /// Zero fires on the first breaching evaluation. Allowed, because "the interface
        /// went down" has no useful dwell — and documented, because it is also the
        /// setting that turns a noisy signal into a pager storm.
        hold_seconds: u32,
    },
    /// No telemetry from a resource for `after_seconds`.
    ///
    /// Covers device-down without a separate mechanism, which is why it is one of the two
    /// rule kinds in v0.1 rather than a feature of the poller.
    Absence { after_seconds: u32 },
}

impl Condition {
    /// The string the `kind` column stores, kept equal to the serde tag by the `CHECK`
    /// in migration 0014.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Threshold { .. } => "threshold",
            Self::Absence { .. } => "absence",
        }
    }

    /// How long a breach must persist before the rule fires.
    ///
    /// **Zero for an absence rule, and that is not an oversight.** `after_seconds` is
    /// already a dwell — "nothing for five minutes" — so adding `pending` on top would
    /// mean ten minutes of silence before anybody is told a device stopped talking, which
    /// is not what the operator asked for and not what SPEC's acceptance criterion
    /// allows: *"an absence rule detects a device stopping telemetry within
    /// `interval + eval_interval`"*.
    #[must_use]
    pub fn hold(self) -> Duration {
        match self {
            Self::Threshold { hold_seconds, .. } => Duration::seconds(i64::from(hold_seconds)),
            Self::Absence { .. } => Duration::zero(),
        }
    }

    /// Whether an observed value breaches this condition.
    ///
    /// For an absence rule the "value" is the age of the newest sample in seconds, which
    /// is what the evaluator can actually measure — there is no telemetry to compare, so
    /// the thing being tested is the size of the hole.
    #[must_use]
    pub fn breached_by(self, observed: f64) -> bool {
        match self {
            Self::Threshold { op, value, .. } => op.holds(observed, value),
            Self::Absence { after_seconds } => observed > f64::from(after_seconds),
        }
    }
}

/// How loud an alert is.
///
/// Three tiers, not the nine syslog severities. These answer *"does this wake somebody
/// up?"* — a routing decision with three real answers — and reusing the log scale would
/// invite rules at `notice` that nobody can explain the handling of.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AlertSeverity {
    /// Worth recording, never worth waking anybody.
    Info,
    /// Looked at during working hours.
    Warning,
    /// Pages.
    Critical,
}

impl AlertSeverity {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Info => "info",
            Self::Warning => "warning",
            Self::Critical => "critical",
        }
    }
}

/// Where one series of one rule currently stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    /// Not breaching. Never notified about.
    Ok,
    /// Breaching, but not for long enough yet. **Nobody has been told.**
    Pending,
    /// Breaching for longer than `hold`. Notified once, on entry.
    Firing,
    /// Was firing and has stopped. Notified once, on entry.
    Resolved,
}

impl Phase {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Pending => "pending",
            Self::Firing => "firing",
            Self::Resolved => "resolved",
        }
    }

    /// Whether this phase is worth a row and a place in the UI.
    ///
    /// `ok` is not: a rule matching five thousand resources that are all healthy would
    /// otherwise write five thousand rows saying nothing happened.
    #[must_use]
    pub const fn is_active(self) -> bool {
        !matches!(self, Self::Ok)
    }
}

/// What an evaluation decided about one series.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Transition {
    pub phase: Phase,
    /// When the current phase began.
    ///
    /// Carried forward unchanged while a phase persists, because `pending` measures from
    /// the **first** breach rather than from the last evaluation — advancing it every
    /// cycle would mean a rule with any dwell at all never fires.
    pub since: DateTime<Utc>,
    /// Whether this transition is one somebody is told about. Exactly the two entries
    /// `firing` and `resolved`, and never a re-entry into either.
    pub notify: bool,
}

/// The whole machine.
///
/// `was` is the stored phase and when it began, or `None` for a series that has never
/// been anything but healthy — which is the common case and deliberately has no row.
///
/// `breaching` is the evaluator's answer for this cycle. `hold` is [`Condition::hold`].
// `pending + clear` and `ok + clear` produce the same transition and are deliberately
// separate arms: one is a flapping signal being absorbed and the other is a healthy series
// staying healthy. Merging them would save four lines and lose the only place the first is
// written down. Same reasoning as `Error::status_code`.
#[allow(clippy::match_same_arms)]
#[must_use]
pub fn step(
    was: Option<(Phase, DateTime<Utc>)>,
    breaching: bool,
    hold: Duration,
    now: DateTime<Utc>,
) -> Transition {
    let (phase, since) = was.unwrap_or((Phase::Ok, now));

    match (phase, breaching) {
        // The first breach starts the clock — or fires immediately when there is no
        // clock to start.
        (Phase::Ok | Phase::Resolved, true) => {
            if hold <= Duration::zero() {
                fire(now)
            } else {
                Transition {
                    phase: Phase::Pending,
                    since: now,
                    notify: false,
                }
            }
        }

        // Still breaching. Fire once the dwell has elapsed, and keep `since` until then.
        (Phase::Pending, true) => {
            if now - since >= hold {
                fire(now)
            } else {
                Transition {
                    phase: Phase::Pending,
                    since,
                    notify: false,
                }
            }
        }

        // Nothing was ever announced, so nothing is retracted. This single arm is what
        // absorbs a flapping signal: every clear evaluation sends it back here and the
        // dwell starts again from zero.
        (Phase::Pending, false) => Transition {
            phase: Phase::Ok,
            since: now,
            notify: false,
        },

        // Firing and still breaching: no second notification. An alert that re-notifies
        // every cycle is the storm this file exists to prevent, arriving by another door.
        (Phase::Firing, true) => Transition {
            phase: Phase::Firing,
            since,
            notify: false,
        },

        (Phase::Firing, false) => Transition {
            phase: Phase::Resolved,
            since: now,
            notify: true,
        },

        // A resolution is announced once and then the series is simply healthy again.
        (Phase::Ok | Phase::Resolved, false) => Transition {
            phase: Phase::Ok,
            since: now,
            notify: false,
        },
    }
}

fn fire(now: DateTime<Utc>) -> Transition {
    Transition {
        phase: Phase::Firing,
        since: now,
        notify: true,
    }
}

/// The key that decides whether two evaluations are about the same alert.
///
/// Rule, resource, and the labels the query grouped by — so `cpu > 90` on `rtr-01` and on
/// `rtr-02` are two alerts, and `interface.errors` on `Gi0/1` and `Gi0/2` of the same
/// device are two more.
///
/// Readable rather than hashed. It appears in a `UNIQUE` constraint, in the API, and in
/// the sentence somebody types into a support ticket; a hex digest would save a hundred
/// bytes a row and cost every one of those. Labels are sorted because a map's iteration
/// order is not a promise, and the same alert must not get two keys because a collector
/// happened to emit its labels in a different order.
#[must_use]
pub fn dedup_key<'a>(
    rule: impl std::fmt::Display,
    resource: impl std::fmt::Display,
    labels: impl IntoIterator<Item = (&'a str, &'a str)>,
) -> String {
    let mut parts: Vec<String> = labels
        .into_iter()
        // `=` and `,` are what separate the parts, so a label value containing one could
        // otherwise make two different label sets produce the same key.
        .map(|(k, v)| format!("{}={}", escape(k), escape(v)))
        .collect();
    parts.sort();

    let mut key = format!("{rule}/{resource}");
    if !parts.is_empty() {
        key.push('/');
        key.push_str(&parts.join(","));
    }
    key
}

fn escape(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('=', "\\=")
        .replace(',', "\\,")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(secs: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(1_700_000_000 + secs, 0).expect("a valid instant")
    }

    const HOLD: Duration = Duration::minutes(5);

    #[test]
    fn a_breach_waits_out_its_dwell_before_anybody_is_told() {
        let start = step(None, true, HOLD, at(0));
        assert_eq!(start.phase, Phase::Pending);
        assert!(!start.notify, "nobody is told about a pending alert");

        // Still pending one second short of the dwell. The boundary is the whole point:
        // `>` here instead of `>=` would make every rule fire one evaluation late, which
        // nobody would ever notice and everybody would be affected by.
        let nearly = step(Some((Phase::Pending, at(0))), true, HOLD, at(299));
        assert_eq!(nearly.phase, Phase::Pending);
        assert_eq!(
            nearly.since,
            at(0),
            "the dwell measures from the first breach"
        );

        let fired = step(Some((Phase::Pending, at(0))), true, HOLD, at(300));
        assert_eq!(fired.phase, Phase::Firing);
        assert!(fired.notify);
    }

    #[test]
    fn a_firing_alert_does_not_notify_again_while_it_keeps_firing() {
        for elapsed in [301, 600, 86_400] {
            let still = step(Some((Phase::Firing, at(300))), true, HOLD, at(elapsed));
            assert_eq!(still.phase, Phase::Firing);
            assert!(!still.notify, "at {elapsed}s");
            assert_eq!(still.since, at(300), "firing since is when it fired");
        }
    }

    #[test]
    fn a_pending_alert_that_clears_tells_nobody() {
        // The arm that absorbs the storm. A "resolved" for an alert nobody was told about
        // is the purest noise there is.
        let cleared = step(Some((Phase::Pending, at(0))), false, HOLD, at(60));
        assert_eq!(cleared.phase, Phase::Ok);
        assert!(!cleared.notify);
    }

    #[test]
    fn a_resolution_is_announced_once() {
        let resolved = step(Some((Phase::Firing, at(300))), false, HOLD, at(600));
        assert_eq!(resolved.phase, Phase::Resolved);
        assert!(resolved.notify);

        let after = step(Some((Phase::Resolved, at(600))), false, HOLD, at(660));
        assert_eq!(after.phase, Phase::Ok);
        assert!(!after.notify, "a resolution is not announced twice");
    }

    #[test]
    fn firing_again_needs_another_full_dwell() {
        // The property the whole design rests on, and the reason resolution does not need
        // a dwell of its own: a resolved alert cannot come back without another `hold` of
        // continuous breach.
        let again = step(Some((Phase::Resolved, at(600))), true, HOLD, at(660));
        assert_eq!(again.phase, Phase::Pending);
        assert!(!again.notify);
    }

    /// SPEC §M4's acceptance criterion, as arithmetic rather than as a deployment.
    ///
    /// A signal crossing its threshold every evaluation for an hour: 1 200 evaluations on
    /// a 3-second interval, every other one breaching. Without `pending` that is 600
    /// notifications. With it, it must be zero — the dwell never completes, because a
    /// single clear evaluation sends it back to `ok`.
    #[test]
    fn a_flapping_signal_produces_no_notifications_at_all() {
        let mut state: Option<(Phase, DateTime<Utc>)> = None;
        let mut notifications = 0;
        let mut breaches = 0;

        for i in 0..1_200 {
            let breaching = i % 2 == 0;
            breaches += i32::from(breaching);
            let t = step(state, breaching, HOLD, at(i * 3));
            notifications += i32::from(t.notify);
            state = Some((t.phase, t.since));
        }

        assert_eq!(
            breaches, 600,
            "the signal really is breaching half the time"
        );
        assert_eq!(
            notifications, 0,
            "600 threshold crossings must not reach anybody"
        );
    }

    /// And the same signal when the breach is real: it fires once and stays fired.
    #[test]
    fn a_sustained_breach_produces_exactly_one_notification() {
        let mut state: Option<(Phase, DateTime<Utc>)> = None;
        let mut notifications = 0;

        for i in 0..1_200 {
            let t = step(state, true, HOLD, at(i * 3));
            notifications += i32::from(t.notify);
            state = Some((t.phase, t.since));
        }

        assert_eq!(notifications, 1);
        assert_eq!(state.expect("a state").0, Phase::Firing);
    }

    /// A signal that is genuinely bad, then good, then bad again, slowly. Each episode is
    /// worth exactly two notifications — and this is the worst case, which is what bounds
    /// the whole engine's output.
    #[test]
    fn the_worst_case_is_two_notifications_per_dwell() {
        let mut state: Option<(Phase, DateTime<Utc>)> = None;
        let mut notifications = 0;

        // Six minutes bad, six minutes good, over two hours: ten episodes.
        for i in 0..2_400i64 {
            let breaching = (i * 3 / 360) % 2 == 0;
            let t = step(state, breaching, HOLD, at(i * 3));
            notifications += i32::from(t.notify);
            state = Some((t.phase, t.since));
        }

        assert_eq!(
            notifications, 20,
            "ten episodes, one firing and one resolution each"
        );
    }

    #[test]
    fn a_rule_with_no_dwell_fires_on_the_first_breach() {
        // "The interface went down" has no useful dwell. Allowed, and the reason the
        // zero case is a branch rather than an accident of the arithmetic.
        let t = step(None, true, Duration::zero(), at(0));
        assert_eq!(t.phase, Phase::Firing);
        assert!(t.notify);
    }

    #[test]
    fn an_absence_rule_does_not_dwell_twice() {
        // `after_seconds` is already "nothing for five minutes". A `pending` on top would
        // mean ten minutes before anybody is told a device stopped talking.
        let absence = Condition::Absence { after_seconds: 300 };
        assert_eq!(absence.hold(), Duration::zero());
        assert!(absence.breached_by(301.0));
        assert!(!absence.breached_by(299.0));
    }

    #[test]
    fn a_threshold_condition_compares_the_way_it_reads() {
        let rule = Condition::Threshold {
            op: Comparison::Gt,
            value: 90.0,
            hold_seconds: 300,
        };
        assert!(rule.breached_by(90.1));
        assert!(!rule.breached_by(90.0), "> 90 is not >= 90");
        assert_eq!(rule.hold(), HOLD);
    }

    #[test]
    fn a_condition_round_trips_through_json_with_its_parameters_attached() {
        // Stored as jsonb and read back by an evaluator that may be a release newer. The
        // tag is what keeps an absence rule from arriving with a threshold's fields.
        let condition = Condition::Threshold {
            op: Comparison::Gte,
            value: 0.95,
            hold_seconds: 600,
        };
        let json = serde_json::to_string(&condition).expect("serialize");
        assert!(json.contains(r#""kind":"threshold""#), "{json}");
        assert_eq!(
            serde_json::from_str::<Condition>(&json).expect("deserialize"),
            condition
        );

        let absence: Condition =
            serde_json::from_str(r#"{"kind":"absence","after_seconds":300}"#).expect("absence");
        assert_eq!(absence.as_str(), "absence");
    }

    #[test]
    fn a_dedup_key_is_stable_whatever_order_the_labels_arrive_in() {
        // Two evaluations of one alert must produce one key. A collector emitting its
        // labels in a different order after a restart would otherwise create a second
        // alert for the same problem — and the first would never resolve.
        let one = dedup_key(
            "rule-1",
            "rtr-01",
            [("interface", "Gi0/1"), ("site", "dhaka")],
        );
        let other = dedup_key(
            "rule-1",
            "rtr-01",
            [("site", "dhaka"), ("interface", "Gi0/1")],
        );
        assert_eq!(one, other);
        assert_eq!(one, "rule-1/rtr-01/interface=Gi0/1,site=dhaka");
    }

    #[test]
    fn a_label_value_cannot_forge_another_series_key() {
        // Without escaping, `{a: "x,b=y"}` and `{a: "x", b: "y"}` are the same string —
        // two different series sharing one alert, which is a silenced alert nobody can
        // explain.
        let forged = dedup_key("r", "d", [("a", "x,b=y")]);
        let real = dedup_key("r", "d", [("a", "x"), ("b", "y")]);
        assert_ne!(forged, real);
    }

    #[test]
    fn only_the_phases_worth_showing_are_active() {
        assert!(!Phase::Ok.is_active(), "a healthy series has no row");
        assert!(Phase::Pending.is_active() && Phase::Firing.is_active());
        assert!(Phase::Resolved.is_active());
    }
}
