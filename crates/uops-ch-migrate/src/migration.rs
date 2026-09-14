//! Reading `ch-migrations/` into an ordered, checksummed set.

use std::fs;
use std::path::{Path, PathBuf};

use std::fmt::Write as _;

use sha2::{Digest, Sha256};

use crate::error::{Error, Result};
use crate::statement::{Statement, split};

/// One migration file, parsed.
#[derive(Clone, Debug)]
pub struct Migration {
    /// The `NNNN` prefix. Ordering is numeric, not lexical.
    pub version: u32,
    /// The filename without its extension, e.g. `0003_logs_counts_5m`.
    pub name: String,
    /// SHA-256 of the file bytes, hex. Integrity, not secrecy: it answers "has this
    /// file changed since it was applied", and the answer decides whether the runner
    /// refuses to continue.
    pub checksum: String,
    pub statements: Vec<Statement>,
    pub path: PathBuf,
}

impl Migration {
    /// Hash of one statement, recorded per step so a resumed migration can verify that
    /// the step it is about to skip is the step that was actually applied.
    #[must_use]
    pub fn step_checksum(&self, index: usize) -> String {
        self.statements
            .get(index)
            .map_or_else(String::new, |s| hash(s.sql.as_bytes()))
    }
}

pub(crate) fn hash(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    // Hex by hand rather than pulling in a hex crate for sixteen lines of use.
    let mut out = String::with_capacity(digest.len() * 2);
    for b in digest {
        let _ = write!(out, "{b:02x}");
    }
    out
}

/// Load every `NNNN_*.sql` in `dir`, ordered by version.
///
/// Subdirectories are ignored, which is what keeps `deferred/` — the traces and flows
/// DDL that M7/M8 will need — out of the applied set while leaving it in the repository
/// where it can be reviewed.
pub fn load_dir(dir: &Path) -> Result<Vec<Migration>> {
    let entries = fs::read_dir(dir).map_err(|source| Error::Io {
        path: dir.display().to_string(),
        source,
    })?;

    let mut found: Vec<Migration> = Vec::new();
    for entry in entries {
        let path = entry
            .map_err(|source| Error::Io {
                path: dir.display().to_string(),
                source,
            })?
            .path();

        if !path.is_file() || path.extension().is_none_or(|e| e != "sql") {
            continue;
        }

        let stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or_default()
            .to_owned();
        let version = parse_version(&stem)?;

        let text = fs::read_to_string(&path).map_err(|source| Error::Io {
            path: path.display().to_string(),
            source,
        })?;

        found.push(Migration {
            version,
            name: stem,
            // Normalised so that a checkout with different line endings is not read as
            // an edit. Windows and Linux clones of this repository must agree, or CI
            // and a developer machine reach opposite conclusions about the same file.
            checksum: hash(text.replace("\r\n", "\n").as_bytes()),
            statements: split(&text),
            path,
        });
    }

    found.sort_by_key(|m| m.version);

    for pair in found.windows(2) {
        if pair[0].version == pair[1].version {
            return Err(Error::DuplicateVersion {
                version: pair[0].version,
                a: pair[0].name.clone(),
                b: pair[1].name.clone(),
            });
        }
    }

    Ok(found)
}

/// `0003_logs_counts_5m` → 3.
fn parse_version(stem: &str) -> Result<u32> {
    let (digits, rest) = stem.split_at(stem.len().min(4));
    let ok = digits.len() == 4 && digits.bytes().all(|b| b.is_ascii_digit());
    if !ok || !rest.starts_with('_') || rest.len() < 2 {
        return Err(Error::BadFilename {
            name: stem.to_owned(),
        });
    }
    digits.parse().map_err(|_| Error::BadFilename {
        name: stem.to_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ch_migrations() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../ch-migrations")
    }

    #[test]
    fn versions_are_numeric_not_lexical() {
        // The day this matters is the day migration 0010 arrives: lexical ordering
        // would run it before 0002, and the schema would be built in an order nobody
        // tested.
        assert_eq!(parse_version("0001_logs").unwrap(), 1);
        assert_eq!(parse_version("0010_later").unwrap(), 10);
        assert!(parse_version("0002_a").unwrap() < parse_version("0010_b").unwrap());
    }

    #[test]
    fn filenames_that_are_not_migrations_are_rejected_loudly() {
        // Silently skipping an unparseable name is how a migration goes missing without
        // anyone noticing until a table is absent in production.
        for bad in ["logs", "1_logs", "00001_logs", "0001logs", "0001_"] {
            assert!(parse_version(bad).is_err(), "{bad} should be rejected");
        }
    }

    #[test]
    fn the_real_migration_set_loads_in_order() {
        let set = load_dir(&ch_migrations()).unwrap();
        let versions: Vec<u32> = set.iter().map(|m| m.version).collect();
        assert_eq!(versions, vec![1, 2, 3, 4, 5, 6], "{versions:?}");
        assert!(set.iter().all(|m| !m.statements.is_empty()));
    }

    #[test]
    fn the_deferred_directory_is_not_part_of_the_applied_set() {
        // traces and flows are declared in the repository and created in M7/M8. If they
        // leaked into the applied set, every deployment would carry two empty tables
        // with TTLs and a bloom filter nobody asked for.
        let set = load_dir(&ch_migrations()).unwrap();
        assert!(
            !set.iter().any(|m| m.name.contains("traces")),
            "deferred DDL must not be applied"
        );
        assert!(ch_migrations().join("deferred/traces_flows.sql").exists());
    }

    #[test]
    fn checksums_are_stable_and_line_ending_independent() {
        // A Windows clone and a Linux clone must agree. Without normalisation, CI would
        // report every migration as edited the first time a developer on the other
        // platform touched the repository.
        assert_eq!(hash(b"CREATE TABLE a"), hash(b"CREATE TABLE a"));
        assert_ne!(hash(b"CREATE TABLE a"), hash(b"CREATE TABLE b"));

        let crlf = "SELECT 1;\r\nSELECT 2;\r\n";
        let lf = "SELECT 1;\nSELECT 2;\n";
        assert_eq!(
            hash(crlf.replace("\r\n", "\n").as_bytes()),
            hash(lf.as_bytes())
        );
    }
}
