//! `Secret<T>` — a value that cannot be logged, printed, or serialised.
//!
//! This platform holds `SNMPv3` auth/priv keys, SSH private keys and API tokens for a
//! customer's entire network. A leak is a full network compromise, not a data breach.
//! SPEC §M0.4 therefore requires that credential material be structurally hard to
//! expose, not merely discouraged.
//!
//! The type deliberately does **not** implement:
//!
//! | trait | why not |
//! |---|---|
//! | `Display` | `format!("{}", secret)` would leak it |
//! | `Serialize` | it could reach an API response or a JSON log line |
//! | `Clone` | copies are additional things to zeroize; use [`Secret::expose`] deliberately |
//!
//! It *does* implement `Debug`, printing `Secret(<redacted>)`, because a struct
//! containing a secret still needs to be `Debug` — and a derived `Debug` on the parent
//! would otherwise be impossible without hand-writing it everywhere.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::{Error, Result};
use zeroize::Zeroize;

/// Wraps credential material so it cannot accidentally escape.
///
/// ```
/// # use uops_core::Secret;
/// let s = Secret::new(String::from("hunter2"));
/// assert_eq!(format!("{s:?}"), "Secret(<redacted>)");
/// assert_eq!(s.expose(), "hunter2"); // deliberate, greppable, reviewable
/// ```
///
/// # Enforced negatives
///
/// These are the invariants that make the type worth having, so they are asserted as
/// `compile_fail` doctests. They live on this public item deliberately: rustdoc does
/// **not** collect doctests from private items inside `#[cfg(test)]` modules, so the
/// same examples written next to the unit tests would silently never run.
///
/// Not `Display` — `format!("{}", secret)` must not compile:
///
/// ```compile_fail
/// # use uops_core::Secret;
/// let s = Secret::new(String::from("x"));
/// let _ = format!("{}", s);
/// ```
///
/// Not `Serialize` — it must not be able to reach an API response or a JSON log:
///
/// ```compile_fail
/// # use uops_core::Secret;
/// fn assert_serialize<T: serde::Serialize>() {}
/// assert_serialize::<Secret<String>>();
/// ```
///
/// Not `Clone` — every copy is another thing to zeroize:
///
/// ```compile_fail
/// # use uops_core::Secret;
/// let s = Secret::new(String::from("x"));
/// let _ = s.clone();
/// ```
pub struct Secret<T: Zeroize>(T);

impl<T: Zeroize> Secret<T> {
    pub const fn new(value: T) -> Self {
        Self(value)
    }

    /// Read the protected value.
    ///
    /// Named `expose` rather than `get` or `as_ref` so that every use is obvious in a
    /// diff and greppable in review. CI greps for `expose()` appearing in the same
    /// statement as a logging or formatting macro.
    pub const fn expose(&self) -> &T {
        &self.0
    }

    /// Consume the wrapper and take the value.
    ///
    /// The returned value is no longer zeroized on drop — the caller becomes
    /// responsible for it. Prefer [`Secret::expose`] unless ownership is genuinely
    /// required.
    pub fn into_inner(mut self) -> T
    where
        T: Default,
    {
        // Swap the value out, then suppress our own Drop so the (now empty) husk is
        // not zeroized twice and the returned value is not wiped from under the caller.
        let out = std::mem::take(&mut self.0);
        std::mem::forget(self);
        out
    }
}

impl<T: Zeroize> fmt::Debug for Secret<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(<redacted>)")
    }
}

impl<T: Zeroize> Drop for Secret<T> {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

impl<T: Zeroize> From<T> for Secret<T> {
    fn from(value: T) -> Self {
        Self::new(value)
    }
}

/// An `SNMPv3` USM authentication protocol.
///
/// The weak ones are here because refusing them means refusing devices that exist. A
/// switch bought in 2012 and still in a rack offers `MD5` and nothing else, and telling
/// its owner to replace it is not monitoring advice. [`AuthProtocol::is_weak`] is how
/// the UI marks them, the same way an SNMP community string is marked — visible, not
/// forbidden.
///
/// SPEC §M2's acceptance criterion is `SHA-256`, which is [`AuthProtocol::Sha256`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthProtocol {
    /// RFC 3414. Broken for collision resistance; still the only option on old kit.
    Md5,
    /// RFC 3414. Weak, and near-universal.
    Sha1,
    /// RFC 7860.
    Sha224,
    /// RFC 7860. What SPEC asks for.
    Sha256,
    /// RFC 7860.
    Sha384,
    /// RFC 7860.
    Sha512,
}

impl AuthProtocol {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Md5 => "md5",
            Self::Sha1 => "sha1",
            Self::Sha224 => "sha224",
            Self::Sha256 => "sha256",
            Self::Sha384 => "sha384",
            Self::Sha512 => "sha512",
        }
    }

    /// True for protocols that should be reported rather than relied on.
    #[must_use]
    pub const fn is_weak(self) -> bool {
        matches!(self, Self::Md5 | Self::Sha1)
    }
}

impl std::str::FromStr for AuthProtocol {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self> {
        Ok(match s {
            "md5" => Self::Md5,
            "sha1" => Self::Sha1,
            "sha224" => Self::Sha224,
            "sha256" => Self::Sha256,
            "sha384" => Self::Sha384,
            "sha512" => Self::Sha512,
            other => {
                return Err(Error::Invalid(format!(
                    "`{other}` is not an SNMPv3 auth protocol"
                )));
            }
        })
    }
}

/// An `SNMPv3` USM privacy protocol.
///
/// `NoPriv` is absent on purpose. A credential that authenticates but does not encrypt
/// puts every polled value on the wire in cleartext, and "authPriv or v2c" is a clearer
/// thing to explain to a customer than three security levels with different meanings.
/// A device that cannot encrypt uses a community string and is marked insecure, which
/// is at least honest about what it is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PrivProtocol {
    /// RFC 3414. 56-bit effective key. Present for the same reason `MD5` is.
    Des,
    /// RFC 3826.
    Aes128,
    /// Non-standard extension, widely implemented.
    Aes192,
    /// Non-standard extension, widely implemented. What SPEC asks for.
    Aes256,
}

impl PrivProtocol {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Des => "des",
            Self::Aes128 => "aes128",
            Self::Aes192 => "aes192",
            Self::Aes256 => "aes256",
        }
    }

    #[must_use]
    pub const fn is_weak(self) -> bool {
        matches!(self, Self::Des)
    }
}

impl std::str::FromStr for PrivProtocol {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self> {
        Ok(match s {
            "des" => Self::Des,
            "aes128" => Self::Aes128,
            "aes192" => Self::Aes192,
            "aes256" => Self::Aes256,
            other => {
                return Err(Error::Invalid(format!(
                    "`{other}` is not an SNMPv3 privacy protocol"
                )));
            }
        })
    }
}

/// Credential material, by kind.
///
/// Every variant's payload is zeroized on drop. This is the only thing a
/// `SecretStore` implementation hands back.
///
/// `Zeroize` is implemented by hand rather than derived: the derive's enum support has
/// shifted across zeroize releases, and this is the one type in the system where a
/// silent failure to wipe memory actually matters.
pub enum CredentialMaterial {
    /// SNMP v1/v2c community string. Marked insecure in the UI — it is cleartext
    /// on the wire and cannot be made otherwise.
    SnmpCommunity(String),
    /// `SNMPv3` USM credentials.
    ///
    /// The protocols travel with the keys because that is where they live: a USM user
    /// on a device is the tuple (name, auth protocol, auth key, privacy protocol,
    /// privacy key), and a credential carrying keys without saying which algorithms
    /// they are for is a credential the poller has to guess about.
    SnmpV3 {
        username: String,
        auth: AuthProtocol,
        auth_key: String,
        privacy: PrivProtocol,
        priv_key: String,
    },
    SshPassword {
        username: String,
        password: String,
    },
    SshKey {
        username: String,
        private_key: String,
        passphrase: String,
    },
    ApiToken(String),
}

impl Zeroize for CredentialMaterial {
    fn zeroize(&mut self) {
        match self {
            Self::SnmpCommunity(s) | Self::ApiToken(s) => s.zeroize(),
            Self::SnmpV3 {
                username,
                auth_key,
                priv_key,
                // The protocol names are not secret and are Copy; there is nothing to
                // wipe. Bound explicitly rather than with `..` so that adding a field
                // that *is* secret fails to compile here.
                auth: _,
                privacy: _,
            } => {
                username.zeroize();
                auth_key.zeroize();
                priv_key.zeroize();
            }
            Self::SshPassword { username, password } => {
                username.zeroize();
                password.zeroize();
            }
            Self::SshKey {
                username,
                private_key,
                passphrase,
            } => {
                username.zeroize();
                private_key.zeroize();
                passphrase.zeroize();
            }
        }
    }
}

impl Drop for CredentialMaterial {
    fn drop(&mut self) {
        self.zeroize();
    }
}

impl fmt::Debug for CredentialMaterial {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Print the variant so logs remain useful, never the payload.
        let kind = match self {
            Self::SnmpCommunity(_) => "SnmpCommunity",
            Self::SnmpV3 { .. } => "SnmpV3",
            Self::SshPassword { .. } => "SshPassword",
            Self::SshKey { .. } => "SshKey",
            Self::ApiToken(_) => "ApiToken",
        };
        write!(f, "CredentialMaterial::{kind}(<redacted>)")
    }
}

impl CredentialMaterial {
    /// Stable discriminant for the `credential.kind` column.
    #[must_use]
    pub const fn kind(&self) -> &'static str {
        match self {
            Self::SnmpCommunity(_) => "snmp_community",
            Self::SnmpV3 { .. } => "snmpv3",
            Self::SshPassword { .. } => "ssh_password",
            Self::SshKey { .. } => "ssh_key",
            Self::ApiToken(_) => "api_token",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_never_reveals_the_value() {
        let s = Secret::new(String::from("super-secret-community"));
        let rendered = format!("{s:?}");
        assert_eq!(rendered, "Secret(<redacted>)");
        assert!(!rendered.contains("super-secret"));
    }

    #[test]
    fn a_protocol_name_round_trips_and_an_unknown_one_is_refused() {
        // The names go on the wire in a sealed credential, so a rename here would make
        // every credential already sealed with the old one undecryptable.
        for p in [
            AuthProtocol::Md5,
            AuthProtocol::Sha1,
            AuthProtocol::Sha224,
            AuthProtocol::Sha256,
            AuthProtocol::Sha384,
            AuthProtocol::Sha512,
        ] {
            assert_eq!(p.as_str().parse::<AuthProtocol>().unwrap(), p);
        }
        for p in [
            PrivProtocol::Des,
            PrivProtocol::Aes128,
            PrivProtocol::Aes192,
            PrivProtocol::Aes256,
        ] {
            assert_eq!(p.as_str().parse::<PrivProtocol>().unwrap(), p);
        }
        assert!("sha3".parse::<AuthProtocol>().is_err());
        assert!("rc4".parse::<PrivProtocol>().is_err());
        // Not case-insensitive, deliberately: these are wire names, not user input.
        assert!("SHA256".parse::<AuthProtocol>().is_err());
    }

    #[test]
    fn the_weak_protocols_are_the_ones_that_are_actually_weak() {
        // Not an opinion that should drift. MD5 and SHA-1 are broken for the property
        // USM relies on; DES has a 56-bit effective key. AES-128 is not weak — it is
        // simply not what SPEC asks for, and conflating the two would make the UI warn
        // about a fine credential.
        assert!(AuthProtocol::Md5.is_weak());
        assert!(AuthProtocol::Sha1.is_weak());
        assert!(!AuthProtocol::Sha256.is_weak());
        assert!(PrivProtocol::Des.is_weak());
        assert!(!PrivProtocol::Aes128.is_weak());
        assert!(!PrivProtocol::Aes256.is_weak());
    }

    #[test]
    fn credential_debug_shows_kind_but_not_payload() {
        let c = CredentialMaterial::SnmpV3 {
            username: "admin".into(),
            auth: AuthProtocol::Sha256,
            auth_key: "auth-key-material".into(),
            privacy: PrivProtocol::Aes256,
            priv_key: "priv-key-material".into(),
        };
        let rendered = format!("{c:?}");
        assert!(
            rendered.contains("SnmpV3"),
            "variant should be visible: {rendered}"
        );
        assert!(
            !rendered.contains("auth-key-material"),
            "leaked: {rendered}"
        );
        assert!(!rendered.contains("admin"), "leaked username: {rendered}");
    }

    #[test]
    fn expose_returns_the_value() {
        let s = Secret::new(String::from("hunter2"));
        assert_eq!(s.expose(), "hunter2");
    }

    #[test]
    fn into_inner_hands_over_ownership() {
        let s = Secret::new(String::from("hunter2"));
        assert_eq!(s.into_inner(), "hunter2");
    }

    // The negative invariants (not Display, not Serialize, not Clone) are asserted as
    // `compile_fail` doctests on `Secret` itself. They cannot live here: rustdoc does
    // not collect doctests from private items in `#[cfg(test)]` modules, so they would
    // pass vacuously without ever being compiled.
}
