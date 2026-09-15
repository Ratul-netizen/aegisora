//! A polling interval, written the way an operator writes one.
//!
//! `60s`, `30s`, `5m`. Not a bare number, because a bare number in a YAML file is a
//! number of *something* and the reader has to guess — and the two plausible guesses,
//! seconds and milliseconds, are three orders of magnitude apart.
//!
//! The bounds are the interesting part. An interval is not a free parameter:
//!
//! * Below one second, SNMP is the wrong protocol. A device's agent queue is small and
//!   a sub-second poll will be dropped rather than refused — which looks like an outage
//!   in exactly the way SPEC §M2 warns about.
//! * Above an hour, the series is too sparse for the 5-minute rollups to mean anything,
//!   and an alert on it cannot fire faster than its own interval.
//!
//! A profile outside those bounds is a mistake, and mistakes in a profile are caught at
//! load or not at all.

use std::fmt;
use std::str::FromStr;
use std::time::Duration;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// The fastest a profile may ask for. See the module docs.
pub const MIN: Duration = Duration::from_secs(1);

/// The slowest.
pub const MAX: Duration = Duration::from_secs(3600);

/// A polling interval or timeout.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Interval(Duration);

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum IntervalError {
    #[error("an interval needs a unit: `60s`, `5m`, `500ms` — not `{0}`")]
    NoUnit(String),

    #[error("`{0}` is not a number of {1}")]
    NotANumber(String, &'static str),

    #[error("`{0}` is not a unit — use ms, s, m or h")]
    UnknownUnit(String),

    #[error("{0:?} is faster than the {1:?} floor; SNMP agents drop what they cannot queue")]
    TooFast(Duration, Duration),

    #[error("{0:?} is slower than the {1:?} ceiling; the series would be too sparse to roll up")]
    TooSlow(Duration, Duration),
}

impl Interval {
    /// The duration.
    #[must_use]
    pub const fn duration(self) -> Duration {
        self.0
    }

    #[must_use]
    pub const fn as_secs_f64(self) -> f64 {
        self.0.as_secs_f64()
    }

    /// Build one without the bounds check.
    ///
    /// For a timeout, which is legitimately sub-second — a 2-second ICMP timeout is
    /// normal and a 200 ms one is not unreasonable on a LAN. Named so that using it for
    /// an interval is visible.
    #[must_use]
    pub const fn unbounded(d: Duration) -> Self {
        Self(d)
    }
}

impl FromStr for Interval {
    type Err = IntervalError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let s = s.trim();
        let split = s
            .find(|c: char| !c.is_ascii_digit())
            .ok_or_else(|| IntervalError::NoUnit(s.to_owned()))?;
        let (number, unit) = s.split_at(split);

        // `ms` before `m`: the longer unit has to be tried first or `500ms` is 500
        // minutes with a stray `s`.
        let (millis_per, unit_name) = match unit {
            "ms" => (1_u64, "milliseconds"),
            "s" => (1_000, "seconds"),
            "m" => (60_000, "minutes"),
            "h" => (3_600_000, "hours"),
            other => return Err(IntervalError::UnknownUnit(other.to_owned())),
        };

        let value: u64 = number
            .parse()
            .map_err(|_| IntervalError::NotANumber(number.to_owned(), unit_name))?;
        let millis = value
            .checked_mul(millis_per)
            .ok_or_else(|| IntervalError::NotANumber(number.to_owned(), unit_name))?;

        Ok(Self(Duration::from_millis(millis)))
    }
}

/// Parse and enforce the polling bounds.
///
/// # Errors
///
/// Anything [`Interval::from_str`] rejects, plus an interval outside [`MIN`]–[`MAX`].
pub fn parse_bounded(s: &str) -> Result<Interval, IntervalError> {
    let interval: Interval = s.parse()?;
    if interval.0 < MIN {
        return Err(IntervalError::TooFast(interval.0, MIN));
    }
    if interval.0 > MAX {
        return Err(IntervalError::TooSlow(interval.0, MAX));
    }
    Ok(interval)
}

impl fmt::Display for Interval {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let ms = self.0.as_millis();
        if ms.is_multiple_of(3_600_000) && ms > 0 {
            write!(f, "{}h", ms / 3_600_000)
        } else if ms.is_multiple_of(60_000) && ms > 0 {
            write!(f, "{}m", ms / 60_000)
        } else if ms.is_multiple_of(1_000) {
            write!(f, "{}s", ms / 1_000)
        } else {
            write!(f, "{ms}ms")
        }
    }
}

impl Serialize for Interval {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Interval {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(d)?;
        parse_bounded(&raw).map_err(serde::de::Error::custom)
    }
}

/// A timeout: the same syntax, without the polling floor.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct Timeout(Interval);

impl Timeout {
    #[must_use]
    pub const fn duration(self) -> Duration {
        self.0.duration()
    }
}

impl<'de> Deserialize<'de> for Timeout {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(d)?;
        let interval: Interval = raw.parse().map_err(serde::de::Error::custom)?;
        if interval.duration() > MAX {
            return Err(serde::de::Error::custom(IntervalError::TooSlow(
                interval.duration(),
                MAX,
            )));
        }
        Ok(Self(interval))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_unit_parses() {
        for (input, expected_ms) in [
            ("500ms", 500_u64),
            ("1s", 1_000),
            ("60s", 60_000),
            ("5m", 300_000),
            ("1h", 3_600_000),
        ] {
            let i: Interval = input.parse().unwrap();
            assert_eq!(i.duration().as_millis(), u128::from(expected_ms), "{input}");
        }
    }

    #[test]
    fn ms_is_not_read_as_minutes() {
        // The bug a naive match arm order produces: `500ms` matching `m` and leaving an
        // `s`, or being read as 500 minutes. Eight hours instead of half a second.
        assert_eq!(
            "500ms".parse::<Interval>().unwrap().duration(),
            Duration::from_millis(500)
        );
    }

    #[test]
    fn a_bare_number_is_refused() {
        // Seconds or milliseconds? Both are plausible and they are a thousand times
        // apart, so the profile has to say.
        assert_eq!(
            "60".parse::<Interval>().unwrap_err(),
            IntervalError::NoUnit("60".to_owned())
        );
    }

    #[test]
    fn an_unknown_unit_names_the_alternatives() {
        assert_eq!(
            "60sec".parse::<Interval>().unwrap_err(),
            IntervalError::UnknownUnit("sec".to_owned())
        );
    }

    #[test]
    fn the_polling_bounds_are_enforced_on_intervals() {
        assert!(parse_bounded("1s").is_ok());
        assert!(parse_bounded("1h").is_ok());
        assert!(matches!(
            parse_bounded("999ms"),
            Err(IntervalError::TooFast(..))
        ));
        assert!(matches!(
            parse_bounded("2h"),
            Err(IntervalError::TooSlow(..))
        ));
    }

    #[test]
    fn a_timeout_may_be_sub_second_but_not_unbounded() {
        // 2s ICMP is the SPEC example; 200ms is reasonable on a LAN and must not be
        // refused by the polling floor, which is about agent queues rather than time.
        let t: Timeout = serde_yaml_ng::from_str("\"200ms\"").unwrap();
        assert_eq!(t.duration(), Duration::from_millis(200));
        assert!(serde_yaml_ng::from_str::<Timeout>("\"2h\"").is_err());
    }

    #[test]
    fn display_round_trips_through_parse() {
        for input in ["500ms", "1s", "90s", "5m", "1h"] {
            let parsed: Interval = input.parse().unwrap();
            let shown = parsed.to_string();
            assert_eq!(
                shown.parse::<Interval>().unwrap(),
                parsed,
                "{input} displayed as {shown}"
            );
        }
        // 90s is a minute and a half, and is shown in the unit that keeps it exact.
        assert_eq!("90s".parse::<Interval>().unwrap().to_string(), "90s");
        assert_eq!("120s".parse::<Interval>().unwrap().to_string(), "2m");
    }
}
