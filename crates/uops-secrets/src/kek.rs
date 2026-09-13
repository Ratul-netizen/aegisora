//! Key encryption keys — where the root of trust comes from.
//!
//! The KEK must live **outside the database it protects**, or envelope encryption buys
//! nothing: an attacker with a database dump would have both the wrapped DEKs and the
//! key that unwraps them.
//!
//! Sources, in the order an `onprem` deployment should prefer them (PLAN §0b):
//!
//! | source | when |
//! |---|---|
//! | file, mode 0600 | default for on-prem; simple, backup-able, auditable |
//! | environment | containers and orchestrators that inject secrets |
//! | generated | tests only — never persisted, so data is unreadable after restart |
//! | KMS / Vault | `hosted` profile, and on-prem sites that run Vault. Later |

use std::collections::HashMap;
use std::path::Path;

use uops_core::Secret;

use crate::aead::{KEY_LEN, Key};
use crate::error::{Error, Result};
use crate::record::KeyId;

/// Holds the KEKs this process can unwrap DEKs with.
///
/// More than one because rotation is not atomic: after a new KEK is introduced, rows
/// wrapped by the previous one must still open until re-wrapping finishes. Dropping
/// the old key too early makes every un-rotated credential permanently unreadable.
pub struct KekRing {
    keys: HashMap<KeyId, Secret<Key>>,
    active: KeyId,
}

impl std::fmt::Debug for KekRing {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KekRing")
            .field("active", &self.active)
            .field("key_count", &self.keys.len())
            .finish()
    }
}

impl KekRing {
    /// Build a ring with a single active key.
    #[must_use]
    pub fn new(active: KeyId, key: Secret<Key>) -> Self {
        let mut keys = HashMap::new();
        keys.insert(active.clone(), key);
        Self { keys, active }
    }

    /// Add a retired key so existing rows keep opening during rotation.
    pub fn add_retired(&mut self, id: KeyId, key: Secret<Key>) {
        self.keys.insert(id, key);
    }

    /// Promote a new key to active, retaining the previous one for unwrapping.
    pub fn promote(&mut self, id: KeyId, key: Secret<Key>) {
        self.keys.insert(id.clone(), key);
        self.active = id;
    }

    #[must_use]
    pub const fn active_id(&self) -> &KeyId {
        &self.active
    }

    pub fn active(&self) -> Result<&Key> {
        self.get(&self.active)
    }

    pub fn get(&self, id: &KeyId) -> Result<&Key> {
        self.keys
            .get(id)
            .map(Secret::expose)
            .ok_or_else(|| Error::UnknownKek(id.as_str().to_owned()))
    }

    /// Load a KEK from an environment variable holding 64 hex characters.
    pub fn from_env(var: &str, id: KeyId) -> Result<Self> {
        let hex = std::env::var(var).map_err(|_| Error::KekUnavailable(var.to_owned()))?;
        Ok(Self::new(id, Secret::new(parse_hex_key(hex.trim())?)))
    }

    /// Load a KEK from a file containing 64 hex characters.
    ///
    /// On Unix the file's permissions are checked and a group- or world-readable key is
    /// **rejected**, not warned about. A KEK readable by other local accounts is not a
    /// root of trust, and a warning in a log nobody reads is not a control.
    pub fn from_file(path: &Path, id: KeyId) -> Result<Self> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(path)
                .map_err(|e| Error::KekUnavailable(format!("{}: {e}", path.display())))?
                .permissions()
                .mode();
            if mode & 0o077 != 0 {
                return Err(Error::KekPermissions {
                    path: path.display().to_string(),
                    mode: mode & 0o777,
                });
            }
        }
        let content = std::fs::read_to_string(path)
            .map_err(|e| Error::KekUnavailable(format!("{}: {e}", path.display())))?;
        Ok(Self::new(id, Secret::new(parse_hex_key(content.trim())?)))
    }

    /// An ephemeral ring for tests.
    ///
    /// `#[cfg(test)]`-gated in spirit but available to integration tests; it is
    /// deliberately named so that finding it in production code is obvious in review.
    pub fn ephemeral_for_tests() -> Result<Self> {
        Ok(Self::new(KeyId::new("test-ephemeral"), Key::generate()?))
    }
}

fn parse_hex_key(s: &str) -> Result<Key> {
    if s.len() != KEY_LEN * 2 {
        return Err(Error::KekMalformed(format!(
            "expected {} hex characters, got {}",
            KEY_LEN * 2,
            s.len()
        )));
    }
    let mut out = [0u8; KEY_LEN];
    for (i, chunk) in s.as_bytes().as_chunks::<2>().0.iter().enumerate() {
        let hi = hex_val(chunk[0])?;
        let lo = hex_val(chunk[1])?;
        out[i] = (hi << 4) | lo;
    }
    Ok(Key::from_bytes(out))
}

const fn hex_val(c: u8) -> Result<u8> {
    match c {
        b'0'..=b'9' => Ok(c - b'0'),
        b'a'..=b'f' => Ok(c - b'a' + 10),
        b'A'..=b'F' => Ok(c - b'A' + 10),
        _ => Err(Error::KekMalformed(String::new())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_valid_hex_key() {
        let k = parse_hex_key(&"ab".repeat(KEY_LEN)).unwrap();
        assert_eq!(k.as_bytes(), &[0xab; KEY_LEN]);
    }

    #[test]
    fn rejects_wrong_length_and_bad_characters() {
        assert!(parse_hex_key("abcd").is_err());
        assert!(parse_hex_key(&"zz".repeat(KEY_LEN)).is_err());
    }

    #[test]
    fn ring_keeps_retired_keys_openable_during_rotation() {
        // Dropping the old KEK at promotion time would make every not-yet-rewrapped
        // credential permanently unreadable. Rotation is not atomic, so the ring must
        // hold both.
        let old_id = KeyId::new("kek-1");
        let mut ring = KekRing::new(old_id.clone(), Key::generate().unwrap());
        assert_eq!(ring.active_id(), &old_id);

        let new_id = KeyId::new("kek-2");
        ring.promote(new_id.clone(), Key::generate().unwrap());

        assert_eq!(ring.active_id(), &new_id);
        assert!(ring.get(&old_id).is_ok(), "retired KEK must still unwrap");
        assert!(ring.active().is_ok());
    }

    #[test]
    fn unknown_key_is_an_error_not_a_panic() {
        let ring = KekRing::ephemeral_for_tests().unwrap();
        assert!(ring.get(&KeyId::new("never-existed")).is_err());
    }

    #[test]
    fn debug_shows_shape_but_no_key_material() {
        let ring = KekRing::ephemeral_for_tests().unwrap();
        let s = format!("{ring:?}");
        assert!(s.contains("key_count"));
        assert!(!s.contains("Key("), "must not render inner keys: {s}");
    }
}
