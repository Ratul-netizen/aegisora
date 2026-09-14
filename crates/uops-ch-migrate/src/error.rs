//! Migration errors.
//!
//! Most of these are refusals rather than failures: the runner found a reason not to
//! touch the schema, and stopping is the correct outcome. On-premise upgrades are
//! unattended, so an error here is read hours later out of a log file by someone who
//! was not present — each message names the file and says what to do about it.

use thiserror::Error;

pub type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Debug, Error)]
#[non_exhaustive]
pub enum Error {
    #[error("cannot read {path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },

    #[error(
        "{name} is not a migration filename; expected NNNN_description.sql \
         (four digits, underscore, description)"
    )]
    BadFilename { name: String },

    #[error("version {version} is used by two files: {a} and {b}")]
    DuplicateVersion { version: u32, a: String, b: String },

    /// The file for an applied migration is gone. Either someone deleted it, or this
    /// binary is older than the database it is pointed at — which is the common case
    /// during a rollback, and the one where continuing would be worst.
    #[error(
        "migration {version} ({name}) is recorded as applied but is not in the \
         migration directory; this binary may be older than the database"
    )]
    MissingOnDisk { version: u32, name: String },

    /// An applied migration was edited. The schema the database has is not the schema
    /// the file now describes, and nothing the runner can do will reconcile them.
    #[error(
        "migration {version} ({name}) was edited after it was applied \
         (recorded {recorded}, on disk {found}); write a new migration instead"
    )]
    Changed {
        version: u32,
        name: String,
        recorded: String,
        found: String,
    },

    /// A new migration numbered below one already applied. Two branches each added a
    /// migration, and the merge produced an order that was never tested.
    #[error(
        "migration {version} ({name}) is new but numbered below {applied}, \
         which is already applied; renumber it above {applied}"
    )]
    OutOfOrder {
        version: u32,
        name: String,
        applied: u32,
    },

    #[error("clickhouse: {0}")]
    Server(String),

    #[error("clickhouse is unreachable at {url}: {detail}")]
    Unreachable { url: String, detail: String },

    #[error("unexpected response from clickhouse: {0}")]
    Protocol(String),
}

impl From<Error> for uops_core::Error {
    fn from(e: Error) -> Self {
        Self::Storage(e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drift_messages_name_the_file_and_the_remedy() {
        // These are read out of an unattended upgrade log by someone who was not there
        // when it happened. "checksum mismatch" alone would start an investigation;
        // naming the file and the fix ends one.
        let e = Error::Changed {
            version: 3,
            name: "0003_logs_counts_5m".into(),
            recorded: "abc123".into(),
            found: "def456".into(),
        };
        let msg = e.to_string();
        assert!(msg.contains("0003_logs_counts_5m"), "{msg}");
        assert!(msg.contains("write a new migration"), "{msg}");

        let e = Error::OutOfOrder {
            version: 2,
            name: "0002_oops".into(),
            applied: 6,
        };
        assert!(e.to_string().contains("renumber"), "{e}");
    }

    #[test]
    fn migration_failures_surface_as_storage_errors() {
        let mapped: uops_core::Error = Error::Server("syntax error".into()).into();
        assert_eq!(mapped.status_code(), 500);
    }
}
