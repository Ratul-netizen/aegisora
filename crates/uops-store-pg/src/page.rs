//! Keyset pagination.
//!
//! SPEC §M1: cursor-based, **never `OFFSET`**. `OFFSET 50000` makes PostgreSQL walk and
//! discard fifty thousand rows to return twenty, so the last page of a large inventory
//! is the slowest — and on telemetry it is unusable outright.
//!
//! The cursor is a `ResourceId`, and that works because IDs are `UUIDv7`: they sort in
//! creation order, so `WHERE id > $cursor ORDER BY id` is both a stable page boundary
//! and an index range scan. A `UUIDv4` keyset would need a separate `(created_at, id)`
//! tuple to be stable at all.

use serde::{Deserialize, Serialize};
use uops_core::ResourceId;

/// The default page size. Large enough that the UI rarely pages, small enough that a
/// response stays within a screen's worth of rendering work.
pub const DEFAULT_PAGE: i64 = 50;
/// Server ceiling. Asking for more is a different feature — an export.
pub const MAX_PAGE: i64 = 500;

/// Where the next page starts. Opaque to the caller by convention; it is a
/// `ResourceId` today and that is not a promise.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Cursor(pub ResourceId);

impl std::fmt::Display for Cursor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

/// One page, and how to ask for the next.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Page<T> {
    pub items: Vec<T>,
    /// `None` when this is the last page.
    pub next: Option<Cursor>,
}

impl<T> Page<T> {
    /// Build a page from one row more than was asked for.
    ///
    /// Fetching `limit + 1` is how "is there a next page" is answered without a second
    /// `COUNT(*)` over the whole filtered set — which on fifty thousand resources costs
    /// more than the page itself.
    pub(crate) fn from_overfetch(
        mut rows: Vec<T>,
        limit: i64,
        cursor_of: impl Fn(&T) -> Cursor,
    ) -> Self {
        let limit = usize::try_from(limit).unwrap_or(0);
        let next = if rows.len() > limit {
            rows.truncate(limit);
            rows.last().map(&cursor_of)
        } else {
            None
        };
        Self { items: rows, next }
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.items.len()
    }
}

/// Clamp a requested page size into what the server will serve.
#[must_use]
pub fn page_size(requested: Option<i64>) -> i64 {
    requested.unwrap_or(DEFAULT_PAGE).clamp(1, MAX_PAGE)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(n: usize) -> Vec<ResourceId> {
        (0..n).map(|_| ResourceId::new()).collect()
    }

    #[test]
    fn an_overfetched_row_becomes_the_cursor_and_is_not_returned() {
        // The extra row exists to answer "is there more", not to be shown. Returning it
        // would make every page one item longer than requested.
        let rows = ids(4);
        let last_returned = rows[2];
        let page = Page::from_overfetch(rows, 3, |id| Cursor(*id));

        assert_eq!(page.len(), 3);
        assert_eq!(page.next, Some(Cursor(last_returned)));
    }

    #[test]
    fn a_short_page_is_the_last_page() {
        let page = Page::from_overfetch(ids(2), 3, |id| Cursor(*id));
        assert_eq!(page.len(), 2);
        assert!(page.next.is_none(), "there is nothing after a short page");
    }

    #[test]
    fn an_exactly_full_page_is_also_the_last_page() {
        // The boundary that produces a phantom empty page if it is got wrong: with
        // limit + 1 fetched, exactly `limit` rows means there was no extra row.
        let page = Page::from_overfetch(ids(3), 3, |id| Cursor(*id));
        assert_eq!(page.len(), 3);
        assert!(page.next.is_none());
    }

    #[test]
    fn page_size_is_clamped_at_both_ends() {
        assert_eq!(page_size(None), DEFAULT_PAGE);
        assert_eq!(page_size(Some(0)), 1, "a zero-row page is an infinite loop");
        assert_eq!(page_size(Some(-5)), 1);
        assert_eq!(page_size(Some(10_000)), MAX_PAGE);
    }
}
