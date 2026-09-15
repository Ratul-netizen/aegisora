//! Getting an `SNMPv3` credential, and being honest about what it buys.
//!
//! SPEC §M2 acceptance: *"`SNMPv3` authPriv (SHA-256 / AES-256) against a real device;
//! credential fetched through `SecretStore` with an access-log entry."*
//!
//! The access-log entry is not written here. [`uops_secrets::LocalVault::get`] writes one
//! on every call including failures, which is the right place for it — a poller that had
//! to remember to log would eventually be a poller that forgot, and a burst of *failed*
//! credential reads is exactly the signal an auditor wants. What this module adds is the
//! [`AccessContext`] that makes the entry mean something: who asked, and what for.
//!
//! # Weak protocols are reported, not refused
//!
//! A switch bought in 2012 and still in a rack offers `MD5` and `DES` and nothing else.
//! Refusing to poll it is not a security posture, it is an unmonitored switch — and an
//! unmonitored switch is a worse outcome than a monitored one whose SNMP transport is
//! old. So [`Strength`] classifies a credential and the caller decides; SPEC's
//! SHA-256/AES-256 is [`Strength::Strong`], and everything else is visible.
//!
//! This is the same position the product already takes on v1/v2c community strings,
//! which are cleartext on the wire and marked insecure rather than removed.

use uops_core::scope::Actor;
use uops_core::{
    ActorId, AuthProtocol, CredentialMaterial, CredentialRef, PrivProtocol, ResourceId, Secret,
    TenantId,
};
use uops_secrets::AccessContext;

/// How good the transport security of a credential actually is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Strength {
    /// Cleartext on the wire. v1/v2c.
    None,
    /// authPriv, with at least one algorithm that should not be relied on.
    Weak,
    /// authPriv with modern algorithms. SPEC's criterion.
    Strong,
}

/// Why a credential could not be used for SNMP.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum CredentialError {
    #[error(
        "this is a {found} credential; SNMP polling needs a community string or an \
         SNMPv3 user"
    )]
    WrongKind { found: &'static str },

    #[error("the credential store refused: {0}")]
    Unavailable(String),
}

/// What a poller needs to talk to one device.
///
/// Deliberately not `Clone` and deliberately holding a [`Secret`]: it exists for the
/// duration of a poll and then goes away. A cached credential is a credential that
/// outlives its rotation.
pub struct SnmpCredential {
    material: Secret<CredentialMaterial>,
}

/// Prints the *kind* and the protocols, never the material.
///
/// Hand-written rather than derived so that adding a field cannot quietly start
/// printing it. `Secret` already redacts itself; this is the second layer, and it is
/// the one a `{:?}` in a log line actually hits.
impl std::fmt::Debug for SnmpCredential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SnmpCredential")
            .field("strength", &self.strength())
            .field("protocols", &self.protocols())
            .finish_non_exhaustive()
    }
}

impl SnmpCredential {
    /// Wrap material the caller has already fetched.
    ///
    /// # Errors
    ///
    /// When the credential is not an SNMP one — an SSH key pointed at a poller.
    pub fn new(material: Secret<CredentialMaterial>) -> Result<Self, CredentialError> {
        let kind = match material.expose() {
            CredentialMaterial::SnmpCommunity(_) | CredentialMaterial::SnmpV3 { .. } => {
                return Ok(Self { material });
            }
            CredentialMaterial::SshPassword { .. } => "ssh password",
            CredentialMaterial::SshKey { .. } => "ssh key",
            CredentialMaterial::ApiToken(_) => "api token",
        };
        Err(CredentialError::WrongKind { found: kind })
    }

    /// The material, for the transport that is about to use it.
    #[must_use]
    pub const fn material(&self) -> &Secret<CredentialMaterial> {
        &self.material
    }

    /// What this credential's transport security is worth.
    #[must_use]
    pub fn strength(&self) -> Strength {
        match self.material.expose() {
            CredentialMaterial::SnmpV3 { auth, privacy, .. } => {
                if auth.is_weak() || privacy.is_weak() {
                    Strength::Weak
                } else {
                    Strength::Strong
                }
            }
            // A community string is cleartext; the other variants cannot reach here
            // because new() refused them, and would be cleartext if they could.
            _ => Strength::None,
        }
    }

    /// The protocols, for reporting. Never the keys.
    #[must_use]
    pub fn protocols(&self) -> Option<(AuthProtocol, PrivProtocol)> {
        match self.material.expose() {
            CredentialMaterial::SnmpV3 { auth, privacy, .. } => Some((*auth, *privacy)),
            _ => None,
        }
    }

    /// One line for an operator, naming the weakness when there is one.
    ///
    /// No key material, no username. This ends up in a device detail page and in
    /// support conversations.
    #[must_use]
    pub fn describe(&self) -> String {
        match self.material.expose() {
            CredentialMaterial::SnmpCommunity(_) => {
                "SNMP v2c community string — cleartext on the wire".to_owned()
            }
            CredentialMaterial::SnmpV3 { auth, privacy, .. } => {
                use std::fmt::Write as _;

                let mut s = format!("SNMPv3 authPriv ({}/{})", auth.as_str(), privacy.as_str());
                if auth.is_weak() {
                    let _ = write!(s, "; {} is broken and should be replaced", auth.as_str());
                }
                if privacy.is_weak() {
                    let _ = write!(s, "; {} has a 56-bit effective key", privacy.as_str());
                }
                s
            }
            _ => "not an SNMP credential".to_owned(),
        }
    }
}

/// The access-log context a poll uses.
///
/// `Actor::Collector`, because that is what is asking — not the user whose dashboard
/// happened to trigger nothing. The purpose string is what an auditor reads when they
/// ask why a credential was opened at 03:14, and `"snmp-poll"` answers it.
#[must_use]
pub fn poll_context(resource: ResourceId) -> AccessContext {
    AccessContext::new(Actor::Collector, "snmp-poll").for_resource(resource)
}

/// The context for a one-off test from the UI, which a person triggered.
///
/// A different purpose *and* a different actor, because "an operator tested this
/// credential" and "the poller used it" are different events and an audit log that
/// cannot tell them apart cannot answer the question that gets asked after an incident.
#[must_use]
pub fn test_context(actor: ActorId, resource: ResourceId) -> AccessContext {
    AccessContext::new(Actor::User(actor), "snmp-credential-test").for_resource(resource)
}

/// A credential reference, resolved for a device.
///
/// A thin alias today; named so that the poller's signature says what it wants rather
/// than passing a bare uuid around.
pub type Reference = CredentialRef;

/// Which tenant and credential a poll is about.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Request {
    pub tenant: TenantId,
    pub credential: Reference,
    pub resource: ResourceId,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v3(auth: AuthProtocol, privacy: PrivProtocol) -> SnmpCredential {
        SnmpCredential::new(Secret::new(CredentialMaterial::SnmpV3 {
            username: "netops".into(),
            auth,
            auth_key: "auth-key-material".into(),
            privacy,
            priv_key: "priv-key-material".into(),
        }))
        .unwrap()
    }

    #[test]
    fn spec_s_combination_is_the_strong_one() {
        let c = v3(AuthProtocol::Sha256, PrivProtocol::Aes256);
        assert_eq!(c.strength(), Strength::Strong);
        assert_eq!(
            c.protocols(),
            Some((AuthProtocol::Sha256, PrivProtocol::Aes256))
        );
    }

    #[test]
    fn one_weak_half_makes_the_whole_credential_weak() {
        // A strong hash over a DES-encrypted payload is a DES-encrypted payload.
        assert_eq!(
            v3(AuthProtocol::Sha256, PrivProtocol::Des).strength(),
            Strength::Weak
        );
        assert_eq!(
            v3(AuthProtocol::Md5, PrivProtocol::Aes256).strength(),
            Strength::Weak
        );
    }

    #[test]
    fn a_weak_credential_is_usable_and_says_why_it_is_weak() {
        // The position this module takes: an unmonitored switch is worse than a
        // monitored one with old SNMP.
        let c = v3(AuthProtocol::Md5, PrivProtocol::Des);
        let described = c.describe();
        assert!(described.contains("md5"), "{described}");
        assert!(described.contains("broken"), "{described}");
        assert!(described.contains("56-bit"), "{described}");
    }

    #[test]
    fn a_description_never_contains_key_material() {
        // It goes on a device detail page and into support tickets.
        for (a, p) in [
            (AuthProtocol::Sha256, PrivProtocol::Aes256),
            (AuthProtocol::Md5, PrivProtocol::Des),
        ] {
            let described = v3(a, p).describe();
            assert!(!described.contains("auth-key-material"), "{described}");
            assert!(!described.contains("priv-key-material"), "{described}");
            assert!(!described.contains("netops"), "{described}");
        }
    }

    #[test]
    fn a_community_string_is_honest_about_being_cleartext() {
        let c = SnmpCredential::new(Secret::new(CredentialMaterial::SnmpCommunity(
            "public".into(),
        )))
        .unwrap();
        assert_eq!(c.strength(), Strength::None);
        assert!(c.describe().contains("cleartext"));
        assert_eq!(c.protocols(), None);
        assert!(!c.describe().contains("public"), "leaked the community");
    }

    #[test]
    fn an_ssh_key_pointed_at_a_poller_is_refused_by_kind() {
        // A real misconfiguration: one credential list, several collectors.
        let err = SnmpCredential::new(Secret::new(CredentialMaterial::SshKey {
            username: "svc".into(),
            private_key: "-----BEGIN".into(),
            passphrase: String::new(),
        }))
        .unwrap_err();
        assert_eq!(err, CredentialError::WrongKind { found: "ssh key" });
        // And the message says what it got, so the fix is obvious.
        assert!(format!("{err}").contains("ssh key"));
    }

    #[test]
    fn debug_shows_the_shape_and_none_of_the_material() {
        // The layer that a stray {:?} in a log line actually hits. Secret redacts
        // itself; this is the one that decides what a struct holding one prints.
        let rendered = format!("{:?}", v3(AuthProtocol::Sha256, PrivProtocol::Aes256));
        assert!(rendered.contains("Strong"), "{rendered}");
        assert!(rendered.contains("Sha256"), "{rendered}");
        assert!(!rendered.contains("auth-key-material"), "{rendered}");
        assert!(!rendered.contains("priv-key-material"), "{rendered}");
        assert!(!rendered.contains("netops"), "{rendered}");
    }

    #[test]
    fn strength_is_ordered_so_a_policy_can_compare_it() {
        assert!(Strength::Strong > Strength::Weak);
        assert!(Strength::Weak > Strength::None);
    }

    #[test]
    fn a_poll_and_an_operator_test_are_different_events() {
        // An audit log that cannot tell "the poller used it" from "somebody tested it"
        // cannot answer the question asked after an incident.
        let resource = ResourceId::new();
        let poll = poll_context(resource);
        let test = test_context(ActorId::new(), resource);
        assert_ne!(poll.purpose, test.purpose);
        assert_ne!(poll.actor, test.actor);
    }
}
