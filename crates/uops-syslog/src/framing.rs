//! Finding the message boundaries in a TCP stream.
//!
//! UDP needs none of this — one datagram is one message, and that is the whole appeal of
//! it. TCP is a byte stream, and RFC 6587 describes two incompatible ways of dividing it:
//!
//! * **Octet counting** (§3.4.1): `123 <34>1 2003-...` — a decimal length, a space, then
//!   exactly that many bytes. Unambiguous, and the one to prefer.
//! * **Non-transparent framing** (§3.4.2): messages separated by `\n`. Ambiguous by
//!   construction, because a message containing a newline is indistinguishable from two
//!   messages — which RFC 6587 acknowledges and then recommends anyway, because it is
//!   what everything already did.
//!
//! SPEC §M3: *"Detect per-connection on the first byte: a digit means octet-counting."*
//! That is what [`Framer`] does, and it decides once per connection rather than per
//! message: a sender that switched mid-stream would be a sender whose framing cannot be
//! determined at all, and guessing again on every message would turn one malformed
//! message into a desynchronised connection.
//!
//! # Why a byte limit is not optional
//!
//! A stream that never contains a newline, or a length prefix claiming four gigabytes, is
//! a stream that fills memory. Both are things a hostile or broken sender does, and a
//! receiver with no ceiling is a receiver that can be stopped by one connection. The
//! limit is applied to both framings and the connection is failed rather than truncated —
//! a truncated message looks like a real one.

/// The largest message this will assemble, in bytes.
///
/// RFC 5425 §4.3.1 requires TLS syslog receivers to accept at least 2 048 octets and
/// recommends supporting 8 192. Real equipment sends more: a Windows event forwarded over
/// syslog routinely runs to tens of kilobytes. 64 KiB matches the UDP datagram ceiling,
/// which makes the two transports agree about what is too big — and a message that is too
/// big for one and not the other would be a difference nobody could explain.
pub const MAX_MESSAGE: usize = 64 * 1024;

/// Which framing a connection is using.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Framing {
    /// RFC 6587 §3.4.1. A decimal byte count, a space, then the message.
    OctetCounted,
    /// RFC 6587 §3.4.2. Newline-separated.
    LineDelimited,
}

/// Why a stream could not be framed.
///
/// Both variants mean the connection is unusable rather than this message being bad:
/// once a length prefix is wrong, there is no way to find where the next message starts.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum FramingError {
    #[error("a message longer than {MAX_MESSAGE} bytes; the connection cannot be resynchronised")]
    TooLong,
    #[error("a length prefix that is not a number; the connection cannot be resynchronised")]
    BadLength,
}

/// Divides a byte stream into messages.
///
/// Fed whatever arrives from the socket, in whatever sizes it arrives in. Holds a
/// partial message between calls, which is the entire point: TCP gives no guarantee that
/// a read ends on a message boundary, and a receiver that assumed one would lose a
/// message every time a packet split in the wrong place — rarely, under load, and
/// invisibly.
#[derive(Debug)]
pub struct Framer {
    buffer: Vec<u8>,
    framing: Option<Framing>,
}

impl Default for Framer {
    fn default() -> Self {
        Self::new()
    }
}

impl Framer {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            buffer: Vec::new(),
            framing: None,
        }
    }

    /// Which framing was detected, once anything has arrived.
    #[must_use]
    pub const fn framing(&self) -> Option<Framing> {
        self.framing
    }

    /// How many bytes are held pending more input.
    #[must_use]
    pub fn pending(&self) -> usize {
        self.buffer.len()
    }

    /// Add bytes from the socket.
    ///
    /// # Errors
    ///
    /// When the buffered partial message exceeds [`MAX_MESSAGE`] — see the module docs on
    /// why that fails the connection rather than truncating.
    pub fn push(&mut self, bytes: &[u8]) -> Result<(), FramingError> {
        if self.buffer.len() + bytes.len() > MAX_MESSAGE.saturating_mul(2) {
            // Twice the limit, because a buffer holding one maximum-length message plus
            // the start of the next is legitimate. Beyond that, nothing that could be
            // assembled is under the ceiling.
            return Err(FramingError::TooLong);
        }
        self.buffer.extend_from_slice(bytes);

        if self.framing.is_none()
            && let Some(first) = self.buffer.first()
        {
            // SPEC: a digit means octet counting. RFC 5424 and RFC 3164 both begin with
            // `<`, so there is no message that could be mistaken for a length.
            self.framing = Some(if first.is_ascii_digit() {
                Framing::OctetCounted
            } else {
                Framing::LineDelimited
            });
        }
        Ok(())
    }

    /// The next complete message, if there is one.
    ///
    /// Returns `Ok(None)` when more bytes are needed. Call until it does.
    ///
    /// # Errors
    ///
    /// A length prefix that is not a number, or a message over [`MAX_MESSAGE`].
    pub fn next_message(&mut self) -> Result<Option<String>, FramingError> {
        match self.framing {
            None => Ok(None),
            Some(Framing::OctetCounted) => self.next_counted(),
            Some(Framing::LineDelimited) => self.next_line(),
        }
    }

    fn next_counted(&mut self) -> Result<Option<String>, FramingError> {
        let Some(space) = self.buffer.iter().position(|b| *b == b' ') else {
            // No separator yet. A prefix longer than twenty digits is not a length that
            // any message could have, and waiting for a space that will never come is how
            // a connection stalls for ever.
            if self.buffer.len() > 20 {
                return Err(FramingError::BadLength);
            }
            return Ok(None);
        };

        let digits = &self.buffer[..space];
        if digits.is_empty() || !digits.iter().all(u8::is_ascii_digit) {
            return Err(FramingError::BadLength);
        }
        let length: usize = std::str::from_utf8(digits)
            .map_err(|_| FramingError::BadLength)?
            .parse()
            .map_err(|_| FramingError::BadLength)?;

        if length > MAX_MESSAGE {
            return Err(FramingError::TooLong);
        }

        let start = space + 1;
        if self.buffer.len() < start + length {
            return Ok(None);
        }

        let message = decode(&self.buffer[start..start + length]);
        self.buffer.drain(..start + length);
        Ok(Some(message))
    }

    fn next_line(&mut self) -> Result<Option<String>, FramingError> {
        let Some(newline) = self.buffer.iter().position(|b| *b == b'\n') else {
            if self.buffer.len() > MAX_MESSAGE {
                return Err(FramingError::TooLong);
            }
            return Ok(None);
        };

        let line = decode(&self.buffer[..newline]);
        self.buffer.drain(..=newline);

        // An empty line is a keepalive or a doubled separator, not a message. Passing it
        // on would fill the log with rows whose body is the empty string.
        if line.trim().is_empty() {
            return self.next_line();
        }
        Ok(Some(line))
    }

    /// Whatever is left when the connection closes.
    ///
    /// A line-framed sender that closes without a trailing newline has still sent a
    /// message, and it is usually the last thing the device said before it stopped — the
    /// most interesting message in the stream. An octet-counted stream ending mid-message
    /// is a truncation and returns nothing, because a partial message that looks complete
    /// is worse than a lost one.
    #[must_use]
    pub fn finish(&mut self) -> Option<String> {
        if self.framing != Some(Framing::LineDelimited) {
            return None;
        }
        let rest = decode(&self.buffer);
        self.buffer.clear();
        if rest.trim().is_empty() {
            None
        } else {
            Some(rest)
        }
    }
}

/// Bytes as text, never failing.
///
/// `from_utf8_lossy`, because a message with one bad byte is still the message and this
/// crate does not drop things. A device sending latin-1 is a real device; the replacement
/// characters are visible in the body, which is a better outcome than the row not
/// existing.
fn decode(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes)
        .trim_end_matches(['\r', '\n'])
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn collect(framer: &mut Framer, bytes: &[u8]) -> Vec<String> {
        framer.push(bytes).expect("push");
        let mut out = Vec::new();
        while let Some(m) = framer.next_message().expect("frame") {
            out.push(m);
        }
        out
    }

    #[test]
    fn a_digit_chooses_octet_counting_and_an_angle_bracket_does_not() {
        let mut counted = Framer::new();
        counted.push(b"11 <34>hello").expect("push");
        assert_eq!(counted.framing(), Some(Framing::OctetCounted));

        let mut lines = Framer::new();
        lines.push(b"<34>hello\n").expect("push");
        assert_eq!(lines.framing(), Some(Framing::LineDelimited));
    }

    #[test]
    fn octet_counted_messages_are_read_exactly() {
        let mut f = Framer::new();
        let out = collect(&mut f, b"9 <34>first10 <34>second");
        assert_eq!(out, vec!["<34>first", "<34>second"]);
        assert_eq!(f.pending(), 0);
    }

    #[test]
    fn a_message_split_across_reads_is_reassembled() {
        // The property the whole type exists for. TCP gives no guarantee that a read ends
        // on a message boundary, and a receiver that assumed one would lose a message
        // whenever a packet split in the wrong place — rarely, under load, invisibly.
        let payload = "<34>Oct 11 22:14:15 host app: x";
        let framed = format!("{} {payload}", payload.len());
        let bytes = framed.as_bytes();

        let mut f = Framer::new();
        // Split at two awkward places: inside the timestamp, and inside the body.
        assert!(collect(&mut f, &bytes[..19]).is_empty());
        assert!(collect(&mut f, &bytes[19..27]).is_empty());
        let out = collect(&mut f, &bytes[27..]);
        assert_eq!(out, vec![payload]);
    }

    #[test]
    fn a_newline_inside_an_octet_counted_message_is_content() {
        // The reason octet counting is worth preferring. A stack trace sent over
        // line-delimited framing becomes one row per line; sent with a length prefix it
        // stays one message.
        let payload = "<34>line one\nline two\nline three";
        let framed = format!("{} {payload}", payload.len());
        let mut f = Framer::new();
        let out = collect(&mut f, framed.as_bytes());
        assert_eq!(out.len(), 1);
        assert!(out[0].contains("line two"), "{out:?}");
    }

    #[test]
    fn line_delimited_messages_split_on_newlines() {
        let mut f = Framer::new();
        let out = collect(&mut f, b"<34>one\n<34>two\n<34>three\n");
        assert_eq!(out, vec!["<34>one", "<34>two", "<34>three"]);
    }

    #[test]
    fn a_carriage_return_is_not_part_of_the_message() {
        // Windows senders and some network equipment use CRLF. Left on, every message
        // from those devices ends in an invisible character that breaks an exact-match
        // search on the last word.
        let mut f = Framer::new();
        let out = collect(&mut f, b"<34>one\r\n<34>two\r\n");
        assert_eq!(out, vec!["<34>one", "<34>two"]);
    }

    #[test]
    fn blank_lines_are_not_messages() {
        // Keepalives and doubled separators. Passed on, they fill the log with rows whose
        // body is the empty string.
        let mut f = Framer::new();
        let out = collect(&mut f, b"<34>one\n\n\n<34>two\n");
        assert_eq!(out, vec!["<34>one", "<34>two"]);
    }

    #[test]
    fn a_line_framed_stream_that_closes_without_a_newline_keeps_its_last_message() {
        // Usually the last thing a device said before it stopped, which is the most
        // interesting message in the stream.
        let mut f = Framer::new();
        assert_eq!(collect(&mut f, b"<34>one\n<34>the last thing").len(), 1);
        assert_eq!(f.finish(), Some("<34>the last thing".to_owned()));
        assert_eq!(f.finish(), None, "and only once");
    }

    #[test]
    fn a_truncated_octet_counted_message_is_not_delivered() {
        // The opposite decision, for the opposite reason: the length prefix says how long
        // the message is, so a short one is a truncation. A partial message that looked
        // complete would be worse than a lost one — nothing downstream could tell.
        let mut f = Framer::new();
        assert!(collect(&mut f, b"100 <34>only the beginning").is_empty());
        assert_eq!(f.finish(), None);
    }

    #[test]
    fn a_length_that_is_not_a_number_fails_the_connection() {
        // Not the message — the connection. Once a length prefix is wrong there is no way
        // to find where the next message starts, and continuing would emit garbage
        // indefinitely.
        let mut f = Framer::new();
        f.push(b"12x4 <34>hello").expect("push");
        assert_eq!(f.next_message(), Err(FramingError::BadLength));
    }

    #[test]
    fn a_length_prefix_that_never_ends_fails_rather_than_stalling() {
        let mut f = Framer::new();
        f.push(b"123456789012345678901234567890").expect("push");
        assert_eq!(f.next_message(), Err(FramingError::BadLength));
    }

    #[test]
    fn nothing_may_grow_without_bound() {
        // A stream with no newline and no length prefix is how one connection stops a
        // receiver. Both framings have a ceiling and both fail the connection at it.
        let mut lines = Framer::new();
        lines.push(b"<34>").expect("push");
        let big = vec![b'x'; MAX_MESSAGE + 1];
        lines.push(&big).expect("under twice the limit");
        assert_eq!(lines.next_message(), Err(FramingError::TooLong));

        let mut counted = Framer::new();
        counted
            .push(format!("{} x", MAX_MESSAGE + 1).as_bytes())
            .expect("push");
        assert_eq!(counted.next_message(), Err(FramingError::TooLong));
    }

    #[test]
    fn invalid_utf8_becomes_a_body_rather_than_a_dropped_message() {
        // A device sending latin-1 is a real device. The replacement characters are
        // visible in the body, which beats the row not existing.
        let mut f = Framer::new();
        let out = collect(&mut f, b"<34>caf\xe9\n");
        assert_eq!(out.len(), 1);
        assert!(out[0].starts_with("<34>caf"), "{out:?}");
    }

    #[test]
    fn the_framing_is_decided_once_per_connection() {
        // A sender that switched mid-stream would be one whose framing cannot be
        // determined at all, and re-deciding per message would turn one malformed message
        // into a desynchronised connection.
        let mut f = Framer::new();
        collect(&mut f, b"<34>one\n");
        assert_eq!(f.framing(), Some(Framing::LineDelimited));
        collect(&mut f, b"9 <34>two\n");
        assert_eq!(
            f.framing(),
            Some(Framing::LineDelimited),
            "the framing must not change under it"
        );
    }
}
