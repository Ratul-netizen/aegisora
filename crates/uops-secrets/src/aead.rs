//! The crypto seam — SPEC §M0.4.
//!
//! Target buyers are unrestricted, including any country's government or military
//! (PLAN §0b). Different jurisdictions mandate different validated cryptography, and
//! **validation attaches to a binary, not to an algorithm or a code path**: an
//! AES-256-GCM implementation is not "FIPS compliant" as source, only a specific
//! validated module is. A runtime toggle therefore cannot deliver it.
//!
//! So the primitive is chosen at compile time and one codebase ships as several
//! artifacts. Nothing outside this crate may call an AEAD directly — CI greps for it.

use std::fmt;

use uops_core::Secret;
use zeroize::Zeroize;

use crate::error::{Error, Result};

/// AES-256 key length.
pub const KEY_LEN: usize = 32;
/// AES-GCM nonce length.
pub const NONCE_LEN: usize = 12;

/// A symmetric key. Zeroized on drop; never `Debug`-printed or serialised.
#[derive(Clone)]
pub struct Key([u8; KEY_LEN]);

impl Key {
    #[must_use]
    pub const fn from_bytes(b: [u8; KEY_LEN]) -> Self {
        Self(b)
    }

    /// A fresh key from the OS CSPRNG.
    pub fn generate() -> Result<Secret<Self>> {
        let mut b = [0u8; KEY_LEN];
        getrandom::fill(&mut b).map_err(|e| Error::Random(e.to_string()))?;
        Ok(Secret::new(Self(b)))
    }

    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; KEY_LEN] {
        &self.0
    }
}

impl Zeroize for Key {
    fn zeroize(&mut self) {
        self.0.zeroize();
    }
}

impl Drop for Key {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

impl fmt::Debug for Key {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Key(<redacted>)")
    }
}

/// A single-use nonce. Not secret — stored alongside the ciphertext — but must never
/// repeat under the same key.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Nonce([u8; NONCE_LEN]);

impl Nonce {
    #[must_use]
    pub const fn from_bytes(b: [u8; NONCE_LEN]) -> Self {
        Self(b)
    }

    /// A random nonce.
    ///
    /// Random rather than a counter: a counter needs durable state that survives
    /// restarts and concurrent writers, and getting that wrong silently repeats a
    /// nonce — which under GCM is catastrophic, not merely weak. At 96 bits the
    /// birthday bound is far beyond any plausible credential count.
    pub fn generate() -> Result<Self> {
        let mut b = [0u8; NONCE_LEN];
        getrandom::fill(&mut b).map_err(|e| Error::Random(e.to_string()))?;
        Ok(Self(b))
    }

    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; NONCE_LEN] {
        &self.0
    }
}

/// Authenticated encryption. The only sanctioned path to a crypto primitive.
pub trait AeadProvider: Send + Sync {
    /// Encrypt `plaintext`, authenticating `aad` alongside it.
    fn seal(&self, key: &Key, nonce: &Nonce, aad: &[u8], plaintext: &[u8]) -> Result<Vec<u8>>;

    /// Decrypt, verifying `aad`. Fails if the ciphertext, the nonce, or the AAD has
    /// been altered — which is what binds a sealed row to its identity.
    fn open(
        &self,
        key: &Key,
        nonce: &Nonce,
        aad: &[u8],
        ciphertext: &[u8],
    ) -> Result<Secret<Vec<u8>>>;

    /// Reported in `/api/v1/health`, in the UI, and stamped onto every sealed row, so
    /// an auditor can confirm which build is actually deployed rather than taking it
    /// on trust.
    fn backend_id(&self) -> &'static str;
}

// ---------------------------------------------------------------------------
// RustCrypto backend — the default, portable build. No C toolchain required.
// ---------------------------------------------------------------------------

#[cfg(feature = "crypto-rustcrypto")]
mod rustcrypto {
    use aes_gcm::aead::{Aead, KeyInit, Payload};
    use aes_gcm::{Aes256Gcm, Key as GcmKey, Nonce as GcmNonce};
    use uops_core::Secret;

    use super::{AeadProvider, Key, Nonce};
    use crate::error::{Error, Result};

    /// Pure-Rust AES-256-GCM. **Not FIPS validated** — see the module docs.
    #[derive(Debug, Default, Clone, Copy)]
    pub struct RustCryptoAead;

    impl AeadProvider for RustCryptoAead {
        fn seal(&self, key: &Key, nonce: &Nonce, aad: &[u8], plaintext: &[u8]) -> Result<Vec<u8>> {
            let cipher = Aes256Gcm::new(GcmKey::<Aes256Gcm>::from_slice(key.as_bytes()));
            cipher
                .encrypt(
                    GcmNonce::from_slice(nonce.as_bytes()),
                    Payload {
                        msg: plaintext,
                        aad,
                    },
                )
                .map_err(|_| Error::Seal)
        }

        fn open(
            &self,
            key: &Key,
            nonce: &Nonce,
            aad: &[u8],
            ciphertext: &[u8],
        ) -> Result<Secret<Vec<u8>>> {
            let cipher = Aes256Gcm::new(GcmKey::<Aes256Gcm>::from_slice(key.as_bytes()));
            cipher
                .decrypt(
                    GcmNonce::from_slice(nonce.as_bytes()),
                    Payload {
                        msg: ciphertext,
                        aad,
                    },
                )
                .map(Secret::new)
                // Deliberately opaque: distinguishing "wrong key" from "wrong AAD"
                // from "tampered ciphertext" hands an attacker an oracle.
                .map_err(|_| Error::Open)
        }

        fn backend_id(&self) -> &'static str {
            "rustcrypto-aes256gcm"
        }
    }
}

#[cfg(feature = "crypto-rustcrypto")]
pub use rustcrypto::RustCryptoAead;

// ---------------------------------------------------------------------------
// FIPS backend — seam only.
// ---------------------------------------------------------------------------

#[cfg(all(feature = "crypto-awslc", not(feature = "crypto-rustcrypto")))]
compile_error!(
    "the `crypto-awslc` FIPS backend is declared but not yet implemented. \
     The trait seam exists so that wiring aws-lc-rs behind it is contained work; \
     PLAN 0b is explicit that certification is not pursued speculatively. \
     Implement `AwsLcAead: AeadProvider` in this module before enabling the feature."
);

/// The backend this build was compiled with.
#[cfg(feature = "crypto-rustcrypto")]
#[must_use]
pub fn default_provider() -> impl AeadProvider + Clone {
    RustCryptoAead
}

#[cfg(test)]
mod tests {
    use super::*;

    fn provider() -> impl AeadProvider {
        default_provider()
    }

    #[test]
    fn round_trips() {
        let p = provider();
        let k = Key::generate().unwrap();
        let n = Nonce::generate().unwrap();
        let ct = p.seal(k.expose(), &n, b"aad", b"community-string").unwrap();
        assert_ne!(
            ct.as_slice(),
            b"community-string",
            "plaintext must not survive"
        );
        let pt = p.open(k.expose(), &n, b"aad", &ct).unwrap();
        assert_eq!(pt.expose().as_slice(), b"community-string");
    }

    #[test]
    fn wrong_aad_fails_to_open() {
        // This is the property that makes AAD worth having: it binds a ciphertext to
        // its tenant/credential/version, so a row moved between tenants will not
        // decrypt rather than silently decrypting for the wrong customer.
        let p = provider();
        let k = Key::generate().unwrap();
        let n = Nonce::generate().unwrap();
        let ct = p.seal(k.expose(), &n, b"tenant-a", b"secret").unwrap();
        assert!(p.open(k.expose(), &n, b"tenant-b", &ct).is_err());
    }

    #[test]
    fn wrong_key_fails_to_open() {
        let p = provider();
        let k1 = Key::generate().unwrap();
        let k2 = Key::generate().unwrap();
        let n = Nonce::generate().unwrap();
        let ct = p.seal(k1.expose(), &n, b"aad", b"secret").unwrap();
        assert!(p.open(k2.expose(), &n, b"aad", &ct).is_err());
    }

    #[test]
    fn tampered_ciphertext_fails_to_open() {
        let p = provider();
        let k = Key::generate().unwrap();
        let n = Nonce::generate().unwrap();
        let mut ct = p.seal(k.expose(), &n, b"aad", b"secret").unwrap();
        ct[0] ^= 0x01;
        assert!(p.open(k.expose(), &n, b"aad", &ct).is_err());
    }

    #[test]
    fn keys_and_nonces_do_not_repeat() {
        let a = Key::generate().unwrap();
        let b = Key::generate().unwrap();
        assert_ne!(a.expose().as_bytes(), b.expose().as_bytes());
        assert_ne!(Nonce::generate().unwrap(), Nonce::generate().unwrap());
    }

    #[test]
    fn key_debug_does_not_leak() {
        let k = Key::from_bytes([7u8; KEY_LEN]);
        assert_eq!(format!("{k:?}"), "Key(<redacted>)");
    }

    #[test]
    fn backend_id_is_reported() {
        // Stamped onto every sealed row so a backend change is detectable and an
        // auditor can confirm which build is deployed.
        assert_eq!(provider().backend_id(), "rustcrypto-aes256gcm");
    }
}
