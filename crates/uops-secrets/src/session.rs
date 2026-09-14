//! Session tokens — SPEC §M0.8.
//!
//! Opaque and random, not a JWT. A signed token that carries its own claims cannot be
//! revoked before it expires, and "sign out everywhere" and "disable this account
//! immediately" are both things an operator expects to work *now*. A server-side store
//! makes revocation a row update; a JWT makes it a deny-list, which is a session store
//! with extra steps and worse failure modes.
//!
//! # What is stored
//!
//! The **hash** of the token, never the token. A stolen database backup must not hand
//! the thief a set of live sessions. A plain SHA-256 is the right primitive here and
//! Argon2 would be wrong: the token is 256 bits of CSPRNG output, so there is no
//! low-entropy secret to make expensive to guess — only a fast one-way mapping needed,
//! on the hot path of every request.

use sha2::{Digest, Sha256};
use uops_core::Secret;

use crate::error::{Error, Result};

/// Bytes of entropy in a token. 256 bits: not guessable, and not worth debating.
const TOKEN_BYTES: usize = 32;

/// The value handed to the client. Shown once, then only its hash exists.
///
/// Wrapped in [`Secret`] so it cannot be logged or serialised by accident — it is a
/// bearer credential for as long as it lives.
#[derive(Debug)]
pub struct SessionToken(Secret<String>);

impl SessionToken {
    /// The string to put in the cookie. Deliberately awkward to reach.
    #[must_use]
    pub fn expose(&self) -> &str {
        self.0.expose()
    }
}

/// What goes in the database.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionTokenHash([u8; 32]);

impl SessionTokenHash {
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// Read a hash back from storage.
    #[must_use]
    pub fn from_stored(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// Try to read a hash of unknown length from storage.
    pub fn from_slice(bytes: &[u8]) -> Result<Self> {
        let array: [u8; 32] = bytes
            .try_into()
            .map_err(|_| Error::Corrupt("session token hash is not 32 bytes"))?;
        Ok(Self(array))
    }
}

/// Mint a new session token and the hash to store beside it.
///
/// Returns both because the token exists exactly once, at this moment: it goes into the
/// response and is never recoverable afterwards, including by whoever runs the database.
pub fn issue() -> Result<(SessionToken, SessionTokenHash)> {
    let mut bytes = [0u8; TOKEN_BYTES];
    getrandom::fill(&mut bytes).map_err(|e| Error::Random(e.to_string()))?;

    let mut token = String::with_capacity(TOKEN_BYTES * 2);
    for b in bytes {
        use std::fmt::Write as _;
        let _ = write!(token, "{b:02x}");
    }

    let hash = hash_of(&token);
    Ok((SessionToken(Secret::new(token)), hash))
}

/// The hash of a token presented by a client.
///
/// Lookup is then a unique-index probe on the hash, which means the comparison happens
/// in the database rather than in Rust — so there is no byte-by-byte comparison here to
/// get the timing wrong.
#[must_use]
pub fn hash_of(token: &str) -> SessionTokenHash {
    let digest = Sha256::digest(token.as_bytes());
    let mut out = [0u8; 32];
    out.copy_from_slice(&digest);
    SessionTokenHash(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn a_token_is_long_random_hex() {
        let (token, _) = issue().unwrap();
        assert_eq!(token.expose().len(), TOKEN_BYTES * 2);
        assert!(token.expose().chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn tokens_do_not_repeat() {
        // Not a statistical test — a smoke test that the CSPRNG is actually being read
        // rather than something deterministic having crept in.
        let mut seen = HashSet::new();
        for _ in 0..1_000 {
            let (token, _) = issue().unwrap();
            assert!(seen.insert(token.expose().to_owned()), "a token repeated");
        }
    }

    #[test]
    fn the_hash_matches_what_the_client_presents() {
        let (token, stored) = issue().unwrap();
        assert_eq!(hash_of(token.expose()), stored);
        assert_ne!(hash_of("something else"), stored);
    }

    #[test]
    fn the_stored_hash_does_not_reveal_the_token() {
        // What a database backup gives an attacker: 32 bytes that are not the token.
        use std::fmt::Write as _;
        let (token, stored) = issue().unwrap();
        let mut as_hex = String::new();
        for b in stored.as_bytes() {
            let _ = write!(as_hex, "{b:02x}");
        }
        assert_ne!(as_hex, token.expose());
    }

    #[test]
    fn a_hash_of_the_wrong_length_is_refused_rather_than_padded() {
        // A truncated column would otherwise silently become a hash that matches
        // nothing, and every session would fail authentication for no visible reason.
        assert!(SessionTokenHash::from_slice(&[0u8; 16]).is_err());
        assert!(SessionTokenHash::from_slice(&[0u8; 32]).is_ok());
    }

    #[test]
    fn a_token_cannot_be_printed_by_accident() {
        // It is a bearer credential. Secret<T> is not Display or Serialize, so reaching
        // it requires the deliberate `.expose()` that the CI grep looks for.
        let (token, _) = issue().unwrap();
        let debug = format!("{token:?}");
        assert!(!debug.contains(token.expose()), "{debug}");
    }
}
