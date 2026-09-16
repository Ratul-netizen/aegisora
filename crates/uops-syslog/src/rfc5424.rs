//! RFC 5424 — the one that was actually standardised.
//!
//! ```text
//! <34>1 2003-10-11T22:14:15.003Z mymachine.example.com su - ID47 [exampleSDID@32473 iut="3"] BOM'su root' failed
//!  |   | |                       |                     |  |  |    |                           |
//!  PRI | TIMESTAMP               HOSTNAME              |  |  MSGID STRUCTURED-DATA             MSG
//!      VERSION                                         |  PROCID
//!                                                      APP-NAME
//! ```
//!
//! Seven header fields separated by single spaces, then structured data, then whatever is
//! left. Every field except the priority, the version and the structured data may be
//! `-`, the NILVALUE, meaning the sender declined to say.
//!
//! # Why this is forgiving about field count
//!
//! The header is fixed-width in *fields*, not characters, so a message missing one has
//! every subsequent field shifted — a hostname read as an app name, a message read as a
//! message id. That is worse than not reading them: it produces an inventory of
//! applications that do not exist.
//!
//! So a short header stops where it runs out, keeps what it read, and flags the rest.
//! What is never done is guessing which field was omitted.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};

use crate::{Message, decode};

/// Parse everything after `<PRI>VERSION `.
pub(crate) fn parse(priority: u8, rest: &str) -> Message {
    let (facility, severity) = decode(priority);
    let mut message = Message {
        facility,
        severity,
        timestamp: None,
        hostname: None,
        app_name: None,
        proc_id: None,
        msg_id: None,
        structured_data: BTreeMap::new(),
        message: String::new(),
        parse_error: None,
    };

    let mut fields = rest.splitn(6, ' ');

    let Some(timestamp) = fields.next() else {
        message.parse_error = Some("rfc5424: header ended before the timestamp");
        return message;
    };
    if let Some(t) = nilable(timestamp) {
        match timestamp_of(t) {
            Some(parsed) => message.timestamp = Some(parsed),
            // Kept as a parse error rather than silently dropped: a device with an
            // unreadable clock is worth knowing about, and the message is still the
            // message.
            None => message.parse_error = Some("rfc5424: unreadable timestamp"),
        }
    }

    message.hostname = fields.next().and_then(nilable).map(ToOwned::to_owned);
    message.app_name = fields.next().and_then(nilable).map(ToOwned::to_owned);
    message.proc_id = fields.next().and_then(nilable).map(ToOwned::to_owned);
    message.msg_id = fields.next().and_then(nilable).map(ToOwned::to_owned);

    let Some(tail) = fields.next() else {
        // A header that ended early. Everything read so far is kept; nothing is shifted
        // into the wrong field.
        if message.parse_error.is_none() {
            message.parse_error = Some("rfc5424: header ended early");
        }
        return message;
    };

    let (structured, body) = structured_data(tail);
    match structured {
        Ok(data) => message.structured_data = data,
        Err(why) => {
            if message.parse_error.is_none() {
                message.parse_error = Some(why);
            }
        }
    }
    strip_bom(body).clone_into(&mut message.message);

    message
}

/// `-` is NILVALUE, and an empty field is not a value either.
fn nilable(field: &str) -> Option<&str> {
    match field {
        "-" | "" => None,
        other => Some(other),
    }
}

/// RFC 5424's TIMESTAMP, which is RFC 3339 with two restrictions.
///
/// Offsets are permitted and are preserved as an instant; `-00:00` means "unknown local
/// offset" per RFC 3339 §4.3 and is treated as UTC, which is what it is.
fn timestamp_of(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|t| t.with_timezone(&Utc))
}

/// Split the structured data from the message, and flatten it.
///
/// Returns the parameters keyed `sdid.param`, and whatever followed. A `-` means no
/// structured data, which is the common case.
fn structured_data(tail: &str) -> (Result<BTreeMap<String, String>, &'static str>, &str) {
    let mut out = BTreeMap::new();

    if let Some(rest) = tail.strip_prefix('-') {
        // NILVALUE. The space after it belongs to the separator, not to the message.
        return (Ok(out), rest.strip_prefix(' ').unwrap_or(rest));
    }
    if !tail.starts_with('[') {
        // No structured data and no NILVALUE either. Some senders simply omit the field;
        // the rest of the line is the message.
        return (Ok(out), tail);
    }

    let bytes = tail.as_bytes();
    let mut i = 0;
    while i < bytes.len() && bytes[i] == b'[' {
        let Some(end) = element_end(bytes, i) else {
            return (Err("rfc5424: unterminated structured data"), &tail[i..]);
        };
        parse_element(&tail[i + 1..end], &mut out);
        i = end + 1;
    }

    let rest = &tail[i..];
    (Ok(out), rest.strip_prefix(' ').unwrap_or(rest))
}

/// The index of the `]` closing the element that starts at `from`.
///
/// Escaping is the part that matters: inside a PARAM-VALUE, `\]` is a literal bracket and
/// must not end the element. A scanner that searched for the next `]` would truncate any
/// message whose structured data contained one — and a firewall rule or a file path in a
/// log parameter contains brackets all the time.
fn element_end(bytes: &[u8], from: usize) -> Option<usize> {
    let mut i = from + 1;
    let mut in_quotes = false;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => {
                // Skips whatever follows, quoted or not: `\\` is a literal backslash and
                // must not make the next byte look escaped.
                i += 2;
                continue;
            }
            b'"' => in_quotes = !in_quotes,
            b']' if !in_quotes => return Some(i),
            _ => {}
        }
        i += 1;
    }
    None
}

/// One `sdid param="value" ...` element, flattened into `out`.
fn parse_element(element: &str, out: &mut BTreeMap<String, String>) {
    let mut chars = element.char_indices().peekable();

    // SD-ID runs to the first space.
    let id_end = element.find(' ').unwrap_or(element.len());
    let id = &element[..id_end];
    if id.is_empty() {
        return;
    }
    while let Some(&(i, _)) = chars.peek() {
        if i >= id_end {
            break;
        }
        chars.next();
    }

    let mut rest = &element[id_end..];
    while let Some(start) = rest.find(|c: char| !c.is_whitespace()) {
        rest = &rest[start..];
        let Some(eq) = rest.find('=') else { break };
        let name = &rest[..eq];
        let after = &rest[eq + 1..];
        let Some(value_rest) = after.strip_prefix('"') else {
            // A parameter without quotes. Not legal, and not worth abandoning the rest of
            // the element over.
            break;
        };

        let (value, consumed) = unescape(value_rest);
        if !name.is_empty() {
            out.insert(format!("{id}.{name}"), value);
        }
        rest = &value_rest[consumed..];
        rest = rest.strip_prefix('"').unwrap_or(rest);
    }
}

/// Read a quoted PARAM-VALUE, returning it and how many bytes it occupied.
///
/// RFC 5424 §6.3.3 escapes exactly three characters — `"`, `\` and `]` — and says a
/// backslash before anything else is a literal backslash. Following that rather than
/// treating every `\x` as an escape matters: Windows paths in log parameters are full of
/// backslashes that were never meant as escapes.
fn unescape(s: &str) -> (String, usize) {
    let mut out = String::new();
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'"' => return (out, i),
            b'\\' if i + 1 < bytes.len() => {
                let next = bytes[i + 1];
                if matches!(next, b'"' | b'\\' | b']') {
                    out.push(next as char);
                    i += 2;
                } else {
                    out.push('\\');
                    i += 1;
                }
            }
            _ => {
                // Copy one whole character, not one byte: a multi-byte UTF-8 sequence
                // pushed a byte at a time would produce replacement characters.
                let ch = s[i..].chars().next().unwrap_or('\u{fffd}');
                out.push(ch);
                i += ch.len_utf8();
            }
        }
    }
    (out, i)
}

/// RFC 5424 §6.4: a MSG that begins with a UTF-8 BOM is UTF-8.
///
/// The BOM is a marker rather than content, and leaving it on puts an invisible character
/// at the front of every message from a compliant sender — which then does not match a
/// search for the first word.
fn strip_bom(s: &str) -> &str {
    s.strip_prefix('\u{feff}').unwrap_or(s)
}

#[cfg(test)]
mod tests {
    use crate::{Severity, parse as parse_any};

    #[test]
    fn the_examples_from_the_rfc_parse() {
        // RFC 5424 §6.5, example 1.
        let m = parse_any(
            "<34>1 2003-10-11T22:14:15.003Z mymachine.example.com su - ID47 - BOM'su root' failed for lonvick on /dev/pts/8",
        );
        assert_eq!(m.parse_error, None);
        assert_eq!(m.facility, 4);
        assert_eq!(m.severity, Severity::Critical);
        assert_eq!(m.hostname.as_deref(), Some("mymachine.example.com"));
        assert_eq!(m.app_name.as_deref(), Some("su"));
        assert_eq!(m.proc_id, None, "`-` is NILVALUE");
        assert_eq!(m.msg_id.as_deref(), Some("ID47"));
        assert!(m.message.starts_with("BOM'su root' failed"));
        assert_eq!(
            m.timestamp.map(|t| t.to_rfc3339()),
            Some("2003-10-11T22:14:15.003+00:00".to_owned())
        );
    }

    #[test]
    fn structured_data_is_flattened_onto_its_id() {
        // RFC 5424 §6.5, example 3, with two elements.
        let m = parse_any(
            r#"<165>1 2003-10-11T22:14:15.003Z mymachine.example.com evntslog - ID47 [exampleSDID@32473 iut="3" eventSource="Application" eventID="1011"][examplePriority@32473 class="high"] msg"#,
        );
        assert_eq!(m.parse_error, None);
        assert_eq!(
            m.structured_data
                .get("exampleSDID@32473.iut")
                .map(String::as_str),
            Some("3")
        );
        assert_eq!(
            m.structured_data
                .get("exampleSDID@32473.eventSource")
                .map(String::as_str),
            Some("Application")
        );
        assert_eq!(
            m.structured_data
                .get("examplePriority@32473.class")
                .map(String::as_str),
            Some("high")
        );
        assert_eq!(m.message, "msg");
    }

    #[test]
    fn a_bracket_inside_a_value_does_not_end_the_element() {
        // The escaping rule, and the reason it matters: a firewall rule or a file path in
        // a log parameter contains brackets, and a scanner looking for the next `]` would
        // truncate the message there.
        let m = parse_any(
            r#"<34>1 2003-10-11T22:14:15Z h a - - [rule@1 match="deny ip any any \] log" note="a\\b"] the body"#,
        );
        assert_eq!(m.parse_error, None);
        assert_eq!(
            m.structured_data.get("rule@1.match").map(String::as_str),
            Some("deny ip any any ] log")
        );
        assert_eq!(
            m.structured_data.get("rule@1.note").map(String::as_str),
            Some(r"a\b")
        );
        assert_eq!(m.message, "the body");
    }

    #[test]
    fn a_backslash_before_anything_else_is_a_backslash() {
        // RFC 5424 §6.3.3 escapes exactly three characters. Treating every `\x` as an
        // escape would eat the separators out of every Windows path ever logged.
        let m = parse_any(r#"<34>1 2003-10-11T22:14:15Z h a - - [f@1 path="C:\Users\new\test"] x"#);
        assert_eq!(
            m.structured_data.get("f@1.path").map(String::as_str),
            Some(r"C:\Users\new\test")
        );
    }

    #[test]
    fn the_bom_is_a_marker_and_not_content() {
        // Left on, it puts an invisible character at the front of every message from a
        // compliant sender, which then does not match a search for the first word.
        let m = parse_any("<34>1 2003-10-11T22:14:15Z h a - - - \u{feff}hello");
        assert_eq!(m.message, "hello");
    }

    #[test]
    fn a_short_header_keeps_what_it_read_and_shifts_nothing() {
        // The failure this is written against: a missing field shifts every later one, so
        // a hostname becomes an app name and an inventory fills with applications that do
        // not exist. Stopping is right; guessing which field was omitted is not.
        let m = parse_any("<34>1 2003-10-11T22:14:15Z myhost");
        assert_eq!(m.hostname.as_deref(), Some("myhost"));
        assert_eq!(m.app_name, None);
        assert!(m.parse_error.is_some(), "{m:?}");
        assert!(m.timestamp.is_some(), "what was read is kept");
    }

    #[test]
    fn an_unreadable_timestamp_is_flagged_and_the_message_survives() {
        let m = parse_any("<34>1 not-a-date host app - - - the body");
        assert_eq!(m.timestamp, None);
        assert_eq!(m.parse_error, Some("rfc5424: unreadable timestamp"));
        assert_eq!(m.message, "the body");
        assert_eq!(m.hostname.as_deref(), Some("host"));
    }

    #[test]
    fn unterminated_structured_data_does_not_lose_the_line() {
        let m = parse_any(r#"<34>1 2003-10-11T22:14:15Z h a - - [broken@1 k="v" still going"#);
        assert!(m.parse_error.is_some());
        assert!(!m.message.is_empty(), "the text must survive: {m:?}");
    }

    #[test]
    fn an_offset_timestamp_becomes_the_same_instant() {
        // A device in Dhaka reporting +06:00 and one in UTC reporting the same instant
        // must sort together. This is the whole reason everything below the UI is UTC.
        let dhaka = parse_any("<34>1 2026-09-16T12:00:00+06:00 h a - - - x");
        let utc = parse_any("<34>1 2026-09-16T06:00:00Z h a - - - x");
        assert_eq!(dhaka.timestamp, utc.timestamp);
    }
}
