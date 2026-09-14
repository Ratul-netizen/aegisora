//! The tenant predicate, enforced on this crate's own source.
//!
//! `TenantScope` makes it a compile error to *build a query without a tenant*. It cannot
//! make it a compile error to accept a `&TenantScope` and then forget to use it, and
//! this is the layer where that mistake would actually be made: the SQL is hand-written
//! and the scope is right there in the argument list.
//!
//! So the source is read back and checked. Cheap, greppable, and it fails in the same
//! place a reviewer would have had to notice — the same approach as the CI greps that
//! keep `.expose()` out of logging macros and crypto primitives out of every crate but
//! `uops-secrets`.
//!
//! A statement that genuinely has no tenant column — an organization lookup, a health
//! probe — opts out by carrying the marker `tenant-exempt` in a comment beside it, which
//! turns silence into a deliberate, reviewable act.

#[cfg(test)]
mod tests {
    use std::path::Path;

    /// Every `sqlx::query…!` invocation in this crate, as source text.
    ///
    /// Doc comments are stripped first: this crate's own prose mentions the macro by
    /// name, and a scanner that reads documentation as code reports the documentation.
    fn statements(source: &str) -> Vec<String> {
        let code: String = source
            .lines()
            .filter(|l| {
                let t = l.trim_start();
                !t.starts_with("///") && !t.starts_with("//!")
            })
            .collect::<Vec<_>>()
            .join("\n");

        let chunks: Vec<&str> = code.split("sqlx::query").collect();
        let mut out = Vec::new();

        for (i, chunk) in chunks.iter().enumerate().skip(1) {
            // The invocation ends where the builder chain starts.
            let end = [
                "\n        .fetch",
                "\n        .execute",
                ".fetch_",
                ".execute(",
            ]
            .iter()
            .filter_map(|marker| chunk.find(marker))
            .min()
            .unwrap_or(chunk.len());

            // Include the handful of lines before the call, so an exemption can be
            // written as an ordinary comment above it rather than wedged in among the
            // arguments. Six, not three: the split leaves the call's own indentation as
            // a line of its own, and a reason worth reading rarely fits on one line.
            let preceding: String = chunks[i - 1]
                .lines()
                .rev()
                .take(6)
                .collect::<Vec<_>>()
                .join("\n");

            out.push(format!("{preceding}\n{}", &chunk[..end]));
        }
        out
    }

    fn source_files() -> Vec<(String, String)> {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut files: Vec<(String, String)> = std::fs::read_dir(&dir)
            .expect("src must exist")
            .filter_map(|e| {
                let p = e.ok()?.path();
                if p.extension()? != "rs" {
                    return None;
                }
                let name = p.file_name()?.to_string_lossy().into_owned();
                Some((name, std::fs::read_to_string(&p).ok()?))
            })
            .collect();
        files.sort();
        files
    }

    #[test]
    fn every_statement_filters_by_tenant() {
        let mut offenders = Vec::new();
        let mut checked = 0;

        for (name, source) in source_files() {
            if name == "enforced.rs" {
                continue; // this file quotes the markers it looks for
            }
            for statement in statements(&source) {
                checked += 1;
                let ok = statement.contains("tenant_id") || statement.contains("tenant-exempt");
                if !ok {
                    let first = statement
                        .lines()
                        .find(|l| {
                            let t = l.trim();
                            !t.is_empty() && !t.starts_with("//")
                        })
                        .unwrap_or("")
                        .trim()
                        .to_owned();
                    offenders.push(format!("{name}: {first}"));
                }
            }
        }

        assert!(
            checked >= 6,
            "the scanner found only {checked} statements — it has stopped matching the \
             code it is supposed to be checking"
        );
        assert!(
            offenders.is_empty(),
            "these statements have no tenant predicate and no `tenant-exempt` marker, \
             so they can read across tenants:\n  {}",
            offenders.join("\n  ")
        );
    }

    #[test]
    fn the_scanner_catches_a_statement_that_forgot_the_tenant() {
        // A guard that cannot fail is not a guard. This is the shape of the mistake:
        // the scope is in the signature, and the WHERE clause does not use it.
        let bad = r#"
            pub async fn resource(&self, scope: &TenantScope, id: ResourceId) -> Result<Row> {
                sqlx::query_as!(Row, "SELECT * FROM resource WHERE id = $1", id)
                    .fetch_one(self.pool())
                    .await
            }
        "#;
        let found = statements(bad);
        assert_eq!(
            found.len(),
            1,
            "the scanner must see the statement: {found:?}"
        );
        assert!(
            !found[0].contains("tenant_id"),
            "and must not consider it tenant-scoped"
        );
    }

    #[test]
    fn the_scanner_accepts_an_explicit_exemption() {
        let exempt = r#"
            sqlx::query!(
                // tenant-exempt: organizations sit above the isolation boundary
                "SELECT id FROM organization"
            )
            .fetch_all(self.pool())
        "#;
        let found = statements(exempt);
        assert_eq!(found.len(), 1);
        assert!(found[0].contains("tenant-exempt"));
    }
}
