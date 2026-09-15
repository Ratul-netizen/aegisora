//! The real transport: `snmp2` over UDP.
//!
//! # One socket per device, pooled — and why not the alternatives
//!
//! This was the open decision. Three options, and the API settles it more than taste
//! does.
//!
//! **A session per request** is the simplest and is wrong for v3. `AsyncSession::init`
//! performs engine-ID discovery — a round trip — and it resets the engine state every
//! time it is called. A twenty-page walk would pay twenty discoveries, doubling the
//! traffic to exactly the agents least able to absorb it.
//!
//! **Caching the engine parameters instead of the session** would be cheaper — a few
//! bytes per device rather than a file descriptor — and `v3::Security` even has
//! `with_engine_id` and `with_engine_boots_and_time` for it. It is not reachable:
//! `AsyncSession` keeps its `Security` private and `init()` unconditionally resets it,
//! so there is no way to read discovered state back out. Worth writing down, because
//! from the outside it looks like the obvious design.
//!
//! **A session per device, held in a bounded pool**, is what is left and is also what
//! is right. Discovery is paid once per device per eviction rather than once per
//! request. The pool is bounded so a ten-thousand-device fleet does not become ten
//! thousand file descriptors: at the executor's default of 256 concurrent polls, a much
//! smaller pool than the fleet is enough, because a device not currently being polled
//! does not need a socket.
//!
//! # Sessions are not shared concurrently
//!
//! `getbulk` takes `&mut self`, so a session is one in-flight request by construction.
//! That is the same limit the executor imposes per device for a different reason — an
//! agent's queue is small — and the two agreeing is a convenience rather than a
//! coincidence: both follow from a device being a single small server.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use snmp2::{Oid as SnmpOid, Value as SnmpValue, v3};
use tokio::sync::Mutex;
use uops_core::{CredentialMaterial, Secret};
use uops_profile::Oid;

use crate::bulk::Repetitions;
use crate::transport::{Target, Transport, TransportError, Value, VarBind};

/// How long to wait for one response.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(5);

/// How many device sessions to keep open.
///
/// Each is one UDP socket. The executor's default concurrency is 256, so a pool this
/// size holds a session for every device that could be mid-poll, plus room for the next
/// wave to reuse rather than rediscover.
pub const DEFAULT_POOL: usize = 512;

/// An `snmp2`-backed transport.
pub struct UdpTransport {
    sessions: Mutex<HashMap<SocketAddr, Arc<Mutex<snmp2::AsyncSession>>>>,
    order: Mutex<Vec<SocketAddr>>,
    credential: Arc<Secret<CredentialMaterial>>,
    timeout: Duration,
    pool: usize,
}

impl std::fmt::Debug for UdpTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never the credential.
        f.debug_struct("UdpTransport")
            .field("timeout", &self.timeout)
            .field("pool", &self.pool)
            .finish_non_exhaustive()
    }
}

impl UdpTransport {
    /// A transport that talks to every device with the same credential.
    ///
    /// One credential per transport rather than per request: a poller instance is
    /// already per-tenant-per-credential in the scheduling above this, and passing it
    /// per call would mean a credential that lives as long as the call stack rather
    /// than as long as the poll.
    ///
    /// Takes the `Secret`, not the material. `CredentialMaterial` is deliberately not
    /// `Clone` — copying key material is not something a caller should be able to do by
    /// typing six characters — so a transport that wanted the bare value could only be
    /// built by a caller that had somehow obtained an owned one. Holding the wrapper
    /// also means the material is zeroized when the last transport is dropped, which
    /// the bare value was not.
    #[must_use]
    pub fn new(credential: Secret<CredentialMaterial>) -> Self {
        Self {
            sessions: Mutex::new(HashMap::new()),
            order: Mutex::new(Vec::new()),
            credential: Arc::new(credential),
            timeout: DEFAULT_TIMEOUT,
            pool: DEFAULT_POOL,
        }
    }

    #[must_use]
    pub const fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    #[must_use]
    pub const fn with_pool(mut self, pool: usize) -> Self {
        self.pool = pool;
        self
    }

    /// How many sessions are currently open. For a health endpoint.
    pub async fn open_sessions(&self) -> usize {
        self.sessions.lock().await.len()
    }

    /// The session for a device, opening one if there isn't one.
    async fn session(
        &self,
        address: SocketAddr,
    ) -> Result<Arc<Mutex<snmp2::AsyncSession>>, TransportError> {
        {
            let mut sessions = self.sessions.lock().await;
            if let Some(existing) = sessions.get(&address) {
                return Ok(Arc::clone(existing));
            }

            // Evict before opening, so the pool is a ceiling rather than a target.
            // Oldest first: a device polled long ago is the one least likely to be
            // polled again before the next eviction.
            if sessions.len() >= self.pool {
                let mut order = self.order.lock().await;
                if !order.is_empty() {
                    let oldest = order.remove(0);
                    sessions.remove(&oldest);
                }
            }
        }

        let session = self.open(address).await?;
        let shared = Arc::new(Mutex::new(session));

        let mut sessions = self.sessions.lock().await;
        self.order.lock().await.push(address);
        sessions.insert(address, Arc::clone(&shared));
        Ok(shared)
    }

    async fn open(&self, address: SocketAddr) -> Result<snmp2::AsyncSession, TransportError> {
        // A request id that is not zero and not the same for every session. snmp2
        // increments from here; starting every session at the same number makes two
        // devices' traffic indistinguishable in a capture, which is a debugging problem
        // rather than a correctness one.
        let start = i32::from_le_bytes([
            address.port().to_le_bytes()[0],
            address.port().to_le_bytes()[1],
            1,
            0,
        ]);

        match self.credential.expose() {
            CredentialMaterial::SnmpCommunity(community) => {
                snmp2::AsyncSession::new_v2c(address, community.as_bytes(), start)
                    .await
                    .map_err(|e| TransportError::Unreachable(e.to_string()))
            }
            CredentialMaterial::SnmpV3 {
                username,
                auth,
                auth_key,
                privacy,
                priv_key,
            } => {
                let security = v3::Security::new(username.as_bytes(), auth_key.as_bytes())
                    .with_auth_protocol(auth_protocol(*auth))
                    .with_auth(v3::Auth::AuthPriv {
                        cipher: cipher(*privacy),
                        privacy_password: priv_key.as_bytes().to_vec(),
                    });

                let mut session = snmp2::AsyncSession::new_v3(address, start, security)
                    .await
                    .map_err(|e| TransportError::Unreachable(e.to_string()))?;

                // Engine discovery. Once per session, which is the whole reason
                // sessions are pooled — see the module docs.
                //
                // Its failures are classified rather than collapsed. The first version
                // mapped every error here to AuthFailed, so a device that was simply
                // unreachable reported "the agent refused the credentials" — which
                // sends an operator to rotate a credential that was never wrong. A
                // closed port is not a bad password.
                match tokio::time::timeout(self.timeout, session.init()).await {
                    Err(_) => return Err(TransportError::Timeout),
                    Ok(Ok(())) => {}
                    Ok(Err(e)) => return Err(classify(&e)),
                }

                Ok(session)
            }
            other => Err(TransportError::Unreachable(format!(
                "not an SNMP credential: {}",
                kind_of(other)
            ))),
        }
    }

    /// Drop a device's session, so the next request rediscovers.
    ///
    /// For an authentication failure: v3 engine boots and time drift, and an agent that
    /// has restarted will reject a session carrying the old values with an error that
    /// looks exactly like a wrong password. Reopening is the fix, and doing it on the
    /// failure rather than on a timer means a rebooted device recovers on its next poll.
    async fn forget(&self, address: SocketAddr) {
        self.sessions.lock().await.remove(&address);
        self.order.lock().await.retain(|a| *a != address);
    }
}

const fn auth_protocol(p: uops_core::AuthProtocol) -> v3::AuthProtocol {
    match p {
        uops_core::AuthProtocol::Md5 => v3::AuthProtocol::Md5,
        uops_core::AuthProtocol::Sha1 => v3::AuthProtocol::Sha1,
        uops_core::AuthProtocol::Sha224 => v3::AuthProtocol::Sha224,
        uops_core::AuthProtocol::Sha256 => v3::AuthProtocol::Sha256,
        uops_core::AuthProtocol::Sha384 => v3::AuthProtocol::Sha384,
        uops_core::AuthProtocol::Sha512 => v3::AuthProtocol::Sha512,
    }
}

const fn cipher(p: uops_core::PrivProtocol) -> v3::Cipher {
    match p {
        uops_core::PrivProtocol::Des => v3::Cipher::Des,
        uops_core::PrivProtocol::Aes128 => v3::Cipher::Aes128,
        uops_core::PrivProtocol::Aes192 => v3::Cipher::Aes192,
        uops_core::PrivProtocol::Aes256 => v3::Cipher::Aes256,
    }
}

const fn kind_of(m: &CredentialMaterial) -> &'static str {
    match m {
        CredentialMaterial::SnmpCommunity(_) => "snmp community",
        CredentialMaterial::SnmpV3 { .. } => "snmp v3",
        CredentialMaterial::SshPassword { .. } => "ssh password",
        CredentialMaterial::SshKey { .. } => "ssh key",
        CredentialMaterial::ApiToken(_) => "api token",
    }
}

/// Our OID as `snmp2`'s.
fn to_snmp_oid(oid: &Oid) -> Result<SnmpOid<'static>, TransportError> {
    // asn1_rs::Oid arcs are u64; ours are u32, which is the SMI limit and fits.
    //
    // `from`, not `from_relative`. An SNMP OID is absolute, and a relative one is
    // encoded differently — the agent reads it as something else and answers from the
    // start of the MIB every time. The symptom is not an error: it is a walk that gets
    // the same first rows forever, which uops_snmp::walk reports as an agent that does
    // not advance. That check earned its place finding this.
    let arcs: Vec<u64> = oid.arcs().iter().map(|a| u64::from(*a)).collect();
    SnmpOid::from(&arcs)
        .map_err(|e| TransportError::Protocol(format!("{oid} is not encodable: {e:?}")))
}

/// `snmp2`'s value as ours.
///
/// Everything a profile can read maps; anything else becomes [`Value::Other`] and is
/// skipped by the caller rather than guessed at.
fn from_snmp_value(v: &SnmpValue<'_>) -> Value {
    match v {
        SnmpValue::Integer(n) => Value::Integer(*n),
        SnmpValue::Counter32(n) | SnmpValue::Unsigned32(n) | SnmpValue::Timeticks(n) => {
            Value::Unsigned(u64::from(*n))
        }
        SnmpValue::Counter64(n) => Value::Counter64(*n),
        SnmpValue::OctetString(b) => Value::Bytes((*b).to_vec()),
        SnmpValue::ObjectIdentifier(o) => {
            o.to_string().parse().map_or(Value::Other, Value::ObjectId)
        }
        SnmpValue::EndOfMibView => Value::EndOfMibView,
        SnmpValue::NoSuchInstance | SnmpValue::NoSuchObject => Value::NoSuchInstance,
        _ => Value::Other,
    }
}

#[async_trait::async_trait]
impl Transport for UdpTransport {
    async fn get_bulk(
        &self,
        target: &Target,
        after: &Oid,
        max_repetitions: Repetitions,
    ) -> Result<Vec<VarBind>, TransportError> {
        let oid = to_snmp_oid(after)?;
        let session = self.session(target.address).await?;

        let repetitions = max_repetitions.get();
        let result = {
            let mut guard = session.lock().await;
            // non-repeaters 0: every OID in the request is a table column to walk, not a
            // scalar to fetch once.
            match tokio::time::timeout(self.timeout, guard.getbulk(&[&oid], 0, repetitions)).await {
                Err(_) => return Err(TransportError::Timeout),
                Ok(Ok(pdu)) => Ok(pdu
                    .varbinds
                    .filter_map(|(o, v)| {
                        o.to_string().parse::<Oid>().ok().map(|oid| VarBind {
                            oid,
                            value: from_snmp_value(&v),
                        })
                    })
                    .collect::<Vec<_>>()),
                Ok(Err(e)) => Err(e),
            }
        };

        self.finish(target.address, result).await
    }

    async fn get_scalars(
        &self,
        target: &Target,
        oids: &[Oid],
    ) -> Result<Vec<VarBind>, TransportError> {
        if oids.is_empty() {
            return Ok(Vec::new());
        }

        // A GET of the instances, not a GETNEXT of the objects. Asking after an OID
        // that is already an instance skips the thing being asked for and answers with
        // whatever the agent holds next — a different metric's value under this
        // metric's name, which is a failure that produces plausible numbers rather than
        // an error. `instance` resolves both spellings a profile may use.
        let encoded: Vec<SnmpOid<'static>> = oids
            .iter()
            .map(|oid| to_snmp_oid(&crate::transport::instance(oid)))
            .collect::<Result<_, TransportError>>()?;
        let refs: Vec<&SnmpOid<'static>> = encoded.iter().collect();
        let session = self.session(target.address).await?;

        let result = {
            let mut guard = session.lock().await;
            // One PDU for the whole set: the difference between one round trip per
            // device per poll and one per metric.
            match tokio::time::timeout(self.timeout, guard.get_many(&refs)).await {
                Err(_) => return Err(TransportError::Timeout),
                Ok(Ok(pdu)) => Ok(pdu
                    .varbinds
                    .filter_map(|(o, v)| {
                        o.to_string().parse::<Oid>().ok().map(|oid| VarBind {
                            oid,
                            value: from_snmp_value(&v),
                        })
                    })
                    .collect::<Vec<_>>()),
                Ok(Err(e)) => Err(e),
            }
        };

        self.finish(target.address, result).await
    }
}

impl UdpTransport {
    /// Classify a request's outcome, dropping the session on what looks like an auth
    /// failure. Shared by both request shapes so they cannot drift apart.
    async fn finish(
        &self,
        address: SocketAddr,
        result: Result<Vec<VarBind>, snmp2::Error>,
    ) -> Result<Vec<VarBind>, TransportError> {
        match result {
            Ok(varbinds) => Ok(varbinds),
            Err(e) => {
                let classified = classify(&e);
                if classified == TransportError::AuthFailed {
                    // Almost always drifted engine counters or a rebooted agent rather
                    // than a wrong password — see forget().
                    self.forget(address).await;
                }
                Err(classified)
            }
        }
    }
}

/// An `snmp2` error as ours.
///
/// The distinction that matters is between "could not reach it" and "it refused the
/// credentials", because they send an operator to two completely different places.
fn classify(e: &snmp2::Error) -> TransportError {
    match e {
        snmp2::Error::AuthFailure(_) => TransportError::AuthFailed,
        snmp2::Error::Receive | snmp2::Error::Send => TransportError::Timeout,
        other => TransportError::Protocol(format!("{other:?}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use uops_core::Secret;
    use uops_core::{AuthProtocol, PrivProtocol};

    #[test]
    fn our_protocols_map_onto_snmp2s() {
        // A silent mismatch here would authenticate with the wrong algorithm and fail
        // in a way that looks like a wrong password.
        assert!(matches!(
            auth_protocol(AuthProtocol::Sha256),
            v3::AuthProtocol::Sha256
        ));
        assert!(matches!(
            auth_protocol(AuthProtocol::Md5),
            v3::AuthProtocol::Md5
        ));
        assert!(matches!(cipher(PrivProtocol::Aes256), v3::Cipher::Aes256));
        assert!(matches!(cipher(PrivProtocol::Des), v3::Cipher::Des));
    }

    #[test]
    fn an_oid_survives_the_round_trip_into_snmp2_and_back() {
        let ours: Oid = "1.3.6.1.2.1.31.1.1.1.6".parse().unwrap();
        let theirs = to_snmp_oid(&ours).unwrap();
        let back: Oid = theirs.to_string().parse().unwrap();
        assert_eq!(ours, back);
    }

    #[test]
    fn a_debug_print_never_shows_the_credential() {
        let t = UdpTransport::new(Secret::new(CredentialMaterial::SnmpV3 {
            username: "netops".into(),
            auth: AuthProtocol::Sha256,
            auth_key: "auth-key-material".into(),
            privacy: PrivProtocol::Aes256,
            priv_key: "priv-key-material".into(),
        }));
        let rendered = format!("{t:?}");
        assert!(!rendered.contains("auth-key-material"), "{rendered}");
        assert!(!rendered.contains("priv-key-material"), "{rendered}");
        assert!(!rendered.contains("netops"), "{rendered}");
    }

    #[tokio::test]
    async fn a_non_snmp_credential_is_refused_when_a_session_is_opened() {
        let t = UdpTransport::new(Secret::new(CredentialMaterial::ApiToken("t".into())));
        let err = t
            .get_bulk(
                &Target {
                    address: "127.0.0.1:1".parse().unwrap(),
                },
                &"1.3.6.1".parse().unwrap(),
                Repetitions::new(1),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(err, TransportError::Unreachable(ref m) if m.contains("api token")),
            "{err:?}"
        );
    }
}
