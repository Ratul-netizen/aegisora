//! Parameterised statement text.
//!
//! Every literal is bound, never formatted in. `ClickHouse` takes typed parameters as
//! `{name:Type}`, which means the type is declared at the call site and a value that
//! does not parse as that type is rejected by the server rather than pasted into the
//! statement.
//!
//! Two consequences worth stating, because they are the reason this module exists
//! rather than a `format!` call:
//!
//!   * There is **no escaping function anywhere in this crate**. Nothing is escaped
//!     because nothing is interpolated. The only caller-supplied strings that reach the
//!     text are aggregation aliases, which are validated as identifiers and rejected if
//!     they are anything else.
//!   * `IN` lists expand to one parameter per element rather than a single array
//!     parameter. An array parameter would need its own literal syntax — quoting,
//!     escaping, the whole problem back again — for no benefit.

use std::collections::BTreeMap;
use std::fmt::Write as _;

/// A bound value, with the `ClickHouse` type the server will parse it as.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Param {
    pub ty: &'static str,
    pub value: String,
}

/// A compiled statement: text plus the parameters it refers to.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Sql {
    text: String,
    params: BTreeMap<String, Param>,
}

impl Sql {
    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }

    #[must_use]
    pub const fn params(&self) -> &BTreeMap<String, Param> {
        &self.params
    }

    /// Stable rendering for the golden files: statement, then every parameter sorted
    /// by name. A diff here is a codegen change, and codegen changes should be visible
    /// in review rather than discovered against a customer's data.
    #[must_use]
    pub fn to_golden(&self) -> String {
        let mut s = self.text.clone();
        s.push('\n');
        if self.params.is_empty() {
            return s;
        }
        s.push_str("\n-- params\n");
        for (name, p) in &self.params {
            let _ = writeln!(s, "--   {name} {} = {}", p.ty, p.value);
        }
        s
    }
}

/// Accumulates statement text and parameters together, so a value cannot be written
/// into one without being registered in the other.
#[derive(Debug, Default)]
pub(crate) struct Builder {
    text: String,
    params: BTreeMap<String, Param>,
    next: usize,
}

impl Builder {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Fixed text: keywords, and column names that came from an enum.
    pub(crate) fn push(&mut self, s: &str) {
        self.text.push_str(s);
    }

    /// Bind a value and write its placeholder. The only way a value reaches the text.
    pub(crate) fn bind(&mut self, ty: &'static str, value: impl Into<String>) {
        let name = format!("p{}", self.next);
        self.next += 1;
        let _ = write!(self.text, "{{{name}:{ty}}}");
        self.params.insert(
            name,
            Param {
                ty,
                value: value.into(),
            },
        );
    }

    pub(crate) fn finish(self) -> Sql {
        Sql {
            text: self.text,
            params: self.params,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bound_value_appears_only_as_a_placeholder() {
        let mut b = Builder::new();
        b.push("WHERE body = ");
        // A value that would end the statement early and start another one, if this
        // crate ever grew a code path that formatted values into the text.
        b.bind("String", "x' OR 1=1; DROP TABLE logs --");
        let sql = b.finish();

        assert_eq!(sql.text(), "WHERE body = {p0:String}");
        assert!(
            !sql.text().contains("DROP"),
            "the value must not reach the statement text: {}",
            sql.text()
        );
        assert_eq!(sql.params()["p0"].value, "x' OR 1=1; DROP TABLE logs --");
    }

    #[test]
    fn placeholders_are_numbered_in_binding_order() {
        let mut b = Builder::new();
        b.bind("UUID", "a");
        b.push(", ");
        b.bind("UUID", "b");
        assert_eq!(b.finish().text(), "{p0:UUID}, {p1:UUID}");
    }

    #[test]
    fn golden_rendering_is_sorted_and_stable() {
        let mut b = Builder::new();
        b.push("SELECT ");
        b.bind("Int64", "1");
        let golden = b.finish().to_golden();
        assert_eq!(
            golden,
            "SELECT {p0:Int64}\n\n-- params\n--   p0 Int64 = 1\n"
        );
    }
}
