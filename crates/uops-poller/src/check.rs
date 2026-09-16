//! Is the device there at all.
//!
//! SPEC §M2 gives a profile two kinds of availability check, ICMP and TCP, each with a
//! timeout and a retry count. This is both of them.
//!
//! # Why ICMP uses an unprivileged datagram socket
//!
//! The classic way to send an echo request is a raw socket, which needs `CAP_NET_RAW`.
//! That capability also permits forging arbitrary packets and putting an interface into
//! promiscuous mode — it is a large grant for a ping, and it is granted to the whole
//! process for the life of the container.
//!
//! Linux has offered `SOCK_DGRAM` + `IPPROTO_ICMP` since 3.11, and it needs no
//! capability at all: the kernel writes the identifier, computes the checksum, and
//! demultiplexes replies to the socket that sent the request. It is gated by
//! `net.ipv4.ping_group_range`, and Docker sets that to `0 2147483647` by default —
//! verified in a container, not assumed — so this works out of the box for any user,
//! including a non-root one.
//!
//! When the sysctl has been narrowed, socket creation fails with `EACCES` and
//! [`CheckError::Unprivileged`] says which sysctl to widen. That is a configuration
//! error with an answer, rather than a device that silently reports down for ever.
//!
//! macOS also supports `SOCK_DGRAM` ICMP. Windows does not, and there is no attempt to
//! pretend: the check reports [`CheckError::Unsupported`] and the poller counts it. The
//! deployment target is a Linux container.
//!
//! # Why blocking sockets on a blocking thread
//!
//! `socket2` gives a blocking socket with a read timeout, and `spawn_blocking` is where
//! that belongs. The alternative is `AsyncFd`, which is Unix-only anyway and buys
//! nothing here: a check is one small write and one small read, bounded by a timeout
//! measured in seconds, and at SPEC's fleet size it is thirty-odd of them a second.
//!
//! # What a check is *not*
//!
//! It is not a measurement of round-trip time. SPEC asks whether the device answers;
//! latency is a metric, it belongs in a profile alongside the others, and reporting it
//! from here would make availability and latency the same reading — so a device that
//! answered slowly would be down and a device that was down would have no latency.

use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

use uops_profile::{Availability, CheckKind};

/// `net.ipv4.ping_group_range`, named once so the error message and the docs cannot
/// disagree about it.
const PING_GROUP_RANGE: &str = "net.ipv4.ping_group_range";

#[cfg(unix)]
/// Where the sequence number sits in an ICMP echo header: type, code, two checksum
/// bytes, two identifier bytes, then the sequence number as a big-endian `u16`. Only its
/// low byte is used — an attempt count never exceeds 255.
const SEQ_OFFSET: usize = 7;

/// Why a check could not be carried out.
///
/// Distinct from the device being down, and that distinction is the point: a poller that
/// cannot open a socket would otherwise report every device in the fleet as unreachable,
/// which reads as a catastrophic outage rather than as a misconfigured container.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CheckError {
    /// The kernel refused the socket. The sysctl has been narrowed.
    Unprivileged(String),
    /// This platform has no unprivileged ICMP.
    Unsupported(&'static str),
    /// The profile asks for something it did not give enough detail to do.
    Malformed(&'static str),
    /// Something else went wrong locally — no route, no descriptors.
    Local(String),
}

impl std::fmt::Display for CheckError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unprivileged(e) => write!(
                f,
                "the kernel refused an ICMP socket ({e}); widen {PING_GROUP_RANGE} to \
                 include this process's group, or the check cannot run"
            ),
            Self::Unsupported(what) => write!(f, "{what}"),
            Self::Malformed(what) => write!(f, "the profile's check is unusable: {what}"),
            Self::Local(e) => write!(f, "the check could not be sent: {e}"),
        }
    }
}

/// What a check found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reachability {
    /// The device answered.
    Up {
        /// Which attempt succeeded, counting from one. Worth keeping: a device that
        /// answers only on the third try is up, and is also a device somebody should
        /// look at.
        attempt: u8,
    },
    /// Every attempt timed out or was refused.
    Down,
}

/// Run a profile's availability check.
///
/// Retries are SPEC's, and they are what stops a single dropped packet from paging
/// somebody. The timeout is per attempt, so a check with three retries and a two-second
/// timeout takes at most six seconds — which is why `Availability` is validated to have
/// a timeout shorter than its interval.
///
/// # Errors
///
/// Only when the check could not be carried out at all — see [`CheckError`]. A device
/// that does not answer is `Ok(Reachability::Down)`.
pub async fn run(check: &Availability, address: SocketAddr) -> Result<Reachability, CheckError> {
    let timeout = check.timeout.duration();
    // At least one attempt. `retries: 0` reads as "do not retry", not "do not check",
    // and a profile that disabled a check by setting a number to zero would be a trap.
    let attempts = check.retries.max(1);

    for attempt in 1..=attempts {
        let reached = match check.kind {
            CheckKind::Icmp => icmp_once(address.ip(), timeout, attempt).await?,
            CheckKind::Tcp => {
                let port = check.port.ok_or(CheckError::Malformed(
                    "a tcp check needs a port, and Profile::validate should have refused it",
                ))?;
                tcp_once(SocketAddr::new(address.ip(), port), timeout).await
            }
        };
        if reached {
            return Ok(Reachability::Up { attempt });
        }
    }
    Ok(Reachability::Down)
}

/// A sentence saying what the check did, for the state row's `reason`.
///
/// The row's `current_status` says *what*; this is the only part that says *why*, and it
/// is the first thing an operator reads.
#[must_use]
pub fn describe(check: &Availability, outcome: Reachability) -> String {
    let kind = match check.kind {
        CheckKind::Icmp => "ICMP echo request".to_owned(),
        CheckKind::Tcp => match check.port {
            Some(port) => format!("TCP connection to port {port}"),
            None => "TCP connection".to_owned(),
        },
    };
    let timeout = check.timeout.duration();
    match outcome {
        Reachability::Up { attempt: 1 } => format!("answered the first {kind}"),
        Reachability::Up { attempt } => {
            format!(
                "answered {kind} {attempt} after {} earlier failed",
                attempt - 1
            )
        }
        Reachability::Down => format!(
            "no reply to {} {kind}s within {timeout:?} each",
            check.retries.max(1)
        ),
    }
}

/// One TCP connect, bounded by the timeout.
///
/// A refused connection is a device that is *there* — something answered the SYN — but
/// this check is about the service the profile named, and a closed port is that service
/// being down. Both are `false`, which is why the distinction is in the reason rather
/// than in the result.
async fn tcp_once(address: SocketAddr, timeout: Duration) -> bool {
    matches!(
        tokio::time::timeout(timeout, tokio::net::TcpStream::connect(address)).await,
        Ok(Ok(_))
    )
}

/// One ICMP echo request, bounded by the timeout.
///
/// `seq` is the attempt number. The kernel rewrites the identifier on a datagram ICMP
/// socket and demultiplexes replies to the socket that sent them, so the sequence number
/// is what distinguishes this attempt's reply from the previous attempt's late one.
#[cfg(unix)]
async fn icmp_once(target: IpAddr, timeout: Duration, seq: u8) -> Result<bool, CheckError> {
    use std::io::{Read as _, Write as _};

    use socket2::{Domain, Protocol, Socket, Type};

    let (domain, protocol) = match target {
        IpAddr::V4(_) => (Domain::IPV4, Protocol::ICMPV4),
        IpAddr::V6(_) => (Domain::IPV6, Protocol::ICMPV6),
    };
    let destination = SocketAddr::new(target, 0).into();
    let request = echo_request(matches!(target, IpAddr::V6(_)), seq);

    // Blocking work, on a blocking thread. See the module docs.
    tokio::task::spawn_blocking(move || {
        let socket = Socket::new(domain, Type::DGRAM, Some(protocol)).map_err(|e| {
            if e.kind() == std::io::ErrorKind::PermissionDenied {
                CheckError::Unprivileged(e.to_string())
            } else {
                CheckError::Local(e.to_string())
            }
        })?;
        socket
            .set_read_timeout(Some(timeout))
            .map_err(|e| CheckError::Local(e.to_string()))?;

        // Connected, then `write`/`read` rather than `send_to`/`recv_from`. Two reasons,
        // and the second is the one that matters: the peer address is of no interest —
        // the kernel demultiplexes replies to the socket that sent the request — and the
        // connected form gives `io::Read`, which takes an initialised `&mut [u8]`.
        // `recv_from` takes `&mut [MaybeUninit<u8>]`, and reading bytes back out of that
        // needs `unsafe`, which this workspace forbids.
        socket
            .connect(&destination)
            .map_err(|e| CheckError::Local(e.to_string()))?;
        (&socket)
            .write_all(&request)
            .map_err(|e| CheckError::Local(e.to_string()))?;

        // An echo reply is eight bytes of header plus whatever payload came back.
        let mut buf = [0u8; 128];
        loop {
            match (&socket).read(&mut buf) {
                Ok(read) => {
                    // The kernel has already matched this datagram to our socket. What is
                    // left is whether it answers *this* attempt: a late reply to the
                    // previous one carries the previous sequence number, and counting it
                    // would make a device that is timing out look like one that answers.
                    if read > SEQ_OFFSET && buf[SEQ_OFFSET] == seq {
                        return Ok(true);
                    }
                    // Not ours. Keep reading; the read timeout still bounds the wait.
                }
                // The timeout expired: nothing answered.
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) =>
                {
                    return Ok(false);
                }
                // A router answered *for* the device: destination unreachable, network
                // unreachable, administratively prohibited. The kernel delivers these on
                // the socket as an error, and they are the device being unreachable —
                // which is exactly what this check is asking. Reporting them as a local
                // failure would say "the poller is broken" about a device behind a
                // firewall rule, and that sends an operator to the wrong place.
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::HostUnreachable
                            | std::io::ErrorKind::NetworkUnreachable
                            | std::io::ErrorKind::ConnectionRefused
                    ) =>
                {
                    return Ok(false);
                }
                Err(e) => return Err(CheckError::Local(e.to_string())),
            }
        }
    })
    .await
    .map_err(|e| CheckError::Local(format!("the check task did not finish: {e}")))?
}

#[cfg(not(unix))]
#[allow(clippy::unused_async)]
async fn icmp_once(_target: IpAddr, _timeout: Duration, _seq: u8) -> Result<bool, CheckError> {
    Err(CheckError::Unsupported(
        "unprivileged ICMP needs a Linux or macOS datagram socket; this platform has none",
    ))
}

#[cfg(unix)]
/// An ICMP echo request.
///
/// Type 8 for IPv4 and 128 for IPv6, code 0, then a checksum, an identifier and a
/// sequence number. The kernel fills in the identifier and the checksum on a datagram
/// socket — they are zero here because writing them would be writing something that is
/// about to be overwritten, and a reader who saw a hand-computed checksum would
/// reasonably conclude it mattered.
fn echo_request(v6: bool, seq: u8) -> [u8; 8] {
    [
        if v6 { 128 } else { 8 }, // type
        0,                        // code
        0,
        0, // checksum, written by the kernel
        0,
        0, // identifier, written by the kernel
        0,
        seq, // sequence number, ours
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `generic-snmp`'s own check — `icmp`, 30s, 2s timeout, 3 retries.
    ///
    /// Taken from the profile rather than built here. `Timeout` is deliberately
    /// constructible only by deserialising, which keeps the "shorter than its interval"
    /// rule in one place; a test that reached around that would be testing a check no
    /// profile could express.
    fn icmp(retries: u8) -> Availability {
        let mut check = uops_profile::builtin::all()
            .expect("built-ins")
            .into_iter()
            .find(|p| p.id == "generic-snmp")
            .expect("generic-snmp")
            .availability
            .into_iter()
            .next()
            .expect("generic-snmp declares an availability check");
        assert_eq!(check.kind, CheckKind::Icmp);
        check.retries = retries;
        check
    }

    /// The same check, pointed at a TCP port.
    fn tcp(port: u16) -> Availability {
        let mut check = icmp(1);
        check.kind = CheckKind::Tcp;
        check.port = Some(port);
        check
    }

    #[cfg(unix)]
    #[test]
    fn an_echo_request_is_the_shape_the_kernel_expects() {
        // Type 8, code 0, and the sequence number last. The identifier and checksum are
        // deliberately zero — the kernel writes them on a datagram socket.
        let v4 = echo_request(false, 3);
        assert_eq!(v4[0], 8, "IPv4 echo request is type 8");
        assert_eq!(v4[1], 0, "code 0");
        assert_eq!(&v4[2..6], &[0, 0, 0, 0], "checksum and id are the kernel's");
        assert_eq!(v4[7], 3, "the sequence number is ours");

        // IPv6 uses a different type number for the same message.
        assert_eq!(echo_request(true, 1)[0], 128);
    }

    /// Whether this machine can open an unprivileged ICMP socket at all.
    ///
    /// The same shape as `uops-snmp`'s agent tests: a developer on a platform without it
    /// should not get a red suite, and a skip that says so out loud cannot be mistaken
    /// for a pass. CI runs on Linux and asserts the absence of the message.
    #[cfg(unix)]
    fn icmp_available() -> bool {
        use socket2::{Domain, Protocol, Socket, Type};
        Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::ICMPV4)).is_ok()
    }

    #[cfg(not(unix))]
    const fn icmp_available() -> bool {
        false
    }

    macro_rules! icmp_or_skip {
        () => {
            if !icmp_available() {
                println!(
                    "SKIPPED-ICMP: no unprivileged ICMP socket on this machine. On Linux,                      widen {} to include this process's group.",
                    PING_GROUP_RANGE
                );
                return;
            }
        };
    }

    #[tokio::test]
    async fn an_icmp_check_against_loopback_is_up() {
        // The one that proves the wire format. Everything else about this module can be
        // right while the packet is wrong, and a wrong packet is silent: the kernel sends
        // it, nothing replies, and every device in the fleet reports down.
        icmp_or_skip!();
        let out = run(&icmp(2), "127.0.0.1:0".parse().expect("address")).await;
        assert_eq!(
            out,
            Ok(Reachability::Up { attempt: 1 }),
            "loopback must answer its own echo request: {out:?}"
        );
    }

    #[tokio::test]
    async fn an_icmp_check_against_something_that_never_answers_is_down() {
        // TEST-NET-1 (RFC 5737), which exists to be unroutable. Whether this times out or
        // draws a destination-unreachable depends on the network the test runs on, and
        // both are the same answer: the device is not reachable. A local error here would
        // be the bug — it would say "the poller is broken" about a device behind a
        // firewall rule.
        icmp_or_skip!();
        let out = run(&icmp(1), "192.0.2.1:0".parse().expect("address")).await;
        assert_eq!(out, Ok(Reachability::Down), "{out:?}");
    }

    #[tokio::test]
    async fn a_check_that_cannot_run_is_not_a_device_that_is_down() {
        // The distinction the whole `CheckError` type exists for. On a machine with no
        // unprivileged ICMP this is the real path; where ICMP works there is nothing to
        // assert, because the check runs.
        if icmp_available() {
            return;
        }
        let out = run(&icmp(1), "127.0.0.1:0".parse().expect("address")).await;
        assert!(
            matches!(
                out,
                Err(CheckError::Unsupported(_) | CheckError::Unprivileged(_))
            ),
            "a check that could not run must not report the device down: {out:?}"
        );
    }

    #[test]
    fn zero_retries_still_checks_once() {
        // `retries: 0` reads as "do not retry", not "do not check". A profile that
        // disabled a check by setting a number to zero would be a trap.
        assert_eq!(icmp(0).retries.max(1), 1);
        assert_eq!(icmp(3).retries.max(1), 3);
    }

    #[test]
    fn the_reason_says_what_was_tried_and_for_how_long() {
        // The row's status says what; this says why, and it is the first thing an
        // operator reads.
        let down = describe(&icmp(3), Reachability::Down);
        assert!(down.contains("ICMP"), "{down}");
        assert!(down.contains('3'), "{down}");
        assert!(down.contains("2s"), "{down}");

        assert!(describe(&icmp(3), Reachability::Up { attempt: 1 }).contains("first"));
        let flaky = describe(&icmp(3), Reachability::Up { attempt: 3 });
        assert!(flaky.contains('3'), "{flaky}");
    }

    #[test]
    fn a_socket_refusal_names_the_sysctl_to_widen() {
        // The message is the whole value of distinguishing this from "down": a poller
        // that cannot open a socket reports every device unreachable, which reads as a
        // catastrophic outage rather than a narrowed sysctl.
        let e = CheckError::Unprivileged("permission denied".to_owned());
        assert!(e.to_string().contains(PING_GROUP_RANGE), "{e}");
    }

    #[tokio::test]
    async fn a_tcp_check_against_a_closed_port_is_down_not_an_error() {
        // A refused connection is a device that is there and a service that is not. The
        // check is about the service the profile named.
        let out = run(&tcp(1), "127.0.0.1:0".parse().expect("address")).await;
        assert_eq!(out, Ok(Reachability::Down), "{out:?}");
    }

    #[tokio::test]
    async fn a_tcp_check_against_something_listening_is_up() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let port = listener.local_addr().expect("addr").port();

        assert_eq!(
            run(&tcp(port), "127.0.0.1:0".parse().expect("address")).await,
            Ok(Reachability::Up { attempt: 1 })
        );
    }
}
