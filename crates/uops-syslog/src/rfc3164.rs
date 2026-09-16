//! RFC 3164 — the one everybody actually sends.
//!
//! ```text
//! <34>Oct 11 22:14:15 mymachine su[1234]: 'su root' failed for lonvick
//!  |  |                |         |  |     |
//!  PRI TIMESTAMP       HOSTNAME  |  PID   MSG
//!                                TAG
//! ```
//!
//! RFC 3164 was never a standard. It is an *informational* document published in 2001
//! describing what BSD syslogd happened to do, and it says so in its own abstract. Every
//! vendor deviates: some omit the hostname, some omit the tag, some use a different
//! timestamp, and some send the whole thing with no header at all.
//!
//! So this parses what it can and keeps the rest as the message. The alternative —
//! insisting on the shape the document describes — would reject a large fraction of what
//! a real network emits, and SPEC is explicit that nothing is dropped.
//!
//! # The timestamp has no year and no timezone
//!
//! `Oct 11 22:14:15` and nothing else. Two consequences, both handled here and neither
//! satisfying:
//!
//! * **The year is inferred.** Almost always the current one — but a message sent at
//!   23:59 on 31 December and parsed at 00:01 on 1 January would be dated eleven months
//!   in the future. So a timestamp that would land more than a day ahead is read as last
//!   year's.
//! * **The offset is assumed to be UTC.** There is nothing on the wire to do better with.
//!   A device in Dhaka sending local time is an hour-six error that no amount of parsing
//!   can fix, and the honest fix is RFC 5424, which carries an offset. This is recorded
//!   rather than hidden: see `Message::timestamp`'s own documentation on why the receipt
//!   time is kept separately.

use chrono::{Datelike, NaiveDate, TimeZone, Utc};

use crate::{Message, decode};

/// Parse everything after `<PRI>`.
pub(crate) fn parse(priority: u8, rest: &str) -> Message {
    parse_at(priority, rest, Utc::now())
}

/// The same, with the clock injected.
///
/// Year inference depends on "now", so a test that could not choose it would be one that
/// behaves differently every December — and the December behaviour is the one worth
/// testing.
pub(crate) fn parse_at(priority: u8, rest: &str, now: chrono::DateTime<Utc>) -> Message {
    let (facility, severity) = decode(priority);
    let mut message = Message {
        facility,
        severity,
        timestamp: None,
        hostname: None,
        app_name: None,
        proc_id: None,
        msg_id: None,
        structured_data: std::collections::BTreeMap::new(),
        message: rest.to_owned(),
        parse_error: None,
    };

    // The timestamp is exactly fifteen characters: `Mmm dd hh:mm:ss`, with the day
    // space-padded rather than zero-padded. Fixed width is what makes it findable at all,
    // since nothing separates it from the hostname but a space.
    let Some(stamp) = rest.get(..15) else {
        message.parse_error = Some("rfc3164: too short for a timestamp");
        return message;
    };

    let Some(timestamp) = timestamp_of(stamp, now) else {
        // A vendor that sends something else here. Common enough that it is not worth a
        // louder complaint than this: the header is not read, the whole line is the
        // message, and the text is intact.
        message.parse_error = Some("rfc3164: unreadable timestamp");
        return message;
    };
    message.timestamp = Some(timestamp);

    let after = rest.get(16..).unwrap_or("");
    let (hostname, after) = token(after);
    message.hostname = hostname.map(ToOwned::to_owned);

    // TAG is up to 32 alphanumerics terminated by a non-alphanumeric — usually `:` or
    // `[`. It is *not* space-delimited: `su[1234]:` is one field, and splitting on space
    // would put the pid in the message.
    let (app, proc_id, body) = tag(after);
    message.app_name = app.map(ToOwned::to_owned);
    message.proc_id = proc_id.map(ToOwned::to_owned);
    body.trim_start().clone_into(&mut message.message);

    if message.hostname.is_none() {
        message.parse_error = Some("rfc3164: no hostname");
    }

    message
}

/// One space-delimited token, and what follows it.
fn token(s: &str) -> (Option<&str>, &str) {
    let s = s.trim_start();
    if s.is_empty() {
        return (None, "");
    }
    match s.find(' ') {
        Some(i) => (Some(&s[..i]), &s[i + 1..]),
        None => (Some(s), ""),
    }
}

/// `app[pid]:` or `app:` or nothing.
///
/// Returns the tag, the pid and the rest. A message with no recognisable tag keeps all of
/// its text — a great deal of equipment sends none, and inventing one would be worse than
/// leaving it empty.
fn tag(s: &str) -> (Option<&str>, Option<&str>, &str) {
    let s = s.trim_start();
    let end = s
        .find(|c: char| !c.is_ascii_alphanumeric() && c != '_' && c != '-' && c != '.')
        .unwrap_or(s.len());

    // RFC 3164 §4.1.3 caps the tag at 32 characters. Beyond that it is not a tag, and
    // the most likely explanation is a message with no tag at all whose first word is
    // long.
    if end == 0 || end > 32 {
        return (None, None, s);
    }
    let name = &s[..end];
    let rest = &s[end..];

    if let Some(open) = rest.strip_prefix('[')
        && let Some(close) = open.find(']')
    {
        let pid = &open[..close];
        let after = open[close + 1..]
            .strip_prefix(':')
            .unwrap_or(&open[close + 1..]);
        return (Some(name), Some(pid), after);
    }
    if let Some(after) = rest.strip_prefix(':') {
        return (Some(name), None, after);
    }

    // A word that is not followed by `:` or `[` is not a tag. Leave it in the message.
    (None, None, s)
}

/// `Mmm dd hh:mm:ss`, with the year inferred.
fn timestamp_of(stamp: &str, now: chrono::DateTime<Utc>) -> Option<chrono::DateTime<Utc>> {
    let bytes = stamp.as_bytes();
    if bytes.len() != 15 {
        return None;
    }

    let month = match &stamp[..3] {
        "Jan" => 1,
        "Feb" => 2,
        "Mar" => 3,
        "Apr" => 4,
        "May" => 5,
        "Jun" => 6,
        "Jul" => 7,
        "Aug" => 8,
        "Sep" => 9,
        "Oct" => 10,
        "Nov" => 11,
        "Dec" => 12,
        _ => return None,
    };
    if bytes[3] != b' ' {
        return None;
    }

    // The day is space-padded — `Oct  1`, not `Oct 01` — but plenty of senders zero-pad
    // it anyway. Both are read.
    let day: u32 = stamp[4..6].trim_start().parse().ok()?;
    if bytes[6] != b' ' || bytes[9] != b':' || bytes[12] != b':' {
        return None;
    }
    let hour: u32 = stamp[7..9].parse().ok()?;
    let minute: u32 = stamp[10..12].parse().ok()?;
    let second: u32 = stamp[13..15].parse().ok()?;

    let build = |year: i32| {
        NaiveDate::from_ymd_opt(year, month, day)
            .and_then(|d| d.and_hms_opt(hour, minute, second))
            .and_then(|dt| Utc.from_local_datetime(&dt).single())
    };

    let candidate = build(now.year())?;
    // More than a day ahead means the year rolled over between sending and parsing: a
    // message from 23:59 on 31 December, read at 00:01 on 1 January, would otherwise be
    // dated eleven months in the future and sort to the end of every search.
    //
    // A day of slack rather than none, because device clocks are routinely a few minutes
    // fast and a receiver that re-dated those to last year would be worse than one that
    // accepted them.
    if candidate > now + chrono::Duration::days(1) {
        return build(now.year() - 1);
    }
    Some(candidate)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Severity, parse as parse_any};

    fn at(s: &str) -> chrono::DateTime<Utc> {
        chrono::DateTime::parse_from_rfc3339(s)
            .expect("a test instant")
            .with_timezone(&Utc)
    }

    #[test]
    fn the_example_from_the_rfc_parses() {
        // RFC 3164 §5.4, example 1.
        let m = parse_any(
            "<34>Oct 11 22:14:15 mymachine su: 'su root' failed for lonvick on /dev/pts/8",
        );
        assert_eq!(m.parse_error, None);
        assert_eq!(m.facility, 4);
        assert_eq!(m.severity, Severity::Critical);
        assert_eq!(m.hostname.as_deref(), Some("mymachine"));
        assert_eq!(m.app_name.as_deref(), Some("su"));
        assert_eq!(m.message, "'su root' failed for lonvick on /dev/pts/8");
    }

    #[test]
    fn a_pid_is_part_of_the_tag_and_not_of_the_message() {
        // `su[1234]:` is one field. Splitting the header on spaces would put `[1234]:` at
        // the front of every message from every daemon that reports its pid — which is
        // most of them.
        let m = parse_any("<13>Oct 11 22:14:15 host sshd[1234]: Accepted password");
        assert_eq!(m.app_name.as_deref(), Some("sshd"));
        assert_eq!(m.proc_id.as_deref(), Some("1234"));
        assert_eq!(m.message, "Accepted password");
    }

    #[test]
    fn a_space_padded_day_and_a_zero_padded_day_both_parse() {
        // The document says space-padded. Plenty of senders zero-pad anyway, and a
        // receiver that read only one of them would lose the first nine days of every
        // month from those devices.
        let padded = parse_at(
            34,
            "Oct  1 22:14:15 host app: x",
            at("2026-10-02T00:00:00Z"),
        );
        let zeroed = parse_at(
            34,
            "Oct 01 22:14:15 host app: x",
            at("2026-10-02T00:00:00Z"),
        );
        assert_eq!(padded.timestamp, zeroed.timestamp);
        assert!(padded.timestamp.is_some());
    }

    #[test]
    fn the_year_rolls_back_rather_than_forward() {
        // A message sent at 23:59 on 31 December, read at 00:01 on 1 January. Dated with
        // the current year it would land eleven months in the future and sort to the end
        // of every search.
        let m = parse_at(
            34,
            "Dec 31 23:59:00 host app: happy new year",
            at("2027-01-01T00:01:00Z"),
        );
        assert_eq!(
            m.timestamp.map(|t| t.to_rfc3339()),
            Some("2026-12-31T23:59:00+00:00".to_owned())
        );
    }

    #[test]
    fn a_clock_a_few_minutes_fast_is_not_re_dated_to_last_year() {
        // The other half of the rule. Device clocks are routinely a little ahead, and a
        // receiver that moved those back a year would be far worse than one that accepted
        // them.
        let m = parse_at(
            34,
            "Sep 16 12:05:00 host app: x",
            at("2026-09-16T12:00:00Z"),
        );
        assert_eq!(
            m.timestamp.map(|t| t.to_rfc3339()),
            Some("2026-09-16T12:05:00+00:00".to_owned())
        );
    }

    #[test]
    fn a_message_with_no_tag_keeps_all_of_its_text() {
        // Plenty of equipment sends none. Inventing one would put a word that is not an
        // application name into an inventory of application names.
        let m = parse_any("<34>Oct 11 22:14:15 host this is just a sentence");
        assert_eq!(m.hostname.as_deref(), Some("host"));
        assert_eq!(m.app_name, None);
        assert_eq!(m.message, "this is just a sentence");
    }

    #[test]
    fn a_vendor_timestamp_nobody_standardised_keeps_the_whole_line() {
        // Cisco prefixes a sequence number and its own date format. The header is not
        // read and the text is intact, which is the rule.
        let raw =
            "<189>45: *Sep 16 12:00:00.123: %LINK-3-UPDOWN: Interface Gi0/1, changed state to down";
        let m = parse_any(raw);
        assert!(m.parse_error.is_some());
        assert!(
            m.message.contains("%LINK-3-UPDOWN"),
            "the text must survive: {m:?}"
        );
        // The priority was still readable, so the facility and severity are still right —
        // which is what an alert rule filters on. 189 is local7 (23 * 8 = 184) at
        // severity 5, which is notice: Cisco encodes its own severity in the `%...-3-...`
        // mnemonic rather than in the PRI, and the two disagree. Worth knowing, and not
        // something this crate should try to reconcile.
        assert_eq!(m.facility, 23);
        assert_eq!(m.severity, Severity::Notice);
    }

    #[test]
    fn a_month_that_is_not_a_month_is_not_a_timestamp() {
        for stamp in ["Foo 11 22:14:15", "Oct 11 22-14-15", "Oct 1 22:14:15 "] {
            assert_eq!(
                timestamp_of(stamp, at("2026-10-12T00:00:00Z")),
                None,
                "{stamp:?}"
            );
        }
    }

    #[test]
    fn a_line_too_short_to_hold_a_timestamp_is_still_kept() {
        let m = parse_any("<34>short");
        assert!(m.parse_error.is_some());
        assert_eq!(m.message, "short");
    }
}
