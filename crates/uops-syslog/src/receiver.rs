//! Getting messages off the wire.
//!
//! Two transports, and they fail in opposite directions — which is the whole reason they
//! are written separately rather than behind one abstraction.
//!
//! **UDP cannot push back.** A datagram that arrives when there is nowhere to put it is
//! gone; the sender will never know and never retry. So the only honest thing a UDP
//! receiver can do is drop it *and say so*. SPEC: *"Track and expose drop counts — a
//! silently dropping syslog receiver is worse than none."*
//!
//! **TCP can push back.** Not reading from a socket makes the kernel shrink the receive
//! window, which makes the sender slow down. A TCP receiver that dropped messages when it
//! was busy would be throwing away something it could simply have taken more slowly.
//!
//! # What "expose drop counts" means here, and what it does not
//!
//! [`Stats`] counts what this process did: datagrams read, and datagrams dropped because
//! the pipeline behind it was full. Those are the drops this code causes and can fix.
//!
//! It does **not** count datagrams the kernel discarded before we ever saw them, because
//! its receive buffer was full. Those need `SO_RXQ_OVFL` and a `recvmsg` with control
//! messages, which is Linux-only and is not wired up. What *is* done about them is the
//! part SPEC names: [`Config::receive_buffer`] raises `SO_RCVBUF` explicitly, and
//! [`UdpReceiver::receive_buffer`] reports what the kernel actually granted — which is
//! often half of what was asked for, and on Linux is capped by `net.core.rmem_max`.
//! A receiver that asked for 8 MB, silently got 208 KB and reported success would be
//! exactly the silent dropping SPEC is warning about.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use chrono::{DateTime, Utc};
use tokio::sync::mpsc;

use crate::framing::{Framer, FramingError};
use crate::{Message, parse};

/// The largest datagram a syslog receiver will read.
///
/// One datagram is one message, so this is also the largest message UDP can carry. 64 KiB
/// is the IPv4 payload ceiling and matches [`crate::framing::MAX_MESSAGE`], so the two
/// transports agree about what is too big.
pub const MAX_DATAGRAM: usize = 64 * 1024;

/// One message, and where it came from.
#[derive(Clone, Debug)]
pub struct Received {
    pub message: Message,
    /// The sender's address. Not the device's identity — a relay forwards other devices'
    /// messages under its own address, and the hostname in the message is what identity
    /// resolution should believe. Kept because it is the only thing that is certainly
    /// true, and because it is what an operator needs when a device is lying about its
    /// hostname.
    pub peer: SocketAddr,
    /// When this process read it.
    ///
    /// Distinct from `message.timestamp`, which is when the *device* says it happened.
    /// Device clocks are wrong often enough that keeping only one of these would make
    /// either the timeline or the ingest lag unknowable.
    pub received_at: DateTime<Utc>,
}

/// What a receiver has done so far.
#[derive(Debug, Default)]
pub struct Stats {
    received: AtomicU64,
    dropped: AtomicU64,
}

impl Stats {
    /// Messages read off the wire and handed on.
    pub fn received(&self) -> u64 {
        self.received.load(Ordering::Relaxed)
    }

    /// Messages this process dropped because the pipeline was full.
    ///
    /// UDP only. A non-zero value here is the signal that ingest cannot keep up, and it
    /// is the number SPEC insists must not be silent.
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

/// How a receiver is set up.
#[derive(Clone, Copy, Debug)]
pub struct Config {
    /// What to ask for as the socket receive buffer, in bytes.
    ///
    /// SPEC: *"64 KB buffer, `SO_RCVBUF` raised explicitly."* The default here is far
    /// larger than one datagram, and deliberately: the buffer's job is to hold the burst
    /// that arrives while this process is busy writing the last batch to `ClickHouse`. A
    /// buffer sized for one message drops the rest of the burst.
    ///
    /// 8 MiB is about two seconds of a 50 000 message/s stream at typical sizes, which is
    /// the pause a batch insert can cause.
    pub receive_buffer: usize,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            receive_buffer: 8 * 1024 * 1024,
        }
    }
}

/// Syslog over UDP.
pub struct UdpReceiver {
    socket: tokio::net::UdpSocket,
    stats: Arc<Stats>,
    granted_buffer: usize,
}

impl std::fmt::Debug for UdpReceiver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UdpReceiver")
            .field("local", &self.socket.local_addr().ok())
            .field("receive_buffer", &self.granted_buffer)
            .finish_non_exhaustive()
    }
}

impl UdpReceiver {
    /// Bind, and raise the receive buffer.
    ///
    /// # Errors
    ///
    /// When the address cannot be bound. A receive buffer the kernel will not grant is
    /// **not** an error — it is reported through [`UdpReceiver::receive_buffer`], because
    /// a receiver that refused to start over a tuning parameter would be worse than one
    /// that started and said what it got.
    pub fn bind(address: SocketAddr, config: Config) -> std::io::Result<Self> {
        let socket = socket2::Socket::new(
            if address.is_ipv4() {
                socket2::Domain::IPV4
            } else {
                socket2::Domain::IPV6
            },
            socket2::Type::DGRAM,
            Some(socket2::Protocol::UDP),
        )?;

        // Before bind: on some platforms SO_RCVBUF is only honoured beforehand.
        //
        // The error is ignored rather than propagated. Every kernel clamps this — Linux
        // to net.core.rmem_max, and it doubles what it grants for bookkeeping — so
        // failing here would mean refusing to start on a machine whose sysctl is
        // conservative. What matters is reporting the truth afterwards.
        let _ = socket.set_recv_buffer_size(config.receive_buffer);
        socket.set_nonblocking(true)?;
        socket.bind(&address.into())?;

        let granted = socket.recv_buffer_size().unwrap_or(0);
        // `from_std` registers the socket with the current runtime's reactor, so this
        // must be called from inside one even though nothing here awaits.
        let socket = tokio::net::UdpSocket::from_std(socket.into())?;

        Ok(Self {
            socket,
            stats: Arc::new(Stats::default()),
            granted_buffer: granted,
        })
    }

    /// What the kernel actually granted as the receive buffer.
    ///
    /// Compare with what was asked for. Linux reports double what it set aside, and caps
    /// the request at `net.core.rmem_max` — which on a stock install is 208 KiB, far below
    /// what a busy syslog receiver wants. An operator seeing a low number here has a
    /// sysctl to change; without it they would have an unexplained drop counter.
    #[must_use]
    pub const fn receive_buffer(&self) -> usize {
        self.granted_buffer
    }

    /// The address actually bound, which matters when the port was 0.
    ///
    /// # Errors
    ///
    /// When the socket has no local address.
    pub fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.socket.local_addr()
    }

    #[must_use]
    pub fn stats(&self) -> Arc<Stats> {
        Arc::clone(&self.stats)
    }

    /// Read until told to stop.
    ///
    /// A datagram that cannot be handed on is dropped and counted — see the module docs
    /// on why that is the only honest option for UDP.
    pub async fn run(self, sink: mpsc::Sender<Received>, shutdown: impl Future<Output = ()>) {
        let mut buffer = vec![0u8; MAX_DATAGRAM];
        let mut shutdown = std::pin::pin!(shutdown);

        loop {
            tokio::select! {
                () = &mut shutdown => return,
                result = self.socket.recv_from(&mut buffer) => {
                    let Ok((len, peer)) = result else {
                        // A datagram-socket read error is per-datagram — an ICMP port
                        // unreachable from a previous send, most often — and is not a
                        // reason to stop listening.
                        continue;
                    };

                    let received = Received {
                        message: parse(&String::from_utf8_lossy(&buffer[..len])),
                        peer,
                        received_at: Utc::now(),
                    };

                    // try_send, not send. Awaiting a full channel would stop reading the
                    // socket, and every datagram that arrived meanwhile would be dropped
                    // by the kernel instead — invisibly, which is the outcome SPEC calls
                    // worse than none. Dropping here is visible.
                    match sink.try_send(received) {
                        Ok(()) => { self.stats.received.fetch_add(1, Ordering::Relaxed); }
                        Err(mpsc::error::TrySendError::Full(_)) => {
                            self.stats.dropped.fetch_add(1, Ordering::Relaxed);
                        }
                        Err(mpsc::error::TrySendError::Closed(_)) => return,
                    }
                }
            }
        }
    }
}

/// Syslog over TCP.
pub struct TcpReceiver {
    listener: tokio::net::TcpListener,
    stats: Arc<Stats>,
}

impl std::fmt::Debug for TcpReceiver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TcpReceiver")
            .field("local", &self.listener.local_addr().ok())
            .finish_non_exhaustive()
    }
}

impl TcpReceiver {
    /// Listen.
    ///
    /// # Errors
    ///
    /// When the address cannot be bound.
    pub async fn bind(address: SocketAddr) -> std::io::Result<Self> {
        Ok(Self {
            listener: tokio::net::TcpListener::bind(address).await?,
            stats: Arc::new(Stats::default()),
        })
    }

    /// The address actually bound, which matters when the port was 0.
    ///
    /// # Errors
    ///
    /// When the socket has no local address.
    pub fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.listener.local_addr()
    }

    #[must_use]
    pub fn stats(&self) -> Arc<Stats> {
        Arc::clone(&self.stats)
    }

    /// Accept until told to stop, serving each connection concurrently.
    pub async fn run(self, sink: mpsc::Sender<Received>, shutdown: impl Future<Output = ()>) {
        let mut shutdown = std::pin::pin!(shutdown);

        loop {
            tokio::select! {
                () = &mut shutdown => return,
                accepted = self.listener.accept() => {
                    let Ok((stream, peer)) = accepted else {
                        // One failed accept — a file-descriptor limit, a connection
                        // reset between SYN and accept — is not a reason to stop
                        // listening to everything else.
                        continue;
                    };
                    let sink = sink.clone();
                    let stats = Arc::clone(&self.stats);
                    tokio::spawn(async move {
                        connection(stream, peer, sink, stats).await;
                    });
                }
            }
        }
    }
}

/// One TCP connection, until it closes or misframes.
async fn connection(
    mut stream: tokio::net::TcpStream,
    peer: SocketAddr,
    sink: mpsc::Sender<Received>,
    stats: Arc<Stats>,
) {
    use tokio::io::AsyncReadExt as _;

    let mut framer = Framer::new();
    let mut buffer = vec![0u8; 16 * 1024];

    // A read of zero is a clean close; an error is an abrupt one. Both end the
    // connection, and neither is worth distinguishing here — a sender that resets instead
    // of closing is a sender, not a fault.
    while let Ok(read @ 1..) = stream.read(&mut buffer).await {
        if let Err(e) = framer.push(&buffer[..read]) {
            report(&peer, e);
            return;
        }

        loop {
            match framer.next_message() {
                Ok(Some(raw)) => {
                    let received = Received {
                        message: parse(&raw),
                        peer,
                        received_at: Utc::now(),
                    };
                    // `send`, not `try_send`. Awaiting here is the backpressure: this task
                    // stops reading, the kernel's receive window shrinks, and the sender
                    // slows down. Nothing is lost — which is the difference between TCP
                    // and UDP and the reason they are not behind one abstraction.
                    if sink.send(received).await.is_err() {
                        return;
                    }
                    stats.received.fetch_add(1, Ordering::Relaxed);
                }
                Ok(None) => break,
                Err(e) => {
                    report(&peer, e);
                    return;
                }
            }
        }
    }

    // A line-framed sender that closed without a trailing newline has still sent a
    // message, and it is the last thing it said.
    if let Some(raw) = framer.finish() {
        let received = Received {
            message: parse(&raw),
            peer,
            received_at: Utc::now(),
        };
        if sink.send(received).await.is_ok() {
            stats.received.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// Say why a connection was dropped.
///
/// Once framing fails there is no way to find where the next message starts, so the
/// connection ends — and an operator needs to know which sender is doing it. One line per
/// connection, not per message, because a misframing sender reconnects.
fn report(peer: &SocketAddr, error: FramingError) {
    eprintln!("syslog: closing {peer}: {error}");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Severity;

    fn udp_pair(
        capacity: usize,
    ) -> (
        UdpReceiver,
        mpsc::Sender<Received>,
        mpsc::Receiver<Received>,
    ) {
        let receiver =
            UdpReceiver::bind("127.0.0.1:0".parse().unwrap(), Config::default()).expect("bind");
        let (tx, rx) = mpsc::channel(capacity);
        (receiver, tx, rx)
    }

    #[tokio::test]
    async fn a_datagram_becomes_a_parsed_message() {
        let (receiver, tx, mut rx) = udp_pair(16);
        let address = receiver.local_addr().expect("addr");
        let stats = receiver.stats();

        let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
        tokio::spawn(receiver.run(tx, async {
            let _ = stopped.await;
        }));

        let sender = tokio::net::UdpSocket::bind("127.0.0.1:0")
            .await
            .expect("sender");
        sender
            .send_to(b"<34>Oct 11 22:14:15 host su: it happened", address)
            .await
            .expect("send");

        let got = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
            .await
            .expect("a datagram within five seconds")
            .expect("a message");

        assert_eq!(got.message.severity, Severity::Critical);
        assert_eq!(got.message.hostname.as_deref(), Some("host"));
        assert_eq!(got.message.message, "it happened");
        assert_eq!(got.peer.ip().to_string(), "127.0.0.1");
        assert_eq!(stats.received(), 1);
        assert_eq!(stats.dropped(), 0);

        let _ = stop.send(());
    }

    #[tokio::test]
    async fn a_datagram_that_cannot_be_handed_on_is_dropped_and_counted() {
        // The property SPEC insists on. UDP cannot push back — the sender will never know
        // and never retry — so the only honest option is to drop it and say so. A
        // receiver that awaited a full channel would stop reading the socket and the
        // kernel would drop the rest *invisibly*, which is the outcome SPEC calls worse
        // than none.
        let (receiver, tx, mut rx) = udp_pair(1);
        let address = receiver.local_addr().expect("addr");
        let stats = receiver.stats();

        let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
        tokio::spawn(receiver.run(tx, async {
            let _ = stopped.await;
        }));

        let sender = tokio::net::UdpSocket::bind("127.0.0.1:0")
            .await
            .expect("sender");
        for i in 0..64 {
            let _ = sender
                .send_to(format!("<34>message {i}").as_bytes(), address)
                .await;
        }

        // Wait until the counters stop moving: UDP may lose some in the loopback stack
        // too, and what is asserted is that this process accounted for what it saw.
        let mut last = (0, 0);
        for _ in 0..50 {
            tokio::time::sleep(std::time::Duration::from_millis(40)).await;
            let now = (stats.received(), stats.dropped());
            if now == last && now.0 + now.1 > 0 {
                break;
            }
            last = now;
        }

        assert!(
            stats.dropped() > 0,
            "a full pipeline must produce visible drops: received {}, dropped {}",
            stats.received(),
            stats.dropped()
        );
        assert!(rx.try_recv().is_ok(), "and what fitted must still arrive");

        let _ = stop.send(());
    }

    #[tokio::test]
    async fn the_granted_receive_buffer_is_reported_rather_than_assumed() {
        // Every kernel clamps SO_RCVBUF. A receiver that asked for 8 MB, silently got
        // 208 KB and reported success would be exactly the silent dropping SPEC warns
        // about — so what it got is readable.
        let receiver = UdpReceiver::bind(
            "127.0.0.1:0".parse().unwrap(),
            Config {
                receive_buffer: 8 * 1024 * 1024,
            },
        )
        .expect("bind");

        assert!(
            receiver.receive_buffer() >= MAX_DATAGRAM,
            "a receive buffer smaller than one datagram cannot work: {}",
            receiver.receive_buffer()
        );
    }

    #[tokio::test]
    async fn a_tcp_connection_delivers_line_framed_messages() {
        use tokio::io::AsyncWriteExt as _;

        let receiver = TcpReceiver::bind("127.0.0.1:0".parse().unwrap())
            .await
            .expect("bind");
        let address = receiver.local_addr().expect("addr");
        let stats = receiver.stats();
        let (tx, mut rx) = mpsc::channel(16);

        let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
        tokio::spawn(receiver.run(tx, async {
            let _ = stopped.await;
        }));

        let mut client = tokio::net::TcpStream::connect(address)
            .await
            .expect("connect");
        client
            .write_all(b"<34>one\n<34>two\n")
            .await
            .expect("write");

        for expected in ["one", "two"] {
            let got = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
                .await
                .expect("a message within five seconds")
                .expect("a message");
            assert_eq!(got.message.message, expected);
        }
        assert_eq!(stats.received(), 2);

        let _ = stop.send(());
    }

    #[tokio::test]
    async fn a_tcp_connection_delivers_octet_counted_messages() {
        use tokio::io::AsyncWriteExt as _;

        let receiver = TcpReceiver::bind("127.0.0.1:0".parse().unwrap())
            .await
            .expect("bind");
        let address = receiver.local_addr().expect("addr");
        let (tx, mut rx) = mpsc::channel(16);

        let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
        tokio::spawn(receiver.run(tx, async {
            let _ = stopped.await;
        }));

        // A message containing a newline, which is the case octet counting exists for:
        // line framing would split this into three rows.
        let payload = "<34>stack trace:\n  at one\n  at two";
        let mut client = tokio::net::TcpStream::connect(address)
            .await
            .expect("connect");
        client
            .write_all(format!("{} {payload}", payload.len()).as_bytes())
            .await
            .expect("write");

        let got = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
            .await
            .expect("a message within five seconds")
            .expect("a message");
        assert!(got.message.message.contains("at two"), "{:?}", got.message);

        let _ = stop.send(());
    }

    #[tokio::test]
    async fn a_tcp_sender_that_closes_without_a_newline_keeps_its_last_message() {
        use tokio::io::AsyncWriteExt as _;

        let receiver = TcpReceiver::bind("127.0.0.1:0".parse().unwrap())
            .await
            .expect("bind");
        let address = receiver.local_addr().expect("addr");
        let (tx, mut rx) = mpsc::channel(16);

        let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
        tokio::spawn(receiver.run(tx, async {
            let _ = stopped.await;
        }));

        let mut client = tokio::net::TcpStream::connect(address)
            .await
            .expect("connect");
        client
            .write_all(b"<34>the last thing it said")
            .await
            .expect("write");
        client.shutdown().await.expect("close");

        let got = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
            .await
            .expect("a message within five seconds")
            .expect("a message");
        assert_eq!(got.message.message, "the last thing it said");

        let _ = stop.send(());
    }

    #[tokio::test]
    async fn a_malformed_datagram_still_arrives() {
        // The rule, at the transport level: what reaches the pipeline is a message with
        // `parse_error` set and the raw text intact, not nothing.
        let (receiver, tx, mut rx) = udp_pair(16);
        let address = receiver.local_addr().expect("addr");

        let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
        tokio::spawn(receiver.run(tx, async {
            let _ = stopped.await;
        }));

        let sender = tokio::net::UdpSocket::bind("127.0.0.1:0")
            .await
            .expect("sender");
        sender
            .send_to(b"this has no priority at all", address)
            .await
            .expect("send");

        let got = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
            .await
            .expect("a datagram within five seconds")
            .expect("a message");
        assert!(got.message.parse_error.is_some());
        assert_eq!(got.message.message, "this has no priority at all");

        let _ = stop.send(());
    }
}
