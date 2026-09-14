//! Generating a password a human has to type once.
//!
//! The first-run admin credential is shown on a terminal and typed into a login form,
//! probably from a photograph of that terminal. That is a different problem from a
//! session token, which no human ever reads:
//!
//! | | session token | first-run password |
//! |---|---|---|
//! | read by | software | a person, once, possibly off a screen |
//! | alphabet | hex, 64 chars | unambiguous, ~20 chars |
//! | entropy target | 256 bits | ~100 bits |
//!
//! Hex would be fine for a machine and miserable for a person. The alphabet below drops
//! every character that is mistaken for another in a terminal font — `0`/`O`, `1`/`l`/`I`
//! — because "the password did not work" from a misread character is a support call, and
//! the person making it has no way to tell a typo from a bug.

use uops_core::Secret;

use crate::error::{Error, Result};

/// Unambiguous in every terminal font: no `0 O o`, no `1 l I`, no `5 S`, no `2 Z`.
const ALPHABET: &[u8] = b"abcdefghijkmnpqrstuvwxyzACDEFGHJKLMNPQRTUVWXY346789";

/// Characters in a generated password.
///
/// 20 characters over a 50-character alphabet is about 112 bits — far more than a human
/// would choose and cheap to type once, which is the only time this is ever typed.
const LENGTH: usize = 20;

/// A fresh password, for showing once and then never again.
pub fn password() -> Result<Secret<String>> {
    let mut bytes = [0u8; LENGTH];
    getrandom::fill(&mut bytes).map_err(|e| Error::Random(e.to_string()))?;

    // Modulo bias: 256 is not a multiple of 50, so the first six characters of the
    // alphabet are very slightly more likely. The skew is under 0.4% and it costs
    // roughly 0.03 bits out of 112. Rejection sampling would remove it and add a loop
    // that can in principle not terminate; for a credential that is rotated the first
    // time anyone logs in, the trade is not close.
    let text: String = bytes
        .iter()
        .map(|b| ALPHABET[*b as usize % ALPHABET.len()] as char)
        .collect();

    Ok(Secret::new(text))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn a_generated_password_is_long_and_from_the_safe_alphabet() {
        let p = password().unwrap();
        assert_eq!(p.expose().len(), LENGTH);
        assert!(
            p.expose().bytes().all(|b| ALPHABET.contains(&b)),
            "{}",
            p.expose()
        );
    }

    #[test]
    fn the_alphabet_contains_nothing_that_is_misread() {
        // Someone reads this off a terminal, possibly from a photograph of one. A
        // character they get wrong produces "the password did not work", and they have
        // no way to tell that from a bug.
        for &confusable in b"0Oo1lI5S2ZB8" {
            let present = ALPHABET.contains(&confusable);
            // 8 is kept and B dropped; 0/O/o, 1/l/I, 5/S and 2/Z are all dropped.
            let allowed = confusable == b'8';
            assert_eq!(
                present,
                allowed,
                "{} should{} be in the alphabet",
                confusable as char,
                if allowed { "" } else { " not" }
            );
        }
        assert!(
            ALPHABET.len() >= 48,
            "dropping confusables must not leave too little entropy per character"
        );
    }

    #[test]
    fn passwords_do_not_repeat() {
        // Not a statistical test — a smoke test that the CSPRNG is read rather than
        // something deterministic having crept in.
        let mut seen = HashSet::new();
        for _ in 0..1_000 {
            assert!(
                seen.insert(password().unwrap().expose().to_owned()),
                "a generated password repeated"
            );
        }
    }

    #[test]
    fn a_password_cannot_be_printed_by_accident() {
        // It is shown once, deliberately, by the first-run path. Everywhere else it is
        // a credential, and Secret<T> is not Display or Serialize.
        let p = password().unwrap();
        assert!(!format!("{p:?}").contains(p.expose()));
    }
}
