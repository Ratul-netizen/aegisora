//! Maintenance windows — when not to wake somebody up.
//!
//! `ResourceStatus::Maintenance` has existed since migration 0002 and nothing has ever
//! written it. This is what writes it, and more importantly it is what the alert engine
//! consults before it fires.
//!
//! The problem it solves is concrete: somebody reboots 500 switches on a Saturday night,
//! and without this the product pages the on-call engineer 500 times about work they
//! scheduled. An alerting system that cannot be told about planned work is an alerting
//! system people turn off.
//!
//! # What is here and what is not
//!
//! Here: the occurrence arithmetic. *Is this window open at this instant?* — pure, with
//! no database and no clock of its own, which is what makes the awkward cases testable.
//!
//! Not here: storage, which is `uops_store_pg::maintenance`, and the decision to suppress,
//! which belongs to the alert engine in M4. This deliberately stops at *"the window is
//! open and it says to suppress alerts"*; what an alert engine does with that is its own
//! business, and building the suppression before the thing being suppressed exists would
//! be guessing.
//!
//! # Why a timezone and not just two instants
//!
//! A one-off window really is just two instants. A recurring one is not: *"every Saturday
//! 02:00–04:00"* means 02:00 **where the equipment is**, and an installation that stored a
//! UTC offset would silently move its maintenance window by an hour twice a year in every
//! country that observes daylight saving. The window would then either fire alerts during
//! the work or suppress them for an hour afterwards, and both are discovered the hard way.
//!
//! So a window stores an IANA zone name and the local time of day, and each occurrence is
//! resolved in that zone.
//!
//! # The two hard cases, and what this does about them
//!
//! DST transitions make some local times ambiguous and others nonexistent:
//!
//! * **Spring forward.** 02:30 does not happen on the transition date. A window at 02:00
//!   simply has no occurrence that day — [`Occurrence::Skipped`]. Inventing one would mean
//!   suppressing alerts at a time the operator did not choose.
//! * **Fall back.** 02:30 happens twice. The **earlier** one is used, and the window's
//!   duration runs from there — so a one-hour window across a fall-back transition is two
//!   wall-clock hours. That is the safer direction: the alternative suppresses less than
//!   the operator asked for, and an alert storm during scheduled work is exactly what this
//!   exists to prevent.
//!
//! Both are decisions rather than accidents, which is why they are named here and asserted
//! in the tests below.

use chrono::{DateTime, Duration, NaiveTime, TimeZone, Utc, Weekday};
use serde::{Deserialize, Serialize};

/// How often a window comes round.
///
/// Four options and deliberately not a cron expression. A cron string is unreadable in a
/// UI, impossible to render as "every Saturday at 02:00", and invites the fifth-percentile
/// schedule at the cost of the ninety-fifth. When somebody needs one, it goes beside these
/// rather than replacing them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum Recurrence {
    /// Happens once, then never again. The common case: a planned upgrade on a date.
    Once,
    Daily,
    /// On one weekday. `Weekday` is the local one, in the window's zone.
    Weekly {
        weekday: Weekday,
    },
    /// On one day of the month, 1–31.
    ///
    /// A month without that day has no occurrence — the 31st does not exist in April, and
    /// a window silently sliding to the 30th would suppress alerts on a day nobody chose.
    Monthly {
        day: u8,
    },
}

impl Recurrence {
    /// The string the database column stores.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Once => "once",
            Self::Daily => "daily",
            Self::Weekly { .. } => "weekly",
            Self::Monthly { .. } => "monthly",
        }
    }
}

/// What a window suppresses while it is open.
///
/// Two flags rather than one, because they are genuinely different requests. *Suppress
/// notifications* means "keep alerting, keep the history, just do not wake anybody" — which
/// is what an operator wants during work they are watching. *Suppress alerts* means the
/// rule does not fire at all.
///
/// The second is the bigger hammer and the default, because an alert that fired during
/// planned work is still in the history afterwards and somebody has to explain each one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Suppression {
    pub alerts: bool,
    pub notifications: bool,
}

impl Default for Suppression {
    fn default() -> Self {
        Self {
            alerts: true,
            notifications: true,
        }
    }
}

/// What a window applies to.
///
/// Exactly one of the three. A site covers everything at a location, a group covers
/// whatever an operator put in it, and a resource covers one device — which is the shape
/// the review asked for and the reason groups had to exist first.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "target", content = "id")]
pub enum Target {
    Resource(crate::ResourceId),
    Group(crate::ResourceGroupId),
    Site(crate::SiteId),
}

impl Target {
    /// The string the database's `target_kind` column stores.
    #[must_use]
    pub const fn kind(self) -> &'static str {
        match self {
            Self::Resource(_) => "resource",
            Self::Group(_) => "group",
            Self::Site(_) => "site",
        }
    }
}

/// When a window's occurrence begins, or why it has none.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Occurrence {
    At(DateTime<Utc>),
    /// The local time does not exist on this date — a spring-forward transition, or the
    /// 31st of a thirty-day month. See the module docs: inventing one would suppress
    /// alerts at a time nobody chose.
    Skipped,
}

/// What is wrong with a window.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum WindowError {
    #[error("{0:?} is not an IANA timezone name")]
    UnknownTimezone(String),
    #[error("a maintenance window must last at least a minute")]
    TooShort,
    #[error("a maintenance window must be shorter than {MAX_HOURS} hours")]
    TooLong,
    #[error("day {0} is not a day of the month")]
    BadDayOfMonth(u8),
    #[error("a window ends before it starts")]
    EndsBeforeItStarts,
    #[error("a maintenance window needs a reason")]
    NoReason,
}

/// The longest a single occurrence may last.
///
/// Not a technical limit. A window longer than a week is almost always a mistake — a
/// mis-typed end date — and the consequence of that mistake is an estate that stops
/// alerting and nobody noticing, which is the worst failure this feature can have. An
/// operator who genuinely wants a month of silence can say so four times, and will
/// remember doing it.
pub const MAX_HOURS: i64 = 24 * 7;

/// A scheduled quiet period.
///
/// The type the alert engine asks and the type the database stores, minus its identity
/// and its target — see `uops_store_pg::maintenance::MaintenanceWindow` for the whole row.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Schedule {
    /// The first occurrence's start, as an instant. Recurrences are computed from the
    /// *local* time this lands on, in `timezone`.
    pub starts_at: DateTime<Utc>,
    /// How long each occurrence lasts.
    ///
    /// A duration rather than an end instant, because a recurring window's end moves with
    /// its start: storing both would let them disagree after the first occurrence, and
    /// they would, on a DST boundary.
    pub duration_minutes: i64,
    /// An IANA name — `Asia/Dhaka`, `Europe/London`. Not an offset; see the module docs.
    pub timezone: String,
    pub recurrence: Recurrence,
    /// Windows stop recurring after this. `None` means forever, which is what a standing
    /// Saturday-night change window is.
    pub until: Option<DateTime<Utc>>,
}

impl Schedule {
    /// Check everything that can be checked without a database.
    ///
    /// # Errors
    ///
    /// The first problem found, so a form can report one fixable thing.
    pub fn validate(&self) -> Result<(), WindowError> {
        self.zone()?;
        if self.duration_minutes < 1 {
            return Err(WindowError::TooShort);
        }
        if self.duration_minutes > MAX_HOURS * 60 {
            return Err(WindowError::TooLong);
        }
        if let Recurrence::Monthly { day } = self.recurrence
            && !(1..=31).contains(&day)
        {
            return Err(WindowError::BadDayOfMonth(day));
        }
        if self.until.is_some_and(|u| u < self.starts_at) {
            return Err(WindowError::EndsBeforeItStarts);
        }
        Ok(())
    }

    fn zone(&self) -> Result<chrono_tz::Tz, WindowError> {
        self.timezone
            .parse::<chrono_tz::Tz>()
            .map_err(|_| WindowError::UnknownTimezone(self.timezone.clone()))
    }

    #[must_use]
    pub fn duration(&self) -> Duration {
        Duration::minutes(self.duration_minutes)
    }

    /// Is this window open at `at`?
    ///
    /// The question the alert engine asks, and the only one it needs. Returns `false` for
    /// a window whose timezone cannot be parsed — a stored zone that a `chrono-tz` upgrade
    /// stopped recognising should mean *alerts keep working*, never *alerts stop*, and
    /// this is the one place where failing open and failing closed point in opposite
    /// directions.
    #[must_use]
    pub fn is_open_at(&self, at: DateTime<Utc>) -> bool {
        if at < self.starts_at {
            return false;
        }
        if self.until.is_some_and(|u| at > u) {
            return false;
        }
        match self.occurrence_covering(at) {
            Some(Occurrence::At(start)) => at >= start && at < start + self.duration(),
            _ => false,
        }
    }

    /// The occurrence that could contain `at`, if the recurrence produces one.
    ///
    /// "Could" because a window longer than its period overlaps itself; the occurrence
    /// that *starts* on or before `at` is the one checked, and a window that long is
    /// refused at validation anyway.
    fn occurrence_covering(&self, at: DateTime<Utc>) -> Option<Occurrence> {
        let Ok(zone) = self.zone() else {
            return None;
        };
        let local_start = self.starts_at.with_timezone(&zone);
        let time = local_start.time();

        // The candidate date, in the window's own zone. Two candidates rather than one:
        // an occurrence that began yesterday evening can still be open now.
        let today = at.with_timezone(&zone).date_naive();
        for date in [today, today.pred_opt()?] {
            if !self.recurs_on(date, local_start.date_naive()) {
                continue;
            }
            match resolve_local(zone, date, time) {
                Occurrence::At(start) if at >= start => return Some(Occurrence::At(start)),
                // An occurrence later today has not begun, and a skipped date never
                // will. Either way, keep looking at the day before — a window that
                // opened yesterday evening can still be open now.
                Occurrence::At(_) | Occurrence::Skipped => {}
            }
        }
        None
    }

    /// Does this window's recurrence land on `date`?
    fn recurs_on(&self, date: chrono::NaiveDate, first: chrono::NaiveDate) -> bool {
        use chrono::Datelike as _;
        if date < first {
            return false;
        }
        match self.recurrence {
            Recurrence::Once => date == first,
            Recurrence::Daily => true,
            Recurrence::Weekly { weekday } => date.weekday() == weekday,
            Recurrence::Monthly { day } => u32::from(day) == date.day(),
        }
    }
}

/// Turn a local date and time into an instant, in a zone that may not have that local
/// time at all.
///
/// See the module docs for why fall-back takes the earlier of the two and spring-forward
/// produces nothing.
fn resolve_local(zone: chrono_tz::Tz, date: chrono::NaiveDate, time: NaiveTime) -> Occurrence {
    let naive = date.and_time(time);
    match zone.from_local_datetime(&naive) {
        chrono::LocalResult::Single(dt) => Occurrence::At(dt.with_timezone(&Utc)),
        // Fall back: the earlier of the two, so the window opens when the operator
        // expects and runs long rather than short.
        chrono::LocalResult::Ambiguous(earlier, _) => Occurrence::At(earlier.with_timezone(&Utc)),
        // Spring forward: this local time did not happen.
        chrono::LocalResult::None => Occurrence::Skipped,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s)
            .expect("an instant")
            .with_timezone(&Utc)
    }

    fn weekly(start: &str, zone: &str, weekday: Weekday, minutes: i64) -> Schedule {
        Schedule {
            starts_at: at(start),
            duration_minutes: minutes,
            timezone: zone.to_owned(),
            recurrence: Recurrence::Weekly { weekday },
            until: None,
        }
    }

    #[test]
    fn a_one_off_window_opens_and_closes() {
        let w = Schedule {
            starts_at: at("2026-09-19T20:00:00Z"),
            duration_minutes: 120,
            timezone: "Asia/Dhaka".to_owned(),
            recurrence: Recurrence::Once,
            until: None,
        };
        assert!(w.validate().is_ok());

        assert!(!w.is_open_at(at("2026-09-19T19:59:59Z")), "before");
        assert!(
            w.is_open_at(at("2026-09-19T20:00:00Z")),
            "the first instant"
        );
        assert!(w.is_open_at(at("2026-09-19T21:59:59Z")), "inside");
        // Half-open: a window ending at 22:00 is closed at 22:00. Otherwise two
        // back-to-back windows both claim the boundary, and an alert at exactly that
        // instant is suppressed by a window that has ended.
        assert!(
            !w.is_open_at(at("2026-09-19T22:00:00Z")),
            "the end is exclusive"
        );

        // And it does not come back next week.
        assert!(!w.is_open_at(at("2026-09-26T21:00:00Z")));
    }

    #[test]
    fn a_weekly_window_stays_at_the_local_hour_across_a_dst_change() {
        // The reason a window stores a zone and not an offset. London is UTC+1 in
        // September and UTC+0 in November; a window at 02:00 local must be 01:00Z in
        // September and 02:00Z in November. Storing an offset would move the maintenance
        // window by an hour and either alert during the work or stay silent afterwards.
        let w = weekly("2026-09-19T01:00:00Z", "Europe/London", Weekday::Sat, 120);

        // September: 02:00 BST = 01:00 UTC.
        assert!(w.is_open_at(at("2026-09-19T01:30:00Z")));
        assert!(!w.is_open_at(at("2026-09-19T03:30:00Z")));

        // November, after the clocks go back: 02:00 GMT = 02:00 UTC.
        assert!(w.is_open_at(at("2026-11-21T02:30:00Z")));
        assert!(
            !w.is_open_at(at("2026-11-21T01:30:00Z")),
            "01:30 UTC is 01:30 local in November — before the window"
        );
    }

    #[test]
    fn a_local_time_that_does_not_exist_has_no_occurrence() {
        // Spring forward in London 2027: 01:00 local becomes 02:00, so 01:30 never
        // happens. Inventing an occurrence would suppress alerts at a time the operator
        // did not choose.
        let w = Schedule {
            starts_at: at("2027-03-21T01:30:00Z"),
            duration_minutes: 30,
            timezone: "Europe/London".to_owned(),
            recurrence: Recurrence::Daily,
            until: None,
        };

        // The day before the transition, 01:30 local is 01:30 UTC and the window is open.
        assert!(w.is_open_at(at("2027-03-27T01:40:00Z")));

        // On the transition date 01:30 does not exist, so nothing is suppressed.
        for probe in ["2027-03-28T00:40:00Z", "2027-03-28T01:40:00Z"] {
            assert!(
                !w.is_open_at(at(probe)),
                "a nonexistent local time must not open a window: {probe}"
            );
        }
    }

    #[test]
    fn an_ambiguous_local_time_takes_the_earlier_one_and_runs_long() {
        // Fall back in London 2026: 01:00-02:00 local happens twice. A window at 01:30
        // opens at the first 01:30 and its duration runs from there, so a one-hour window
        // covers two wall-clock hours. Deliberately the safer direction — suppressing
        // less than the operator asked for means an alert storm during scheduled work.
        let w = Schedule {
            starts_at: at("2026-10-25T00:30:00Z"),
            duration_minutes: 60,
            timezone: "Europe/London".to_owned(),
            recurrence: Recurrence::Once,
            until: None,
        };

        // The first 01:30 local is 00:30 UTC (still BST).
        assert!(w.is_open_at(at("2026-10-25T00:30:00Z")));
        assert!(w.is_open_at(at("2026-10-25T01:29:00Z")));
        assert!(
            !w.is_open_at(at("2026-10-25T01:31:00Z")),
            "and it still ends an hour after it opened, in real time"
        );
    }

    #[test]
    fn a_window_that_starts_in_the_evening_is_still_open_after_midnight() {
        // The case a one-day lookback exists for. 22:00 Saturday for four hours is open
        // at 01:00 on Sunday, and a naive "does it recur today" check says Sunday is not
        // Saturday and reports the estate as alerting.
        let w = weekly("2026-09-19T16:00:00Z", "Asia/Dhaka", Weekday::Sat, 240);

        // 22:00 Dhaka = 16:00 UTC. Four hours later is 02:00 Sunday local, 20:00 UTC.
        assert!(w.is_open_at(at("2026-09-19T16:30:00Z")), "Saturday evening");
        assert!(
            w.is_open_at(at("2026-09-19T19:30:00Z")),
            "01:30 Sunday local, still inside the Saturday window"
        );
        assert!(!w.is_open_at(at("2026-09-19T20:30:00Z")), "after it closes");
    }

    #[test]
    fn a_monthly_window_skips_months_without_that_day() {
        // The 31st does not exist in April. Sliding to the 30th would suppress alerts on
        // a day nobody chose.
        let w = Schedule {
            starts_at: at("2026-01-31T02:00:00Z"),
            duration_minutes: 60,
            timezone: "UTC".to_owned(),
            recurrence: Recurrence::Monthly { day: 31 },
            until: None,
        };

        assert!(w.is_open_at(at("2026-01-31T02:30:00Z")));
        assert!(w.is_open_at(at("2026-03-31T02:30:00Z")));
        for probe in ["2026-04-30T02:30:00Z", "2026-02-28T02:30:00Z"] {
            assert!(!w.is_open_at(at(probe)), "no 31st in that month: {probe}");
        }
    }

    #[test]
    fn nothing_is_open_before_the_first_occurrence_or_after_until() {
        let w = Schedule {
            starts_at: at("2026-09-19T02:00:00Z"),
            duration_minutes: 60,
            timezone: "UTC".to_owned(),
            recurrence: Recurrence::Daily,
            until: Some(at("2026-09-21T23:59:59Z")),
        };

        assert!(
            !w.is_open_at(at("2026-09-18T02:30:00Z")),
            "before the first"
        );
        assert!(w.is_open_at(at("2026-09-20T02:30:00Z")), "inside the range");
        assert!(!w.is_open_at(at("2026-09-22T02:30:00Z")), "after until");
    }

    #[test]
    fn an_unparseable_timezone_fails_open() {
        // The one place where failing open and failing closed point in opposite
        // directions. A stored zone a chrono-tz upgrade stopped recognising must mean
        // "alerts keep working", never "alerts stop" — the second is an estate that goes
        // quiet and nobody notices.
        let w = Schedule {
            starts_at: at("2026-09-19T02:00:00Z"),
            duration_minutes: 60,
            timezone: "Mars/Olympus_Mons".to_owned(),
            recurrence: Recurrence::Daily,
            until: None,
        };
        assert!(!w.is_open_at(at("2026-09-19T02:30:00Z")));
        assert!(matches!(w.validate(), Err(WindowError::UnknownTimezone(_))));
    }

    #[test]
    fn a_window_longer_than_a_week_is_refused() {
        // Not a technical limit. A month of silence is almost always a mis-typed end
        // date, and the consequence is an estate that stops alerting with nobody
        // noticing — the worst failure this feature can have.
        let w = Schedule {
            starts_at: at("2026-09-19T02:00:00Z"),
            duration_minutes: MAX_HOURS * 60 + 1,
            timezone: "UTC".to_owned(),
            recurrence: Recurrence::Once,
            until: None,
        };
        assert_eq!(w.validate(), Err(WindowError::TooLong));

        assert_eq!(
            Schedule {
                duration_minutes: 0,
                ..w.clone()
            }
            .validate(),
            Err(WindowError::TooShort)
        );
    }

    #[test]
    fn suppressing_notifications_is_not_the_same_as_suppressing_alerts() {
        // Two flags because they are different requests. "Keep alerting, keep the
        // history, just do not wake anybody" is what an operator watching their own work
        // wants; "do not fire at all" is the bigger hammer and the default.
        let default = Suppression::default();
        assert!(default.alerts && default.notifications);

        let watching = Suppression {
            alerts: false,
            notifications: true,
        };
        assert!(!watching.alerts, "the rule still evaluates and records");
    }

    #[test]
    fn a_target_is_exactly_one_thing() {
        // Enforced by the schema's CHECK; asserted here so the kind strings the column
        // stores cannot drift from the enum.
        assert_eq!(
            Target::Resource(crate::ResourceId::nil()).kind(),
            "resource"
        );
        assert_eq!(Target::Group(crate::ResourceGroupId::nil()).kind(), "group");
        assert_eq!(Target::Site(crate::SiteId::nil()).kind(), "site");
    }
}
