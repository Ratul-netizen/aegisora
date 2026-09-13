//! Serialising credential material for sealing.
//!
//! `CredentialMaterial` deliberately does **not** implement `Serialize` (SPEC §M0.4) —
//! if it did, it could reach an API response or a JSON log line. So this module does
//! the encoding by hand, and it is the only place that may.
//!
//! The wire format is length-prefixed, not delimited: a delimiter would have to be
//! escaped, and an escaping bug in credential parsing is a vulnerability rather than a
//! display glitch. Everything is written into a `Secret<Vec<u8>>` so the intermediate
//! buffer is zeroized rather than left in the allocator.

use uops_core::{CredentialMaterial, Secret};

use crate::error::{Error, Result};

const TAG_SNMP_COMMUNITY: u8 = 1;
const TAG_SNMP_V3: u8 = 2;
const TAG_SSH_PASSWORD: u8 = 3;
const TAG_SSH_KEY: u8 = 4;
const TAG_API_TOKEN: u8 = 5;

fn put_field(out: &mut Vec<u8>, s: &str) {
    // u32 length prefix: credential fields (an SSH private key, say) comfortably
    // exceed u16.
    let len = u32::try_from(s.len()).unwrap_or(u32::MAX);
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(s.as_bytes());
}

fn take_field(input: &[u8], pos: &mut usize) -> Result<String> {
    // Checked arithmetic throughout. These bytes have already been authenticated by
    // the AEAD, so they are not attacker-controlled unless the key is compromised —
    // but an overflow here would be an out-of-bounds panic in the credential path,
    // which is not a failure mode worth saving four instructions on.
    let end = pos
        .checked_add(4)
        .ok_or(Error::Corrupt("length prefix overflows"))?;
    if end > input.len() {
        return Err(Error::Corrupt("truncated length prefix"));
    }
    let len = u32::from_be_bytes([
        input[*pos],
        input[*pos + 1],
        input[*pos + 2],
        input[*pos + 3],
    ]) as usize;
    *pos = end;

    let field_end = pos
        .checked_add(len)
        .ok_or(Error::Corrupt("field length overflows"))?;
    if field_end > input.len() {
        return Err(Error::Corrupt("truncated field"));
    }
    let s = std::str::from_utf8(&input[*pos..field_end])
        .map_err(|_| Error::Corrupt("field is not utf-8"))?
        .to_owned();
    *pos = field_end;
    Ok(s)
}

#[must_use]
pub fn serialize_material(m: &CredentialMaterial) -> Secret<Vec<u8>> {
    let mut out = Vec::with_capacity(128);
    match m {
        CredentialMaterial::SnmpCommunity(s) => {
            out.push(TAG_SNMP_COMMUNITY);
            put_field(&mut out, s);
        }
        CredentialMaterial::SnmpV3 {
            username,
            auth_key,
            priv_key,
        } => {
            out.push(TAG_SNMP_V3);
            put_field(&mut out, username);
            put_field(&mut out, auth_key);
            put_field(&mut out, priv_key);
        }
        CredentialMaterial::SshPassword { username, password } => {
            out.push(TAG_SSH_PASSWORD);
            put_field(&mut out, username);
            put_field(&mut out, password);
        }
        CredentialMaterial::SshKey {
            username,
            private_key,
            passphrase,
        } => {
            out.push(TAG_SSH_KEY);
            put_field(&mut out, username);
            put_field(&mut out, private_key);
            put_field(&mut out, passphrase);
        }
        CredentialMaterial::ApiToken(t) => {
            out.push(TAG_API_TOKEN);
            put_field(&mut out, t);
        }
    }
    Secret::new(out)
}

pub fn deserialize_material(input: &[u8]) -> Result<Secret<CredentialMaterial>> {
    let Some((&tag, rest)) = input.split_first() else {
        return Err(Error::Corrupt("empty payload"));
    };
    let mut pos = 0usize;

    let material = match tag {
        TAG_SNMP_COMMUNITY => CredentialMaterial::SnmpCommunity(take_field(rest, &mut pos)?),
        TAG_SNMP_V3 => CredentialMaterial::SnmpV3 {
            username: take_field(rest, &mut pos)?,
            auth_key: take_field(rest, &mut pos)?,
            priv_key: take_field(rest, &mut pos)?,
        },
        TAG_SSH_PASSWORD => CredentialMaterial::SshPassword {
            username: take_field(rest, &mut pos)?,
            password: take_field(rest, &mut pos)?,
        },
        TAG_SSH_KEY => CredentialMaterial::SshKey {
            username: take_field(rest, &mut pos)?,
            private_key: take_field(rest, &mut pos)?,
            passphrase: take_field(rest, &mut pos)?,
        },
        TAG_API_TOKEN => CredentialMaterial::ApiToken(take_field(rest, &mut pos)?),
        _ => return Err(Error::Corrupt("unknown credential tag")),
    };

    Ok(Secret::new(material))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Returns the re-read value still wrapped, because `CredentialMaterial`
    /// implements `Drop` in order to zeroize — so its fields cannot be moved out of a
    /// pattern, and callers must borrow through `expose()`. That restriction is the
    /// zeroize guarantee working as intended, not an inconvenience to design around.
    fn round_trip(m: &CredentialMaterial) -> Secret<CredentialMaterial> {
        let bytes = serialize_material(m);
        deserialize_material(bytes.expose()).unwrap()
    }

    #[test]
    fn every_variant_round_trips() {
        let out = round_trip(&CredentialMaterial::SnmpCommunity("public".into()));
        assert!(matches!(out.expose(), CredentialMaterial::SnmpCommunity(s) if s == "public"));

        let out = round_trip(&CredentialMaterial::SnmpV3 {
            username: "admin".into(),
            auth_key: "auth".into(),
            priv_key: "priv".into(),
        });
        assert!(matches!(
            out.expose(),
            CredentialMaterial::SnmpV3 { username, auth_key, priv_key }
                if username == "admin" && auth_key == "auth" && priv_key == "priv"
        ));

        let out = round_trip(&CredentialMaterial::SshPassword {
            username: "svc".into(),
            password: "p@ss".into(),
        });
        assert!(matches!(
            out.expose(),
            CredentialMaterial::SshPassword { username, password }
                if username == "svc" && password == "p@ss"
        ));

        let out = round_trip(&CredentialMaterial::ApiToken("t0ken".into()));
        assert!(matches!(out.expose(), CredentialMaterial::ApiToken(t) if t == "t0ken"));
    }

    #[test]
    fn fields_containing_delimiters_survive() {
        // The reason for length prefixes rather than a separator. A private key is full
        // of newlines, and an SNMP community string can contain anything at all.
        let key = "-----BEGIN KEY-----\nline\0with\tnulls\n-----END KEY-----";
        let out = round_trip(&CredentialMaterial::SshKey {
            username: "a\0b".into(),
            private_key: key.into(),
            passphrase: String::new(),
        });
        match out.expose() {
            CredentialMaterial::SshKey {
                username,
                private_key,
                passphrase,
            } => {
                assert_eq!(username, "a\0b");
                assert_eq!(private_key, key);
                assert!(passphrase.is_empty(), "empty fields must round-trip too");
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn unicode_survives() {
        let out = round_trip(&CredentialMaterial::SnmpCommunity("পাসওয়ার্ড".into()));
        assert!(matches!(out.expose(), CredentialMaterial::SnmpCommunity(s) if s == "পাসওয়ার্ড"));
    }

    #[test]
    fn corrupt_input_errors_rather_than_panicking() {
        // These bytes come from a decrypted blob. A malformed one should be an error,
        // never an index-out-of-bounds panic in the credential path.
        assert!(deserialize_material(&[]).is_err());
        assert!(deserialize_material(&[99]).is_err(), "unknown tag");
        assert!(deserialize_material(&[TAG_API_TOKEN]).is_err(), "no length");
        assert!(
            deserialize_material(&[TAG_API_TOKEN, 0, 0, 0, 200, b'x']).is_err(),
            "length beyond the buffer must not panic"
        );
        assert!(
            deserialize_material(&[TAG_API_TOKEN, 0, 0, 0, 1, 0xff]).is_err(),
            "invalid utf-8"
        );
    }
}
