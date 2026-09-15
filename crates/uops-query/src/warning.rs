//! Warnings returned alongside results — SPEC §M0.5 requirement 4.
//!
//! A warning is not an error. The query runs. It exists because W1 measured several
//! ways for a query to be *correct and slow*, and the user is the only one who can
//! decide whether to narrow it. Silently running a two-second full-tenant scan and
//! silently downsampling to five-minute averages are both worse than saying so.

use std::borrow::Cow;

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "warning")]
#[non_exhaustive]
pub enum QueryWarning {
    /// Substring or phrase search. Measured at 100M rows: `LIKE` 1 928 ms, phrase
    /// 2 378 ms, both reading the whole tenant with no granule pruning.
    NotIndexAccelerated {
        mode: Cow<'static, str>,
        why: Cow<'static, str>,
    },

    /// The requested limit exceeded the server ceiling and was reduced.
    LimitClamped { requested: u32, applied: u32 },

    /// Answered from a rollup because the window is longer than raw retention. Values
    /// are aggregates of aggregates, which matters for how the axis is labelled.
    Downsampled {
        table: Cow<'static, str>,
        bucket_seconds: u32,
    },

    /// Grouped or filtered on a map key that is not a materialised column. W1's slowest
    /// query was exactly this at 2 252 ms — `Map` columns decompress in full per row.
    AttributeNotMaterialised { key: String },

    /// The selector resolved to no resources at all. Emitted because the alternative
    /// reading — "no resource filter, so read everything" — is the dangerous one, and
    /// an empty result with no explanation looks like a broken query.
    SelectorMatchedNothing,

    /// A tail or scan across every resource in the tenant. Served by the `p_by_time`
    /// projection (W1: 2 303 ms → 72 ms), which exists precisely for this shape, but
    /// it is still the widest read the product offers.
    FullTenantScan,
}

/// A warning as the API sends it.
///
/// The tagged variant, flattened, plus the sentence [`QueryWarning::message`] already
/// knows how to write. The message is on the wire rather than reconstructed by each
/// client because the alternative is a switch statement in every one of them, and the
/// day a variant is added those switches do not fail — they fall through to a default
/// and print `not_index_accelerated`, losing the half of the warning that says what to
/// do about it.
///
/// Serialize only. It is an output shape; nothing parses it back.
#[derive(Clone, Debug, Serialize)]
pub struct WarningView {
    #[serde(flatten)]
    pub warning: QueryWarning,
    pub message: String,
}

impl From<QueryWarning> for WarningView {
    fn from(warning: QueryWarning) -> Self {
        Self {
            message: warning.message(),
            warning,
        }
    }
}

impl QueryWarning {
    /// One line, for the UI banner.
    #[must_use]
    pub fn message(&self) -> String {
        match self {
            Self::NotIndexAccelerated { mode, why } => {
                format!("{mode} search cannot use the text index: {why}")
            }
            Self::LimitClamped { requested, applied } => {
                format!("limit reduced from {requested} to the server maximum of {applied}")
            }
            Self::Downsampled {
                table,
                bucket_seconds,
            } => format!(
                "showing pre-aggregated {bucket_seconds}s buckets from {table}; \
                 raw points are past their retention"
            ),
            Self::AttributeNotMaterialised { key } => format!(
                "`{key}` is not a materialised column, so every row must be decompressed \
                 to read it"
            ),
            Self::SelectorMatchedNothing => {
                "the resource selector matched no resources, so this query returns nothing".into()
            }
            Self::FullTenantScan => {
                "this reads every resource in the tenant; narrowing to a resource is much faster"
                    .into()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn warnings_survive_the_api_boundary() {
        let w = QueryWarning::Downsampled {
            table: "metrics_5m".into(),
            bucket_seconds: 300,
        };
        let json = serde_json::to_string(&w).unwrap();
        assert!(json.contains("\"warning\":\"downsampled\""), "{json}");
        assert_eq!(serde_json::from_str::<QueryWarning>(&json).unwrap(), w);
    }

    #[test]
    fn every_warning_says_something_actionable() {
        // A warning the user cannot act on is noise, and noise gets dismissed, which
        // is how the one that mattered gets dismissed too.
        for w in [
            QueryWarning::NotIndexAccelerated {
                mode: "phrase".into(),
                why: "tokens narrow granules, then the phrase is verified by scanning".into(),
            },
            QueryWarning::LimitClamped {
                requested: 1_000_000,
                applied: 10_000,
            },
            QueryWarning::AttributeNotMaterialised {
                key: "custom.tag".into(),
            },
            QueryWarning::SelectorMatchedNothing,
            QueryWarning::FullTenantScan,
        ] {
            let m = w.message();
            assert!(m.len() > 20, "warning too terse to act on: {m}");
        }
    }
}
