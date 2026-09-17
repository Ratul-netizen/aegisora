//! The email transport.
//!
//! A plain SMTP conversation with a relay on the customer's own network. Four decisions,
//! and the first one is the one that shapes the rest:
//!
//! # No TLS, and therefore no AUTH
//!
//! This workspace carries no TLS by decision — see the root `Cargo.toml`: the rustls and
//! native-tls trees carry licences outside the `cargo-deny` allow-list. For a webhook that
//! means "point it at your egress proxy". For SMTP it means something stronger, and worth
//! stating rather than working around:
//!
//! **Authenticated submission is not supported, deliberately.** `AUTH PLAIN` over an
//! unencrypted connection sends the password base64-encoded, which is to say in clear. A
//! build that offered it would be inviting somebody to put their Microsoft 365 password in
//! a config field and send it across a network in the clear, and no amount of documentation
//! makes that safe. So this speaks to a **smarthost**: a relay that accepts mail from the
//! hosts on its own network, which is what an on-premise mail setup already has and what
//! `ssmtp`, `nullmailer` and every appliance in this category assume.
//!
//! A deployment that must reach an authenticated provider puts a submission proxy in
//! front — the same answer syslog-over-TLS and https webhooks got, for the same reason.
//!
//! # The other three
//!
//! **Header injection is refused, not escaped.** A newline in an address or a subject is
//! either an attack or a typo; either way the message it would produce is not the one
//! anybody meant. Addresses are checked when the channel is written, and the subject is
//! checked again when the message is built, because a rule's name reaches it and a rule
//! can be renamed after the channel was made.
//!
//! **Dot-stuffing is not optional.** A body line consisting of a single `.` ends the
//! message. `system.cpu` never produces one, but a log line pasted into a rule description
//! can, and the failure is a truncated email with the rest of it interpreted as SMTP
//! commands.
//!
//! **A non-ASCII subject is encoded.** Device names are not all ASCII, and a raw UTF-8
//! subject is a specification violation that different relays handle differently — some
//! pass it, some mangle it, one rejects the message. RFC 2047 is four lines of base64.

use std::time::Duration;

use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};
use tokio::net::TcpStream;

use crate::notification::Notification;

/// How long the whole conversation has.
///
/// A relay that has not finished in ten seconds is one an evaluation slot should not be
/// waiting for — there are sixteen of those, and a hung relay must cost one notification
/// rather than the cycle.
pub const TIMEOUT: Duration = Duration::from_secs(10);

/// The default submission port for a smarthost that is not doing TLS.
const DEFAULT_PORT: u16 = 25;

/// A configured mail channel.
#[derive(Clone, Debug)]
pub struct Smtp {
    host: String,
    port: u16,
    from: String,
    to: Vec<String>,
}

impl Smtp {
    /// Read a channel's `config`.
    ///
    /// `{"host": "smtp.internal", "port": 25, "from": "veyronis@example.com",
    ///   "to": ["ops@example.com"]}`
    ///
    /// # Errors
    ///
    /// When the host, sender or recipients are missing or unusable. All of it is checked
    /// when the channel is written, so the sentence arrives while somebody is still
    /// looking at the form rather than at 4am.
    pub fn from_config(config: &serde_json::Value) -> Result<Self, String> {
        let host = config
            .get("host")
            .and_then(serde_json::Value::as_str)
            .ok_or("an email channel needs a host — the relay that accepts mail from this network")?
            .trim()
            .to_owned();
        if host.is_empty() || host.contains(char::is_whitespace) {
            return Err(format!("{host} is not a host name"));
        }

        let port = match config.get("port") {
            None => DEFAULT_PORT,
            Some(value) => value
                .as_u64()
                .and_then(|n| u16::try_from(n).ok())
                .filter(|n| *n > 0)
                .ok_or("port must be a number between 1 and 65535")?,
        };

        let from = address(config.get("from").and_then(serde_json::Value::as_str))
            .map_err(|e| format!("from: {e}"))?;

        let to: Vec<String> = config
            .get("to")
            .and_then(serde_json::Value::as_array)
            .ok_or("an email channel needs a list of recipients")?
            .iter()
            .map(|value| address(value.as_str()).map_err(|e| format!("to: {e}")))
            .collect::<Result<_, _>>()?;

        if to.is_empty() {
            return Err("an email channel needs at least one recipient".to_owned());
        }

        Ok(Self {
            host,
            port,
            from,
            to,
        })
    }

    /// Send one notification.
    ///
    /// # Errors
    ///
    /// A sentence for the record, carrying the relay's own reply — an operator debugging
    /// mail needs "550 5.7.1 relay denied", not "delivery failed".
    pub async fn deliver(&self, notification: &Notification) -> Result<(), String> {
        let message = self.message(notification)?;

        tokio::time::timeout(TIMEOUT, self.converse(&message))
            .await
            .map_err(|_| {
                format!(
                    "{}:{} did not finish the conversation within {TIMEOUT:?}",
                    self.host, self.port
                )
            })?
    }

    /// The SMTP conversation itself.
    async fn converse(&self, message: &str) -> Result<(), String> {
        let stream = TcpStream::connect((self.host.as_str(), self.port))
            .await
            .map_err(|e| format!("{}:{} could not be reached: {e}", self.host, self.port))?;
        let (read, mut write) = stream.into_split();
        let mut reader = BufReader::new(read);

        expect(&mut reader, 220, "greeting").await?;

        // The name this server calls itself. A smarthost that cares about it is one that
        // is checking its own allow-list, and it is checking the address rather than this.
        say(&mut write, "EHLO veyronis").await?;
        expect(&mut reader, 250, "EHLO").await?;

        say(&mut write, &format!("MAIL FROM:<{}>", self.from)).await?;
        expect(&mut reader, 250, "MAIL FROM").await?;

        for recipient in &self.to {
            say(&mut write, &format!("RCPT TO:<{recipient}>")).await?;
            // One bad recipient fails the message rather than being skipped: a mail that
            // reached three of four people, silently, is worse than one that did not
            // arrive and said so.
            expect(&mut reader, 250, &format!("RCPT TO {recipient}")).await?;
        }

        say(&mut write, "DATA").await?;
        expect(&mut reader, 354, "DATA").await?;

        write
            .write_all(message.as_bytes())
            .await
            .map_err(|e| format!("the message could not be sent: {e}"))?;
        say(&mut write, ".").await?;
        expect(&mut reader, 250, "the message").await?;

        // Politeness, and a relay that logs an unclosed connection as an error is one
        // whose logs somebody reads.
        say(&mut write, "QUIT").await.ok();

        Ok(())
    }

    /// The RFC 5322 message, ready for `DATA`.
    ///
    /// Built with `push_str(&format!(..))` rather than `write!`: a header line ends in
    /// CRLF, and `writeln!` — which is what the lint suggests instead — emits a bare LF.
    /// A bare LF in an SMTP header is a specification violation that some relays silently
    /// repair and others reject, and what the lint would save is a few hundred bytes on a
    /// message nobody sends twice a second.
    #[allow(clippy::format_push_string)]
    fn message(&self, notification: &Notification) -> Result<String, String> {
        // Checked again here, not only at configuration time: a rule's name reaches the
        // subject and a rule can be renamed after the channel was made.
        //
        // The em dash is folded first. It is right everywhere else a summary is shown,
        // and in a subject it is the one character that would send *every* message
        // through RFC 2047 — so a mail log that could have been readable becomes a
        // column of base64. Folded, only a genuinely non-ASCII device or rule name is
        // encoded, which is the case the encoding exists for.
        let subject = one_line(&notification.summary().replace('\u{2014}', "-"))?;

        let mut out = String::new();
        out.push_str(&format!("From: {}\r\n", self.from));
        out.push_str(&format!("To: {}\r\n", self.to.join(", ")));
        out.push_str(&format!("Subject: {}\r\n", encode_subject(&subject)));
        out.push_str(&format!(
            "Date: {}\r\n",
            notification.at.format("%a, %d %b %Y %H:%M:%S +0000")
        ));
        // Stable per alert episode and phase, so a relay that deduplicates does not
        // collapse a firing and its resolution into one thread entry.
        out.push_str(&format!(
            "Message-ID: <{}.{}@veyronis>\r\n",
            notification.at.timestamp_millis(),
            notification.rule_id.simple()
        ));
        out.push_str("MIME-Version: 1.0\r\n");
        out.push_str("Content-Type: text/plain; charset=utf-8\r\n");
        out.push_str("Content-Transfer-Encoding: 8bit\r\n");
        out.push_str("\r\n");

        for line in notification.text().split('\n') {
            let line = line.trim_end_matches('\r');
            // Dot-stuffing: a line that is just `.` would end the message, and the rest
            // of it would be read as SMTP commands.
            if line.starts_with('.') {
                out.push('.');
            }
            out.push_str(line);
            out.push_str("\r\n");
        }

        Ok(out)
    }
}

/// One line of the conversation.
async fn say(write: &mut tokio::net::tcp::OwnedWriteHalf, line: &str) -> Result<(), String> {
    write
        .write_all(format!("{line}\r\n").as_bytes())
        .await
        .map_err(|e| format!("could not send {line}: {e}"))
}

/// Read a reply and check its code.
///
/// Multi-line replies — `250-STARTTLS`, `250 SIZE` — are read to the end, because leaving
/// the rest in the buffer desynchronises every later step and produces an error about the
/// wrong command.
async fn expect(
    reader: &mut BufReader<tokio::net::tcp::OwnedReadHalf>,
    code: u16,
    what: &str,
) -> Result<(), String> {
    loop {
        let mut line = String::new();
        let read = reader
            .read_line(&mut line)
            .await
            .map_err(|e| format!("no reply to {what}: {e}"))?;
        if read == 0 {
            return Err(format!("the relay closed the connection during {what}"));
        }

        let line = line.trim_end();
        let (number, rest) = line.split_at(line.len().min(3));
        let number: u16 = number
            .parse()
            .map_err(|_| format!("the relay answered {what} with {line}"))?;

        if number != code {
            return Err(format!("the relay answered {what} with {line}"));
        }
        // `250-` continues, `250 ` ends.
        if !rest.starts_with('-') {
            return Ok(());
        }
    }
}

/// An address that cannot carry a second header.
fn address(value: Option<&str>) -> Result<String, String> {
    let value = value.ok_or("missing")?.trim();
    if value.is_empty() {
        return Err("empty".to_owned());
    }
    one_line(value)?;
    // Not a full RFC 5321 validation, deliberately: the relay is the authority on what it
    // will accept, and a regular expression here would reject addresses that work. What
    // this rules out is the shape that is never an address and always a mistake.
    if !value.contains('@') || value.contains(' ') || value.contains('<') || value.contains('>') {
        return Err(format!("{value} is not an address"));
    }
    Ok(value.to_owned())
}

/// Refuse anything that could become a second header.
fn one_line(value: &str) -> Result<String, String> {
    if value.contains('\r') || value.contains('\n') || value.contains('\0') {
        return Err("a line break in a header is either an attack or a typo".to_owned());
    }
    Ok(value.to_owned())
}

/// RFC 2047 for a subject that is not ASCII.
///
/// A raw UTF-8 subject is a specification violation that relays disagree about: some pass
/// it, some mangle it, one rejects the message. Device names are not all ASCII.
fn encode_subject(subject: &str) -> String {
    if subject.is_ascii() {
        return subject.to_owned();
    }
    format!("=?UTF-8?B?{}?=", base64(subject.as_bytes()))
}

/// Base64, for the one place this crate needs it.
///
/// Sixteen lines rather than a dependency: the alphabet is fixed, the input is a subject
/// line, and the alternative is a crate in the SBOM for a function with no edge cases
/// beyond its padding.
fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);

    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);

        out.push(ALPHABET[(n >> 18) as usize & 63] as char);
        out.push(ALPHABET[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[n as usize & 63] as char
        } else {
            '='
        });
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::DateTime;
    use uops_core::ResourceId;
    use uops_core::alert::{AlertSeverity, Phase};

    fn channel() -> Smtp {
        Smtp::from_config(&serde_json::json!({
            "host": "smtp.internal",
            "from": "veyronis@example.com",
            "to": ["ops@example.com", "oncall@example.com"]
        }))
        .expect("a valid channel")
    }

    fn notification(rule: &str) -> Notification {
        Notification {
            phase: Phase::Firing,
            severity: AlertSeverity::Critical,
            rule: rule.to_owned(),
            rule_id: uuid::Uuid::nil(),
            resource_id: ResourceId::nil(),
            resource: "rtr-01".to_owned(),
            dedup_key: "rule/rtr-01".to_owned(),
            value: Some(94.5),
            since: DateTime::from_timestamp(1_700_000_000, 0).expect("an instant"),
            at: DateTime::from_timestamp(1_700_000_600, 0).expect("an instant"),
        }
    }

    #[test]
    fn a_channel_needs_a_relay_a_sender_and_somebody_to_tell() {
        assert!(Smtp::from_config(&serde_json::json!({})).is_err());
        assert!(
            Smtp::from_config(&serde_json::json!({ "host": "smtp.internal" })).is_err(),
            "no sender"
        );
        assert!(
            Smtp::from_config(&serde_json::json!({
                "host": "smtp.internal",
                "from": "veyronis@example.com",
                "to": []
            }))
            .is_err(),
            "a channel that tells nobody is a channel that does nothing"
        );
    }

    #[test]
    fn an_address_with_a_line_break_in_it_is_refused_when_the_channel_is_written() {
        // The classic header injection: everything after the newline becomes headers of
        // its own, including a second Bcc.
        let injected = Smtp::from_config(&serde_json::json!({
            "host": "smtp.internal",
            "from": "veyronis@example.com\r\nBcc: attacker@example.com",
            "to": ["ops@example.com"]
        }));
        assert!(injected.is_err(), "{injected:?}");

        let in_recipient = Smtp::from_config(&serde_json::json!({
            "host": "smtp.internal",
            "from": "veyronis@example.com",
            "to": ["ops@example.com\nBcc: attacker@example.com"]
        }));
        assert!(in_recipient.is_err(), "{in_recipient:?}");
    }

    #[test]
    fn a_rule_renamed_to_carry_a_header_is_refused_when_the_message_is_built() {
        // The address was checked when the channel was written. A rule's name reaches the
        // subject and can be changed afterwards, so it is checked again here.
        let message = channel().message(&notification("CPU hot\r\nBcc: attacker@example.com"));
        assert!(message.is_err(), "{message:?}");
    }

    #[test]
    fn the_message_has_the_headers_a_relay_expects() {
        let message = channel()
            .message(&notification("CPU hot"))
            .expect("message");

        assert!(
            message.contains("From: veyronis@example.com\r\n"),
            "{message}"
        );
        assert!(
            message.contains("To: ops@example.com, oncall@example.com\r\n"),
            "{message}"
        );
        assert!(
            message.contains("Subject: [critical] rtr-01 - CPU hot firing (94.500)\r\n"),
            "an ASCII subject stays readable rather than becoming a column of base64: {message}"
        );
        assert!(message.contains("Date: "), "{message}");
        assert!(message.contains("Message-ID: <"), "{message}");
        assert!(
            message.contains("Content-Type: text/plain; charset=utf-8\r\n"),
            "{message}"
        );
        // Headers end with a blank line, and the body follows.
        assert!(message.contains("\r\n\r\n"), "{message}");
        assert!(message.contains("Since:"), "the body is the notification");
    }

    #[test]
    fn every_line_ends_the_way_smtp_requires() {
        let message = channel()
            .message(&notification("CPU hot"))
            .expect("message");
        for line in message.split("\r\n") {
            assert!(!line.contains('\n'), "a bare newline survived: {line:?}");
        }
    }

    #[test]
    fn a_body_line_that_would_end_the_message_is_stuffed() {
        // A line consisting of one dot ends DATA. The rest of the message would then be
        // read as SMTP commands — which is a truncated email at best.
        let mut alert = notification("CPU hot");
        alert.dedup_key = ".".to_owned();

        let message = channel().message(&alert).expect("message");
        assert!(
            message.contains("Alert:    ."),
            "the line is still there: {message}"
        );
        // And nothing in the body is a lone dot any more.
        let body = message.split("\r\n\r\n").nth(1).expect("a body");
        assert!(
            !body.split("\r\n").any(|line| line == "."),
            "a lone dot survived: {body:?}"
        );
    }

    #[test]
    fn a_subject_that_is_not_ascii_is_encoded_rather_than_sent_raw() {
        // Relays disagree about raw UTF-8 in a header: some pass it, some mangle it, one
        // rejects the message.
        let message = channel()
            .message(&notification("CPU wärmer als erwartet"))
            .expect("message");

        let subject = message
            .split("\r\n")
            .find(|line| line.starts_with("Subject: "))
            .expect("a subject");
        assert!(subject.starts_with("Subject: =?UTF-8?B?"), "{subject}");
        assert!(subject.ends_with("?="), "{subject}");
        assert!(subject.is_ascii(), "the header itself must be ASCII");
    }

    #[test]
    fn base64_agrees_with_the_examples_in_the_rfc() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foob"), "Zm9vYg==");
        assert_eq!(base64(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn the_port_defaults_and_is_bounded() {
        assert_eq!(channel().port, DEFAULT_PORT);
        assert!(
            Smtp::from_config(&serde_json::json!({
                "host": "smtp.internal",
                "port": 0,
                "from": "veyronis@example.com",
                "to": ["ops@example.com"]
            }))
            .is_err()
        );
    }
}
