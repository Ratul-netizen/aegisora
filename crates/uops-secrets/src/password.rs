//! Password hashing — SPEC §M0.8.
//!
//! Here rather than in `uops-api` because a CI grep forbids a KDF outside this crate.
//! That guard is the point: the crypto a deployment actually runs should be auditable in
//! one place, not scattered wherever someone needed a hash.
//!
//! **Argon2id, m=19456 KiB, t=2, p=1** — the SPEC minimum. Memory-hard by design,
//! because the cost that matters against an attacker with GPUs is memory rather than
//! iterations.
//!
//! The parameters live *inside* the hash string, so raising them later does not
//! invalidate existing passwords: an old hash keeps verifying under its own parameters
//! and is rewritten at the next successful login, which is the one moment the plaintext
//! is in hand. [`needs_rehash`] is how that is noticed.

use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::{Algorithm, Argon2, Params, Version};
use uops_core::Secret;

use crate::error::{Error, Result};

/// Memory cost in KiB — 19 MiB.
pub const MEMORY_KIB: u32 = 19_456;
/// Passes over memory.
pub const TIME_COST: u32 = 2;
/// Lanes. One: parallelism helps an attacker as much as the defender, and a server
/// authenticating many users at once has no spare thread per login anyway.
pub const PARALLELISM: u32 = 1;

/// A PHC-format hash string. Safe to store; useless without the password.
///
/// Deliberately not [`Secret`]: a password hash is not credential material, and wrapping
/// it would imply this column needs the protection the vault gives an SNMP key. What it
/// needs is to be expensive to attack, which is what the parameters above are for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PasswordHashString(String);

impl PasswordHashString {
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Wrap a hash read back from the database.
    #[must_use]
    pub fn from_stored(s: impl Into<String>) -> Self {
        Self(s.into())
    }
}

fn hasher() -> Result<Argon2<'static>> {
    let params = Params::new(MEMORY_KIB, TIME_COST, PARALLELISM, None)
        .map_err(|e| Error::Storage(format!("argon2 parameters: {e}")))?;
    Ok(Argon2::new(Algorithm::Argon2id, Version::V0x13, params))
}

/// Hash a password for storage.
pub fn hash(password: &Secret<String>) -> Result<PasswordHashString> {
    let mut salt = [0u8; 16];
    getrandom::fill(&mut salt).map_err(|e| Error::Random(e.to_string()))?;
    let salt = SaltString::encode_b64(&salt).map_err(|e| Error::Storage(e.to_string()))?;

    let hashed = hasher()?
        .hash_password(password.expose().as_bytes(), &salt)
        .map_err(|e| Error::Storage(format!("hashing: {e}")))?;

    Ok(PasswordHashString(hashed.to_string()))
}

/// Whether `password` produced `stored`.
///
/// `false` for a wrong password and for a malformed stored hash alike. A corrupted row
/// must not be distinguishable from a wrong password by an unauthenticated caller —
/// that difference is an oracle.
#[must_use]
pub fn verify(password: &Secret<String>, stored: &PasswordHashString) -> bool {
    let Ok(parsed) = PasswordHash::new(&stored.0) else {
        return false;
    };
    let Ok(argon) = hasher() else {
        return false;
    };
    argon
        .verify_password(password.expose().as_bytes(), &parsed)
        .is_ok()
}

/// Whether a stored hash was produced with weaker parameters than current policy.
///
/// Checked after a *successful* login, which is the only moment a stronger hash can be
/// written without asking the user for anything.
#[must_use]
pub fn needs_rehash(stored: &PasswordHashString) -> bool {
    let Ok(parsed) = PasswordHash::new(&stored.0) else {
        return true; // unparseable is not worth keeping
    };
    let Ok(params) = Params::try_from(&parsed) else {
        return true;
    };

    parsed.algorithm != Algorithm::Argon2id.ident()
        || params.m_cost() < MEMORY_KIB
        || params.t_cost() < TIME_COST
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secret(s: &str) -> Secret<String> {
        Secret::new(s.to_owned())
    }

    #[test]
    fn a_password_verifies_against_its_own_hash_and_nothing_else() {
        let hashed = hash(&secret("correct horse battery staple")).unwrap();
        assert!(verify(&secret("correct horse battery staple"), &hashed));
        assert!(!verify(&secret("correct horse battery stapl"), &hashed));
        assert!(!verify(&secret(""), &hashed));
    }

    #[test]
    fn the_same_password_hashes_differently_every_time() {
        // A fresh salt per hash. Without one, identical passwords are visibly identical
        // in the database, and cracking a hash once cracks every account sharing it.
        let a = hash(&secret("same")).unwrap();
        let b = hash(&secret("same")).unwrap();
        assert_ne!(a, b);
        assert!(verify(&secret("same"), &a) && verify(&secret("same"), &b));
    }

    #[test]
    fn the_stored_parameters_are_the_spec_minimum() {
        // SPEC §M0.8: argon2id, m=19456, t=2, p=1 minimum. They live in the hash string,
        // which is what lets them be raised later without a password reset.
        let hashed = hash(&secret("x")).unwrap();
        let text = hashed.as_str();
        assert!(text.starts_with("$argon2id$"), "{text}");
        assert!(text.contains("m=19456"), "{text}");
        assert!(text.contains("t=2"), "{text}");
        assert!(text.contains("p=1"), "{text}");
    }

    #[test]
    fn a_corrupted_hash_is_a_failed_login_not_a_different_answer() {
        // "Wrong password" and "this row is damaged" must look identical to an
        // unauthenticated caller, or the difference is an oracle.
        let broken = PasswordHashString::from_stored("not a PHC string");
        assert!(!verify(&secret("anything"), &broken));
    }

    #[test]
    fn a_weaker_stored_hash_is_flagged_for_rehashing() {
        let current = hash(&secret("x")).unwrap();
        assert!(!needs_rehash(&current));
        assert!(needs_rehash(&PasswordHashString::from_stored("garbage")));
    }

    #[test]
    fn hashing_is_slow_enough_to_be_worth_attacking_slowly() {
        // Not a benchmark — a smoke test that the parameters are actually applied.
        // Argon2id over 19 MiB cannot finish in microseconds, and if it does, the cost
        // was silently dropped somewhere between here and the library.
        let started = std::time::Instant::now();
        let _ = hash(&secret("timing")).unwrap();
        assert!(
            started.elapsed() >= std::time::Duration::from_millis(5),
            "hashing finished in {:?} — the cost parameters are not being applied",
            started.elapsed()
        );
    }
}
