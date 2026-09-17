//! The SMTP transport, against a socket that speaks the protocol back.
//!
//! Eighty lines of `TcpListener` rather than a mock, for the same reason the webhook test
//! uses a real one: what is under test is that a notification leaves this process as a
//! conversation a relay would accept, and a mock of our own client proves only that our
//! own client was called.
//!
//! The fake relay is deliberately pedantic in one way — it answers `EHLO` with a
//! multi-line reply, because that is what every real relay does and reading only the
//! first line of one desynchronises everything after it.

use std::sync::Arc;

use chrono::Utc;
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};
use tokio::net::TcpListener;
use tokio::sync::Mutex;
use uops_core::ResourceId;
use uops_core::alert::{AlertSeverity, Phase};
use uops_notify::{Notification, Smtp};

/// What the relay saw.
#[derive(Clone, Debug, Default)]
struct Session {
    commands: Vec<String>,
    message: String,
}

/// How the relay should behave.
#[derive(Clone, Copy)]
enum Relay {
    /// Accepts everything.
    Willing,
    /// Refuses the recipient, the way one does when it will not relay for you.
    RefusesRecipient,
}

/// A relay on `127.0.0.1`, and what it was told.
async fn relay(behaviour: Relay) -> (String, u16, Arc<Mutex<Vec<Session>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let sessions: Arc<Mutex<Vec<Session>>> = Arc::new(Mutex::new(Vec::new()));

    let recorder = Arc::clone(&sessions);
    tokio::spawn(async move {
        while let Ok((socket, _)) = listener.accept().await {
            let recorder = Arc::clone(&recorder);
            tokio::spawn(async move {
                let (read, mut write) = socket.into_split();
                let mut reader = BufReader::new(read);
                let mut session = Session::default();
                let mut in_data = false;

                let _ = write.write_all(b"220 relay.invalid ESMTP\r\n").await;

                loop {
                    let mut line = String::new();
                    let Ok(read) = reader.read_line(&mut line).await else {
                        break;
                    };
                    if read == 0 {
                        break;
                    }
                    let line = line.trim_end_matches(['\r', '\n']).to_owned();

                    if in_data {
                        if line == "." {
                            in_data = false;
                            let _ = write.write_all(b"250 2.0.0 queued\r\n").await;
                            continue;
                        }
                        session.message.push_str(&line);
                        session.message.push('\n');
                        continue;
                    }

                    session.commands.push(line.clone());
                    let upper = line.to_uppercase();

                    let reply: &[u8] = if upper.starts_with("EHLO") {
                        // Multi-line, as every real relay answers.
                        b"250-relay.invalid\r\n250-SIZE 10240000\r\n250 8BITMIME\r\n"
                    } else if upper.starts_with("RCPT TO") {
                        match behaviour {
                            Relay::Willing => b"250 2.1.5 ok\r\n",
                            Relay::RefusesRecipient => b"550 5.7.1 relay denied\r\n",
                        }
                    } else if upper.starts_with("DATA") {
                        in_data = true;
                        b"354 end with .\r\n"
                    } else if upper.starts_with("QUIT") {
                        let _ = write.write_all(b"221 bye\r\n").await;
                        break;
                    } else {
                        b"250 2.0.0 ok\r\n"
                    };

                    let _ = write.write_all(reply).await;
                }

                recorder.lock().await.push(session);
            });
        }
    });

    ("127.0.0.1".to_owned(), port, sessions)
}

/// Wait for the relay to finish recording a session.
///
/// `deliver` returns when the relay accepts the message; the relay's own task records the
/// session a moment later, after `QUIT`. Reading the recorder immediately is a race the
/// test loses about half the time — and one that says nothing about the transport.
async fn session_of(sessions: &Arc<Mutex<Vec<Session>>>) -> Session {
    for _ in 0..200 {
        if let Some(session) = sessions.lock().await.first() {
            return session.clone();
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("the relay recorded no session");
}

fn notification(rule: &str) -> Notification {
    Notification {
        phase: Phase::Firing,
        severity: AlertSeverity::Critical,
        rule: rule.to_owned(),
        rule_id: uuid::Uuid::now_v7(),
        resource_id: ResourceId::nil(),
        resource: "rtr-01".to_owned(),
        dedup_key: "rule/rtr-01".to_owned(),
        value: Some(94.5),
        since: Utc::now(),
        at: Utc::now(),
    }
}

fn channel(host: &str, port: u16) -> Smtp {
    Smtp::from_config(&serde_json::json!({
        "host": host,
        "port": port,
        "from": "veyronis@example.com",
        "to": ["ops@example.com", "oncall@example.com"]
    }))
    .expect("a valid channel")
}

#[tokio::test]
async fn an_alert_reaches_a_relay_as_a_conversation_it_would_accept() {
    let (host, port, sessions) = relay(Relay::Willing).await;

    channel(&host, port)
        .deliver(&notification("CPU hot"))
        .await
        .expect("delivered");

    let session = session_of(&sessions).await;

    // The conversation, in the order a relay expects it — and one RCPT per recipient.
    let commands: Vec<&str> = session.commands.iter().map(String::as_str).collect();
    assert!(commands[0].starts_with("EHLO"), "{commands:?}");
    assert_eq!(
        commands[1], "MAIL FROM:<veyronis@example.com>",
        "{commands:?}"
    );
    assert_eq!(commands[2], "RCPT TO:<ops@example.com>", "{commands:?}");
    assert_eq!(commands[3], "RCPT TO:<oncall@example.com>", "{commands:?}");
    assert_eq!(commands[4], "DATA", "{commands:?}");

    // And the message itself is one a mail client can read.
    assert!(
        session.message.contains("From: veyronis@example.com"),
        "{}",
        session.message
    );
    assert!(
        session
            .message
            .contains("To: ops@example.com, oncall@example.com"),
        "{}",
        session.message
    );
    assert!(
        session.message.contains("Subject: [critical] rtr-01"),
        "{}",
        session.message
    );
    assert!(session.message.contains("rtr-01"), "{}", session.message);
}

#[tokio::test]
async fn a_multi_line_greeting_does_not_desynchronise_the_conversation() {
    // The fake relay answers EHLO with three lines, as every real one does. A client that
    // read only the first would send MAIL FROM into the middle of the reply and then
    // mis-attribute every code after it.
    let (host, port, sessions) = relay(Relay::Willing).await;

    channel(&host, port)
        .deliver(&notification("CPU hot"))
        .await
        .expect("delivered");

    let session = session_of(&sessions).await;
    assert!(
        session.commands.iter().any(|c| c == "DATA"),
        "the conversation reached DATA: {:?}",
        session.commands
    );
    assert!(!session.message.is_empty(), "and the message arrived");
}

#[tokio::test]
async fn a_relay_that_refuses_a_recipient_is_recorded_with_what_it_said() {
    // "550 5.7.1 relay denied" is the sentence an operator needs — it says the relay will
    // not send on their behalf, which is a configuration they can fix.
    let (host, port, _sessions) = relay(Relay::RefusesRecipient).await;

    let error = channel(&host, port)
        .deliver(&notification("CPU hot"))
        .await
        .expect_err("the relay refused");

    assert!(error.contains("550"), "{error}");
    assert!(error.contains("relay denied"), "{error}");
}

#[tokio::test]
async fn a_relay_that_is_not_there_fails_without_taking_the_cycle_with_it() {
    // Port 1 on loopback: the connection is refused immediately rather than hanging, so
    // this costs a millisecond rather than the ten-second timeout.
    let error = channel("127.0.0.1", 1)
        .deliver(&notification("CPU hot"))
        .await
        .expect_err("nothing is listening");

    assert!(error.contains("could not be reached"), "{error}");
}
