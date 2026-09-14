//! Splitting a migration file into individual statements.
//!
//! Not a nicety. The `ClickHouse` HTTP interface accepts **one statement per request**,
//! so a migration file has to be split before it can be sent at all. And because
//! `ClickHouse` has no transactional DDL, the split is also the unit of recovery: a
//! file that fails at statement three has applied statements one and two, and the
//! runner records exactly that.
//!
//! Splitting on `;` with `str::split` would be wrong in three ways that all appear in
//! this project's own migrations: a semicolon inside a string literal, one inside a
//! comment, and one inside a quoted identifier. Each would produce a fragment that
//! either fails to parse or — worse — parses into something different from what was
//! written.

/// One statement, and the text a human should see for it in a log line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Statement {
    /// The full text, comments included, exactly as it will be sent.
    pub sql: String,
}

impl Statement {
    /// A one-line description for progress output: the first line that is actually
    /// SQL, with leading comments skipped and the tail elided.
    #[must_use]
    pub fn summary(&self) -> String {
        let first = self
            .sql
            .lines()
            .map(str::trim)
            .find(|l| !l.is_empty() && !l.starts_with("--"))
            .unwrap_or("");

        let mut out: String = first.chars().take(72).collect();
        if first.chars().count() > 72 {
            out.push('…');
        }
        out
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Normal,
    LineComment,
    BlockComment,
    /// `'…'` — string literal.
    Single,
    /// `"…"` or `` `…` `` — quoted identifier.
    Quoted(char),
}

/// Split a migration file into statements, discarding fragments that are only
/// whitespace and comments.
///
/// The trailing `;` is dropped; everything else, including interior comments, is
/// preserved so that what the server receives is what the file says.
#[must_use]
pub fn split(sql: &str) -> Vec<Statement> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut mode = Mode::Normal;
    let mut chars = sql.chars().peekable();

    while let Some(c) = chars.next() {
        match mode {
            Mode::Normal => match c {
                ';' => {
                    push_if_meaningful(&mut out, &current);
                    current.clear();
                    continue;
                }
                '-' if chars.peek() == Some(&'-') => mode = Mode::LineComment,
                '/' if chars.peek() == Some(&'*') => mode = Mode::BlockComment,
                '\'' => mode = Mode::Single,
                '"' | '`' => mode = Mode::Quoted(c),
                _ => {}
            },
            Mode::LineComment => {
                if c == '\n' {
                    mode = Mode::Normal;
                }
            }
            Mode::BlockComment => {
                if c == '*' && chars.peek() == Some(&'/') {
                    current.push(c);
                    current.push(chars.next().expect("peeked"));
                    mode = Mode::Normal;
                    continue;
                }
            }
            Mode::Single => match c {
                // ClickHouse accepts both escape styles, and they nest differently:
                // '\\' consumes the next character whatever it is, while '' is a
                // literal quote that does NOT end the string.
                '\\' => {
                    current.push(c);
                    if let Some(escaped) = chars.next() {
                        current.push(escaped);
                    }
                    continue;
                }
                '\'' if chars.peek() == Some(&'\'') => {
                    current.push(c);
                    current.push(chars.next().expect("peeked"));
                    continue;
                }
                '\'' => mode = Mode::Normal,
                _ => {}
            },
            Mode::Quoted(q) => {
                if c == q {
                    mode = Mode::Normal;
                }
            }
        }
        current.push(c);
    }

    push_if_meaningful(&mut out, &current);
    out
}

/// A fragment that is only whitespace and comments is not a statement. Sending one to
/// `ClickHouse` is an error, and counting it would shift every recorded step index.
fn push_if_meaningful(out: &mut Vec<Statement>, text: &str) {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return;
    }
    let has_sql = trimmed
        .lines()
        .map(str::trim)
        .any(|l| !l.is_empty() && !l.starts_with("--"));
    if !has_sql {
        return;
    }
    out.push(Statement {
        sql: trimmed.to_owned(),
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sqls(input: &str) -> Vec<String> {
        split(input).into_iter().map(|s| s.sql).collect()
    }

    #[test]
    fn splits_on_statement_boundaries() {
        assert_eq!(
            sqls("CREATE TABLE a (x UInt8) ENGINE=Memory;\nDROP TABLE a;"),
            vec![
                "CREATE TABLE a (x UInt8) ENGINE=Memory".to_owned(),
                "DROP TABLE a".to_owned(),
            ]
        );
    }

    #[test]
    fn a_semicolon_inside_a_string_is_not_a_boundary() {
        // This is not hypothetical: an enum definition or a default value containing a
        // semicolon would otherwise be cut in half and the halves sent separately.
        let out = sqls("INSERT INTO t VALUES ('a;b'); SELECT 1");
        assert_eq!(out.len(), 2, "{out:?}");
        assert!(out[0].contains("'a;b'"));
    }

    #[test]
    fn a_semicolon_inside_a_comment_is_not_a_boundary() {
        let out = sqls("-- drop it; really\nSELECT 1;\n/* and; again */ SELECT 2;");
        assert_eq!(out.len(), 2, "{out:?}");
    }

    #[test]
    fn a_semicolon_inside_a_quoted_identifier_is_not_a_boundary() {
        let out = sqls("SELECT `weird;name` FROM t; SELECT 2");
        assert_eq!(out.len(), 2, "{out:?}");
        assert!(out[0].contains("`weird;name`"));
    }

    #[test]
    fn both_string_escape_styles_survive() {
        // '' and backslash both mean "this quote does not end the string". Getting
        // either wrong flips the parser into the opposite mode for the rest of the
        // file, and every later boundary is then in the wrong place.
        let out = sqls("SELECT 'it''s'; SELECT 'a\\'b'; SELECT 3");
        assert_eq!(out.len(), 3, "{out:?}");
        assert!(out[0].contains("it''s"));
    }

    #[test]
    fn comment_only_fragments_are_not_statements() {
        // A file that ends with a trailing comment after the last semicolon must not
        // produce an empty statement: ClickHouse rejects it, and it would shift the
        // recorded step indices of everything before it on the next run.
        let out = sqls("SELECT 1;\n-- a trailing note\n");
        assert_eq!(out, vec!["SELECT 1".to_owned()]);

        assert!(sqls("-- nothing but a comment\n").is_empty());
        assert!(sqls("  \n\t\n").is_empty());
    }

    #[test]
    fn a_missing_final_semicolon_still_yields_the_statement() {
        assert_eq!(sqls("SELECT 1"), vec!["SELECT 1".to_owned()]);
    }

    #[test]
    fn summary_skips_the_comment_header() {
        let s = &split(
            "-- 0002 — the tail. W1 FIX 1.\n\
             -- Measured: 2 303 ms.\n\
             ALTER TABLE logs ADD PROJECTION IF NOT EXISTS p_by_time (SELECT * ORDER BY x);",
        )[0];
        assert!(
            s.summary().starts_with("ALTER TABLE logs ADD PROJECTION"),
            "{}",
            s.summary()
        );
    }

    #[test]
    fn this_projects_own_migrations_split_cleanly() {
        // The real files, at the real path. Guards against a migration being written in
        // a style the splitter mishandles — which would show up as a confusing server
        // error rather than as a parse failure here.
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../ch-migrations");
        let mut files: Vec<_> = std::fs::read_dir(&dir)
            .expect("ch-migrations must exist")
            .filter_map(|e| {
                let p = e.ok()?.path();
                (p.extension()? == "sql").then_some(p)
            })
            .collect();
        files.sort();
        assert!(files.len() >= 6, "expected the M0 migration set: {files:?}");

        for f in files {
            let text = std::fs::read_to_string(&f).unwrap();
            let stmts = split(&text);
            assert!(!stmts.is_empty(), "{} split to nothing", f.display());
            for s in &stmts {
                // Not "contains no semicolon" — these files carry semicolons inside
                // comments, and the splitter is right to keep them. The property that
                // matters is that nothing splittable is left: re-splitting a statement
                // must yield exactly that statement back.
                assert_eq!(
                    split(&s.sql).len(),
                    1,
                    "{} left an unconsumed boundary in: {}",
                    f.display(),
                    s.summary()
                );
            }
        }
    }
}
