//! Turning `sqlx` failures into the platform's error vocabulary.
//!
//! The mapping is not cosmetic. Two `SQLSTATE`s carry product meaning that would be lost
//! if everything became a 500:
//!
//! * **`23505` on `resource_identifier`** is not a database problem. It is two devices
//!   claiming one hostname — a genuine identity conflict, which belongs in the review
//!   queue as a 409 rather than in an error log as a 500. SPEC §M0.2 is explicit about
//!   this, and the `UNIQUE (tenant_id, kind, value)` constraint is how it surfaces.
//! * **`23503`** from one of the composite foreign keys means a row tried to reference
//!   another tenant's row. That is the schema refusing a cross-tenant reference, and it
//!   must not be reported as "not found" by accident — it is a bug or an attack.

use uops_core::Error as CoreError;

/// Map a database failure onto the shared error type.
///
/// `kind` names the thing being operated on, so `RowNotFound` can produce a useful
/// `NotFound` rather than a bare "not found".
pub(crate) fn map(kind: &'static str, id: String, e: sqlx::Error) -> CoreError {
    match e {
        sqlx::Error::RowNotFound => CoreError::NotFound { kind, id },
        sqlx::Error::Database(db) => map_database(kind, db.as_ref()),
        other => CoreError::Storage(other.to_string()),
    }
}

fn map_database(kind: &'static str, db: &dyn sqlx::error::DatabaseError) -> CoreError {
    let constraint = db.constraint().unwrap_or_default().to_owned();

    match db.code().as_deref() {
        // unique_violation
        Some("23505") if constraint.starts_with("resource_identifier") => {
            CoreError::IdentityConflict {
                kind: "identifier",
                value: constraint,
            }
        }
        Some("23505") => CoreError::Invalid(format!("{kind} already exists ({constraint})")),

        // foreign_key_violation. The composite keys carry tenant_id, so this is where a
        // cross-tenant reference lands — see migrations/0002_resource.sql.
        Some("23503") => CoreError::Invalid(format!(
            "{kind} references a row that does not exist in this tenant ({constraint})"
        )),

        // check_violation
        Some("23514") => CoreError::Invalid(format!("{kind} violates {constraint}")),

        _ => CoreError::Storage(db.message().to_owned()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_row_is_a_not_found_that_names_what_was_missing() {
        let e = map("resource", "abc".into(), sqlx::Error::RowNotFound);
        assert_eq!(e.status_code(), 404);
        assert!(e.to_string().contains("resource"), "{e}");
        assert!(e.to_string().contains("abc"), "{e}");
    }

    #[test]
    fn a_pool_failure_is_a_500_not_a_400() {
        let e = map("resource", String::new(), sqlx::Error::PoolTimedOut);
        assert_eq!(e.status_code(), 500);
    }

    // The SQLSTATE arms are asserted against the real server in tests/integration.rs:
    // constructing a sqlx DatabaseError by hand would assert only that this file
    // matches itself, and the thing worth testing is which constraint PostgreSQL
    // actually reports.
}
