//! Opening credentials, and the transports built from them.
//!
//! # One transport per credential, not per device
//!
//! [`uops_snmp::UdpTransport`] holds one credential and a pool of sessions keyed by
//! address, which is the shape the scheduling above it already has: a fleet is a handful
//! of credentials and thousands of devices, not the other way round. Building a
//! transport per device would mean re-deriving v3's localised keys — an HMAC over the
//! passphrase and the engine ID — once per device instead of once per credential, and
//! holding the material in as many places.
//!
//! # What happens to the plaintext
//!
//! The `Secret<CredentialMaterial>` the vault returns is moved straight into the
//! transport, which holds the wrapper rather than the material — so there is exactly one
//! live copy per credential, it is never owned outside a `Secret`, and it is zeroized
//! when the last transport holding it is dropped.
//!
//! The cache is also what keeps the access log readable. Opening once per credential per
//! process means the vault records a read when a credential is first used, not one per
//! device per poll; a log written at poll rate is a log nobody can find an anomaly in.
//!
//! # A failure here is per credential, not per device
//!
//! A credential that cannot be opened — revoked, or wrapped by a KEK this process does
//! not have — fails every device using it, and would otherwise produce one identical
//! error line per device per poll forever. The failure is remembered so it is reported
//! once and retried on reload, when the operator may have fixed it.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use uops_core::{CredentialRef, ResourceId, TenantId};
use uops_secrets::{KekRing, LocalVault, MemoryAccessLog, RustCryptoAead};
use uops_snmp::{Transport, UdpTransport};
use uops_store_pg::{PgSealedStore, PgStore};

use crate::config::{Config, KekSource};

/// The vault this binary uses: `RustCrypto` over `PostgreSQL`, logging to memory.
///
/// The access log is in-memory because there is nowhere durable to put it yet — the
/// `credential_access` table is M3. That is a real gap and it is named here rather than
/// hidden behind a type alias that reads as if it were resolved.
pub type Vault = LocalVault<RustCryptoAead, PgSealedStore, MemoryAccessLog>;

/// Build the vault from the configured key ring.
///
/// # Errors
///
/// When the KEK cannot be read, is not 64 hex characters, or — on Unix — is in a file
/// other local accounts can read.
pub fn vault(store: PgStore, config: &Config) -> Result<Vault, uops_secrets::Error> {
    let ring = match &config.kek {
        KekSource::File(path) => KekRing::from_file(path, config.kek_id.clone())?,
        KekSource::Env(name) => KekRing::from_env(name, config.kek_id.clone())?,
    };
    Ok(LocalVault::new(
        RustCryptoAead,
        PgSealedStore::new(store),
        MemoryAccessLog::new(),
        ring,
    ))
}

/// Why a device cannot be polled.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CredentialProblem {
    /// The `resource.credential_ref` column is null. The device was created without
    /// one, which is a configuration gap rather than a failure.
    NotAssigned,
    /// The vault refused it. The string is the vault's own message, which says whether
    /// it was missing, revoked or wrapped by an unknown key.
    Unopenable(String),
}

impl std::fmt::Display for CredentialProblem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotAssigned => f.write_str("no credential is assigned"),
            Self::Unopenable(why) => write!(f, "the credential could not be opened: {why}"),
        }
    }
}

/// Where a device's transport comes from.
///
/// One implementation in this crate — [`Transports`], which opens a credential and
/// builds an SNMP session from it. The trait exists so the loop can be run against
/// something else, and the something else that matters is the simulator: SPEC §M2's
/// first acceptance criterion is *1 000 simulated agents*, and measuring it through the
/// binary means the binary has to be able to talk to simulated ones.
///
/// `&self`, not `&mut self`: the cache is behind the implementation's own lock, so the
/// loop can hold one of these in an `Arc` and every task can reach it at once.
pub trait TransportSource: Send + Sync {
    /// The transport for a device.
    ///
    /// # Errors
    ///
    /// When the device has no credential, or the credential cannot be opened.
    fn for_device(
        &self,
        tenant: TenantId,
        resource: ResourceId,
        credential: Option<CredentialRef>,
        timeout: std::time::Duration,
    ) -> Result<Arc<dyn Transport>, CredentialProblem>;

    /// Forget which credentials failed, so the next poll tries them again. Returns how
    /// many were forgotten.
    fn retry_failures(&self) -> usize;
}

/// Transports, one per credential, opened on demand.
///
/// The caches are behind a `Mutex` rather than an `RwLock`: opening a credential is rare
/// — once per credential per process — and the read path is a hash lookup, so the
/// contention an `RwLock` would save does not exist.
pub struct Transports {
    vault: Vault,
    open: Mutex<HashMap<(TenantId, CredentialRef), Arc<UdpTransport>>>,
    /// Credentials that failed, so the error is reported once rather than per device
    /// per poll. Cleared by [`TransportSource::retry_failures`].
    failed: Mutex<HashMap<(TenantId, CredentialRef), CredentialProblem>>,
}

impl std::fmt::Debug for Transports {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Counts only. Everything inside is either a credential or derived from one.
        f.debug_struct("Transports")
            .field(
                "open",
                &self.open.lock().map(|m| m.len()).unwrap_or_default(),
            )
            .field(
                "failed",
                &self.failed.lock().map(|m| m.len()).unwrap_or_default(),
            )
            .finish_non_exhaustive()
    }
}

impl Transports {
    #[must_use]
    pub fn new(vault: Vault) -> Self {
        Self {
            vault,
            open: Mutex::new(HashMap::new()),
            failed: Mutex::new(HashMap::new()),
        }
    }

    /// A poisoned lock means a panic while holding it, which cannot happen here — the
    /// critical sections are hash lookups and inserts. Recovering the guard beats
    /// propagating a panic into every device's poll.
    fn open(
        &self,
    ) -> std::sync::MutexGuard<'_, HashMap<(TenantId, CredentialRef), Arc<UdpTransport>>> {
        self.open
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn failed(
        &self,
    ) -> std::sync::MutexGuard<'_, HashMap<(TenantId, CredentialRef), CredentialProblem>> {
        self.failed
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// How many credentials are open.
    #[must_use]
    pub fn open_count(&self) -> usize {
        self.open().len()
    }
}

impl TransportSource for Transports {
    /// Forget which credentials failed, so the next poll tries them again.
    ///
    /// Called on reload rather than on a timer: reload is when an operator's fix — a
    /// re-assigned credential, a restored KEK — would have landed.
    fn retry_failures(&self) -> usize {
        let mut failed = self.failed();
        let n = failed.len();
        failed.clear();
        n
    }

    /// The transport for a device, opening its credential the first time.
    fn for_device(
        &self,
        tenant: TenantId,
        resource: ResourceId,
        credential: Option<CredentialRef>,
        timeout: std::time::Duration,
    ) -> Result<Arc<dyn Transport>, CredentialProblem> {
        let Some(credential) = credential else {
            return Err(CredentialProblem::NotAssigned);
        };
        let key = (tenant, credential);

        if let Some(transport) = self.open().get(&key) {
            return Ok(Arc::clone(transport) as Arc<dyn Transport>);
        }
        if let Some(problem) = self.failed().get(&key) {
            return Err(problem.clone());
        }

        // poll_context names the resource this read is *for*, which is what makes the
        // access log answer "who was this credential used against" rather than only
        // "how often".
        let ctx = uops_snmp::credential::poll_context(resource);
        let opened = self.vault.get(tenant, credential, &ctx).map_err(|e| {
            let problem = CredentialProblem::Unopenable(e.to_string());
            self.failed().insert(key, problem.clone());
            problem
        })?;

        // The Secret itself, not the material: UdpTransport takes the wrapper, so the
        // plaintext is never owned outside one and is zeroized when the last transport
        // holding it is dropped.
        let transport = Arc::new(UdpTransport::new(opened).with_timeout(timeout));
        self.open().insert(key, Arc::clone(&transport));
        Ok(transport as Arc<dyn Transport>)
    }
}
