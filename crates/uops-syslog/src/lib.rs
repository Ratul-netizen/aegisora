//! Syslog, as it is actually sent.
//!
//! SPEC §M3: *"Support both wire formats — real networks emit both, often from the same
//! vendor."* That sentence is the whole design brief. RFC 5424 has been the standard
//! since 2009 and a great deal of equipment still emits RFC 3164, which was never a
//! standard at all — it was published as an *informational* description of what BSD
//! syslogd happened to do in 2001, and vendors deviate from it freely.
//!
//! # Nothing is ever dropped
//!
//! SPEC is explicit, and it is the rule that shapes every function here:
//!
//! > Parse failures are **never dropped**. An unparseable message is stored with
//! > `severity=unknown`, the raw bytes as `body`, and `attributes['parse.error']` set.
//! > Dropping malformed input loses exactly the messages that matter during an incident.
//!
//! So [`parse`] does not return a `Result`. It always returns a [`Message`]; what varies
//! is how much of it was understood and what [`Message::parse_error`] says. A receiver
//! built on this cannot accidentally discard anything, because there is no error path to
//! discard it in.
//!
//! That is not a stylistic preference. The messages a device emits while it is failing
//! are the ones most likely to be truncated, to have a clock that is wrong, or to come
//! from the one daemon that formats its output badly — and they are the messages
//! somebody is looking for at three in the morning.
//!
//! # What this crate does not do
//!
//! No identity resolution: that needs a database and a cache, and belongs to the daemon
//! that owns both. What is here is everything from the wire to a row — [`rfc5424`] and
//! [`rfc3164`] parse one message, [`framing`] divides a TCP stream into messages,
//! [`receiver`] reads them off a socket, [`normalize`] turns one into a `LogRow`, and
//! [`batch`] accumulates rows into the few large inserts `ClickHouse` wants.
//!
//! The parsers themselves touch no I/O at all, which is what lets both wire formats be
//! tested exhaustively without a network.

pub mod batch;
pub mod framing;
pub mod normalize;
pub mod receiver;
pub mod rfc3164;
pub mod rfc5424;

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};

/// How bad it is, in the vocabulary the `logs` table uses.
///
/// Syslog defines eight severities and the schema has nine: `trace` has no syslog
/// equivalent and exists for OTLP, which does. Nothing here ever produces it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    Emergency,
    Alert,
    Critical,
    Error,
    Warning,
    Notice,
    Informational,
    Debug,
}

impl Severity {
    /// The numeric severity from a PRI, which is always 0–7 once masked.
    #[must_use]
    pub const fn from_code(code: u8) -> Self {
        match code & 0b111 {
            0 => Self::Emergency,
            1 => Self::Alert,
            2 => Self::Critical,
            3 => Self::Error,
            4 => Self::Warning,
            5 => Self::Notice,
            6 => Self::Informational,
            _ => Self::Debug,
        }
    }

    /// The string the `logs` table stores.
    ///
    /// Not the syslog spelling. The schema's enum is shared with OTLP and with events, so
    /// `warning` is stored as `warn` and `informational` as `info` — and a column that
    /// spelled the same severity two ways depending on where it came from would make
    /// every query that filters on it wrong for half the rows.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Emergency => "emergency",
            Self::Alert => "alert",
            Self::Critical => "critical",
            Self::Error => "error",
            Self::Warning => "warn",
            Self::Notice => "notice",
            Self::Informational => "info",
            Self::Debug => "debug",
        }
    }
}

/// Which subsystem sent it, as syslog names them.
///
/// Kept as a number rather than an enum. The well-known sixteen are fixed and the local0
/// through local7 range means whatever a customer decided it means — a vendor shipping
/// firewall logs on local4 is not an error to be corrected, and an enum would invite
/// somebody to correct it.
pub type Facility = u8;

/// One message, as far as it could be understood.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Message {
    pub facility: Facility,
    pub severity: Severity,
    /// When the *device* says it happened. `None` when the message carried no timestamp
    /// or carried one that could not be read.
    ///
    /// Deliberately not defaulted to the time of receipt. A device with a wrong clock is
    /// a real and common thing, and the difference between "this happened at 04:12" and
    /// "this arrived at 04:12" is the difference between a timeline and a guess. The
    /// receiver decides what to do about a missing one; this says what the wire said.
    pub timestamp: Option<DateTime<Utc>>,
    pub hostname: Option<String>,
    /// `TAG` in RFC 3164, `APP-NAME` in RFC 5424.
    pub app_name: Option<String>,
    pub proc_id: Option<String>,
    /// RFC 5424 only.
    pub msg_id: Option<String>,
    /// RFC 5424 structured data, flattened to `id@enterprise.param` keys.
    ///
    /// Flattened because that is the shape `attributes` has, and because the alternative
    /// — a nested map — would have to be flattened by every consumer instead of once
    /// here.
    pub structured_data: BTreeMap<String, String>,
    /// The human-readable part.
    pub message: String,
    /// What could not be read, if anything. `None` means the message parsed cleanly.
    ///
    /// A `Some` here is not a reason to discard the message — see the module docs. It is
    /// what `attributes['parse.error']` is set from, so an operator can find the
    /// malformed ones and a vendor can be told what their firmware emits.
    pub parse_error: Option<&'static str>,
}

impl Message {
    /// Everything unknown, with the raw text as the body.
    ///
    /// The last resort, and the reason this crate cannot lose a message. Facility 1
    /// (`user`) and severity `notice` are what RFC 3164 §4.3.3 prescribes for a message
    /// with no readable priority, so this is the documented behaviour rather than a
    /// number chosen here.
    #[must_use]
    pub fn unparseable(raw: &str, why: &'static str) -> Self {
        Self {
            facility: 1,
            severity: Severity::Notice,
            timestamp: None,
            hostname: None,
            app_name: None,
            proc_id: None,
            msg_id: None,
            structured_data: BTreeMap::new(),
            message: raw.to_owned(),
            parse_error: Some(why),
        }
    }
}

/// Parse one message, whichever format it is in.
///
/// Never fails. See the module docs.
///
/// # How the format is decided
///
/// RFC 5424 puts a version number immediately after the priority — `<34>1 ` — and RFC
/// 3164 puts a timestamp there. A digit followed by a space is therefore a version, and
/// nothing else can be. That is the dispatch, and it is reliable in a way that sniffing
/// the timestamp shape is not: `<34>1 2003-10-11T22:14:15Z` and `<34>Oct 11 22:14:15`
/// differ in the first byte after the priority.
#[must_use]
pub fn parse(raw: &str) -> Message {
    let trimmed = raw.trim_end_matches(['\r', '\n']);
    if trimmed.is_empty() {
        return Message::unparseable(raw, "empty message");
    }

    let Some((priority, rest)) = priority(trimmed) else {
        // No `<n>` at all. Plenty of things write to a syslog socket without one — an
        // application logging with `logger -n`, a device with broken firmware — and the
        // text is still the text.
        return Message::unparseable(trimmed, "no priority");
    };

    // RFC 5424's VERSION is a non-zero digit followed by a space. RFC 5424 §6.2.2 fixes
    // it at `1`; a future version would still match this shape, and dispatching on the
    // shape rather than on the literal `1` means a version 2 message is parsed as 5424
    // and reports what it could not read, instead of being mistaken for a BSD one.
    let mut chars = rest.chars();
    match (chars.next(), chars.next()) {
        (Some(v), Some(' ')) if v.is_ascii_digit() && v != '0' => {
            rfc5424::parse(priority, &rest[2..])
        }
        _ => rfc3164::parse(priority, rest),
    }
}

/// `<n>` at the start, and what follows it.
///
/// Returns `None` for anything that is not a well-formed priority: a missing `<`, a
/// missing `>`, a non-numeric body, or a value above 191 — which is the largest a
/// priority can be, since facility is 0–23 and severity 0–7.
fn priority(s: &str) -> Option<(u8, &str)> {
    let rest = s.strip_prefix('<')?;
    let end = rest.find('>')?;
    let digits = &rest[..end];

    // Leading zeros are refused. RFC 5424 §6.2.1 says the priority "MUST NOT" contain
    // them, and accepting `<034>` would mean accepting a message whose sender is
    // demonstrably not following the spec in a way that suggests the rest of it is not
    // to be trusted either — better to keep the text and flag it than to read half of it
    // confidently.
    if digits.is_empty() || (digits.len() > 1 && digits.starts_with('0')) {
        return None;
    }
    let value: u16 = digits.parse().ok()?;
    if value > 191 {
        return None;
    }
    #[allow(clippy::cast_possible_truncation)]
    Some((value as u8, &rest[end + 1..]))
}

/// Split a priority into its facility and severity.
#[must_use]
pub const fn decode(priority: u8) -> (Facility, Severity) {
    (priority >> 3, Severity::from_code(priority))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_priority_splits_into_facility_and_severity() {
        // The canonical example from RFC 5424 §6.2.1: facility 4 (auth), severity 2
        // (critical), so 4 * 8 + 2 = 34.
        assert_eq!(decode(34), (4, Severity::Critical));
        // Facility 0 (kern), severity 0 (emergency).
        assert_eq!(decode(0), (0, Severity::Emergency));
        // The largest legal value: facility 23 (local7), severity 7 (debug).
        assert_eq!(decode(191), (23, Severity::Debug));
    }

    #[test]
    fn a_malformed_priority_keeps_the_message() {
        // Each of these loses the priority and none of them loses the text. That is the
        // rule this crate exists to hold: the messages a device emits while it is failing
        // are the ones most likely to be malformed.
        for raw in [
            "no priority here",
            "<>empty",
            "<abc>not a number",
            "<192>too large",
            "<034>leading zero",
            "<34 unterminated",
        ] {
            let m = parse(raw);
            assert!(m.parse_error.is_some(), "{raw:?} should not have parsed");
            assert_eq!(m.message, raw, "the text must survive: {raw:?}");
        }
    }

    #[test]
    fn the_largest_and_smallest_priorities_are_accepted() {
        assert_eq!(priority("<0>x"), Some((0, "x")));
        assert_eq!(priority("<191>x"), Some((191, "x")));
        assert_eq!(priority("<192>x"), None);
    }

    #[test]
    fn a_version_digit_chooses_rfc_5424() {
        // The dispatch. A digit-then-space after the priority is a VERSION and cannot be
        // anything else; a month name is RFC 3164.
        let five = parse("<34>1 2003-10-11T22:14:15.003Z host app 1 ID47 - msg");
        assert_eq!(five.msg_id.as_deref(), Some("ID47"));
        assert_eq!(five.parse_error, None);

        let three = parse("<34>Oct 11 22:14:15 host app: msg");
        assert_eq!(three.msg_id, None, "RFC 3164 has no MSGID");
        assert_eq!(three.app_name.as_deref(), Some("app"));
    }

    #[test]
    fn an_empty_message_is_still_a_message() {
        assert!(parse("").parse_error.is_some());
        assert!(parse("\n").parse_error.is_some());
    }

    #[test]
    fn severity_is_stored_the_way_the_schema_spells_it() {
        // `warning` and `informational` are syslog's names; the column is shared with
        // OTLP and events, and a severity spelled two ways would make every filter on it
        // wrong for half the rows.
        assert_eq!(Severity::Warning.as_str(), "warn");
        assert_eq!(Severity::Informational.as_str(), "info");
        assert_eq!(Severity::Emergency.as_str(), "emergency");
    }

    #[test]
    fn severity_orders_worst_first() {
        // Emergency is 0 and debug is 7, so the derived ordering is "more severe is
        // less". Asserted because an alert rule comparing severities depends on it and
        // the direction is easy to assume backwards.
        assert!(Severity::Emergency < Severity::Debug);
        assert!(Severity::Error < Severity::Warning);
    }
}
