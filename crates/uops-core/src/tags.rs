//! Operator tags — what a human decided about a resource.
//!
//! Deliberately a different type from [`AttrMap`](crate::AttrMap), and the distinction is
//! the whole point:
//!
//! | | written by | read by |
//! |---|---|---|
//! | `attributes` | collectors, on every walk | humans, alerts, queries |
//! | `tags` | humans | collectors never; everything else |
//!
//! One map for both would work right up until the first time discovery overwrote
//! `criticality=critical`. It would, silently, and the resulting alert-routing bug would
//! be unreproducible because the evidence would have been overwritten too.
//!
//! The precedent was already in the schema and was already right: `Resource::name` is
//! system-chosen and `Resource::display_name` is the human override, *"never written
//! automatically — if a human named it, discovery must not silently rename it underneath
//! them."* Tags are that argument applied to attributes.
//!
//! # Why a separate type and not just another `AttrMap`
//!
//! Because the rule *"collectors never write tags"* has to be enforceable somewhere, and
//! the type system is the only place it can be enforced cheaply. A collector that holds
//! an `AttrMap` cannot accidentally pass it where a `Tags` is wanted, so the mistake is a
//! compile error rather than a silent overwrite found six months later by an operator
//! whose paging rule stopped firing.
//!
//! # Flat, and string-valued
//!
//! `environment=production`, not `owner={team: network}`. The database refuses anything
//! else (`resource_tags_are_flat_strings`, migration 0011) and so does this type, for the
//! same reason: a routing rule that silently ignores `owner.team` because it is an object
//! is not a debugging session anybody should have.
//!
//! Values are strings, not the four-way `AttrValue`. A tag is a label an operator typed;
//! `criticality=1` and `criticality="1"` being different things would be a distinction
//! nobody asked for and everybody would trip over.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Keys an operator is likely to want, offered as constants so that the obvious ones are
/// spelled the same way in every dashboard, alert rule and routing policy.
///
/// **Not enforced.** A tag vocabulary that refuses unknown keys is a tag vocabulary that
/// refuses the one an operator actually needs, at 3am, from a locked-down UI. These exist
/// so that the common cases converge, not so that the uncommon ones are impossible.
pub mod well_known {
    /// `production`, `staging`, `development`.
    pub const ENVIRONMENT: &str = "environment";
    /// `critical`, `high`, `normal`, `low`. Drives alert routing and escalation.
    pub const CRITICALITY: &str = "criticality";
    /// The team or person responsible. `network-team`, `dba`.
    pub const OWNER: &str = "owner";
    /// The business unit, for cost and reporting rollups.
    pub const DEPARTMENT: &str = "department";
    /// A customer, for an MSP whose tenants are not one-per-customer.
    pub const CUSTOMER: &str = "customer";
    /// Free-form, for the estate's own vocabulary. `dhaka-office`, `rack-4`.
    pub const LOCATION: &str = "location";
}

/// The largest a single key or value may be.
///
/// Tags are labels, not payload. A cap makes the jsonb column's size predictable and
/// stops somebody storing a runbook in one — which would then be copied into every alert
/// notification that resource ever produces.
pub const MAX_LEN: usize = 256;

/// How many tags one resource may carry.
///
/// Generous for labelling and far short of the point where the GIN index or a tag
/// selector's containment query becomes something to think about.
pub const MAX_TAGS: usize = 64;

/// What is wrong with a tag.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum TagError {
    #[error("a tag key must not be empty")]
    EmptyKey,
    #[error("tag key {key:?} is {len} bytes; the limit is {MAX_LEN}")]
    KeyTooLong { key: String, len: usize },
    #[error("the value of tag {key:?} is {len} bytes; the limit is {MAX_LEN}")]
    ValueTooLong { key: String, len: usize },
    #[error("a tag key must not contain leading or trailing whitespace: {key:?}")]
    UntrimmedKey { key: String },
    #[error("{count} tags; the limit is {MAX_TAGS}")]
    TooMany { count: usize },
}

/// An operator-managed label map.
///
/// `BTreeMap` for the same reason `AttrMap` uses one: deterministic serialisation, so
/// that a golden test and a stable diff are both possible.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Tags(BTreeMap<String, String>);

impl Tags {
    #[must_use]
    pub fn new() -> Self {
        Self(BTreeMap::new())
    }

    /// Builder form.
    ///
    /// Does **not** validate — [`validate`](Self::validate) does, and the repository
    /// calls it before writing. Building an invalid map in a test and asserting the
    /// rejection is the point; making the builder fallible would make every call site
    /// carry a `Result` for a value that is usually a literal.
    #[must_use]
    pub fn with(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.0.insert(key.into(), value.into());
        self
    }

    pub fn insert(&mut self, key: impl Into<String>, value: impl Into<String>) {
        self.0.insert(key.into(), value.into());
    }

    pub fn remove(&mut self, key: &str) -> Option<String> {
        self.0.remove(key)
    }

    #[must_use]
    pub fn get(&self, key: &str) -> Option<&str> {
        self.0.get(key).map(String::as_str)
    }

    #[must_use]
    pub fn contains(&self, key: &str, value: &str) -> bool {
        self.get(key) == Some(value)
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = (&String, &String)> {
        self.0.iter()
    }

    /// Check every key and value.
    ///
    /// # Errors
    ///
    /// The first problem found. One at a time rather than a list, because the caller is a
    /// human editing one resource, and a form that reports one fixable thing is more
    /// useful than one that reports five.
    pub fn validate(&self) -> Result<(), TagError> {
        if self.0.len() > MAX_TAGS {
            return Err(TagError::TooMany {
                count: self.0.len(),
            });
        }
        for (key, value) in &self.0 {
            if key.is_empty() {
                return Err(TagError::EmptyKey);
            }
            if key.trim() != key {
                // Refused rather than trimmed. ` owner` and `owner` would otherwise be
                // two tags that look identical in every UI, and the routing rule matching
                // one of them would be right about half the time.
                return Err(TagError::UntrimmedKey { key: key.clone() });
            }
            if key.len() > MAX_LEN {
                return Err(TagError::KeyTooLong {
                    key: key.clone(),
                    len: key.len(),
                });
            }
            if value.len() > MAX_LEN {
                return Err(TagError::ValueTooLong {
                    key: key.clone(),
                    len: value.len(),
                });
            }
        }
        Ok(())
    }
}

impl<'a> IntoIterator for &'a Tags {
    type Item = (&'a String, &'a String);
    type IntoIter = std::collections::btree_map::Iter<'a, String, String>;
    fn into_iter(self) -> Self::IntoIter {
        self.0.iter()
    }
}

impl FromIterator<(String, String)> for Tags {
    fn from_iter<I: IntoIterator<Item = (String, String)>>(iter: I) -> Self {
        Self(iter.into_iter().collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tag_map_serialises_as_a_flat_object() {
        // The database column is jsonb with a CHECK that every value is a string
        // (migration 0011). This type has to produce exactly that shape or the write
        // fails at runtime instead of at compile time.
        let tags = Tags::new()
            .with(well_known::ENVIRONMENT, "production")
            .with(well_known::CRITICALITY, "critical");
        let json = serde_json::to_string(&tags).expect("serialise");
        assert_eq!(
            json, r#"{"criticality":"critical","environment":"production"}"#,
            "flat, string-valued, and ordered"
        );
    }

    #[test]
    fn keys_are_ordered_so_that_serialisation_is_deterministic() {
        // Golden tests and stable diffs both depend on it, which is the same reason
        // AttrMap uses a BTreeMap.
        let one = Tags::new().with("b", "2").with("a", "1");
        let two = Tags::new().with("a", "1").with("b", "2");
        assert_eq!(
            serde_json::to_string(&one).expect("one"),
            serde_json::to_string(&two).expect("two")
        );
    }

    #[test]
    fn an_untrimmed_key_is_refused_rather_than_trimmed() {
        // ` owner` and `owner` would be two tags that look identical in every UI, and a
        // routing rule matching one of them would be right about half the time.
        let tags = Tags::new().with(" owner", "network-team");
        assert!(matches!(
            tags.validate(),
            Err(TagError::UntrimmedKey { .. })
        ));
    }

    #[test]
    fn an_empty_key_is_refused() {
        assert_eq!(
            Tags::new().with("", "x").validate(),
            Err(TagError::EmptyKey)
        );
    }

    #[test]
    fn a_value_long_enough_to_be_a_runbook_is_refused() {
        // Without the cap, a tag becomes somewhere to put prose — and that prose is then
        // copied into every alert notification the resource ever produces.
        let tags = Tags::new().with("note", "x".repeat(MAX_LEN + 1));
        assert!(matches!(
            tags.validate(),
            Err(TagError::ValueTooLong { .. })
        ));
        assert!(
            Tags::new()
                .with("note", "x".repeat(MAX_LEN))
                .validate()
                .is_ok()
        );
    }

    #[test]
    fn too_many_tags_are_refused() {
        let mut tags = Tags::new();
        for i in 0..=MAX_TAGS {
            tags.insert(format!("k{i}"), "v");
        }
        assert!(matches!(tags.validate(), Err(TagError::TooMany { .. })));
    }

    #[test]
    fn the_empty_map_is_valid() {
        // The default for every resource, so it had better be.
        assert!(Tags::new().validate().is_ok());
        assert!(Tags::new().is_empty());
    }

    #[test]
    fn contains_is_the_shape_a_selector_asks_in() {
        // `tagged environment=production` is a containment question, which is what the
        // jsonb_path_ops GIN index is for on the database side.
        let tags = Tags::new().with("environment", "production");
        assert!(tags.contains("environment", "production"));
        assert!(!tags.contains("environment", "staging"));
        assert!(!tags.contains("criticality", "production"));
    }
}
