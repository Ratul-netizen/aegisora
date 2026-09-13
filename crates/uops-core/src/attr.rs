//! Attributes, keyed by OpenTelemetry semantic conventions.
//!
//! SPEC §M0.3 requires semconv naming (`server.address`, `network.protocol.name`,
//! `source.address`) rather than a bespoke schema — it costs nothing now and buys
//! interoperability with the entire `OTel` ecosystem later.
//!
//! `BTreeMap` rather than `HashMap`: serialisation must be deterministic so that
//! identical telemetry produces identical bytes. Golden tests, content hashing and
//! stable diffs all depend on it.

use std::borrow::Cow;
use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// An attribute value. Deliberately small — attributes are metadata, not payload.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum AttrValue {
    Str(String),
    Int(i64),
    Float(f64),
    Bool(bool),
}

impl AttrValue {
    /// Rendering for storage backends that hold attributes as `Map(String, String)`.
    #[must_use]
    pub fn as_storage_string(&self) -> Cow<'_, str> {
        match self {
            Self::Str(s) => Cow::Borrowed(s),
            Self::Int(i) => Cow::Owned(i.to_string()),
            Self::Float(f) => Cow::Owned(f.to_string()),
            Self::Bool(b) => Cow::Borrowed(if *b { "true" } else { "false" }),
        }
    }
}

impl From<&str> for AttrValue {
    fn from(v: &str) -> Self {
        Self::Str(v.to_owned())
    }
}
impl From<String> for AttrValue {
    fn from(v: String) -> Self {
        Self::Str(v)
    }
}
impl From<i64> for AttrValue {
    fn from(v: i64) -> Self {
        Self::Int(v)
    }
}
impl From<f64> for AttrValue {
    fn from(v: f64) -> Self {
        Self::Float(v)
    }
}
impl From<bool> for AttrValue {
    fn from(v: bool) -> Self {
        Self::Bool(v)
    }
}

/// Attribute keys are static strings in the overwhelming majority of cases (they come
/// from the semconv constants below), so `Cow` avoids an allocation per attribute per
/// telemetry record. At 50k messages/second that is the difference between a rounding
/// error and a bottleneck.
pub type AttrKey = Cow<'static, str>;

/// An ordered attribute map.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AttrMap(BTreeMap<AttrKey, AttrValue>);

impl AttrMap {
    #[must_use]
    pub fn new() -> Self {
        Self(BTreeMap::new())
    }

    pub fn insert(&mut self, key: impl Into<AttrKey>, value: impl Into<AttrValue>) {
        self.0.insert(key.into(), value.into());
    }

    /// Builder form, for constructing envelopes inline.
    #[must_use]
    pub fn with(mut self, key: impl Into<AttrKey>, value: impl Into<AttrValue>) -> Self {
        self.insert(key, value);
        self
    }

    #[must_use]
    pub fn get(&self, key: &str) -> Option<&AttrValue> {
        self.0.get(key)
    }

    /// Convenience for the common case of reading a string-valued attribute.
    #[must_use]
    pub fn get_str(&self, key: &str) -> Option<&str> {
        match self.0.get(key) {
            Some(AttrValue::Str(s)) => Some(s.as_str()),
            _ => None,
        }
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = (&AttrKey, &AttrValue)> {
        self.0.iter()
    }
}

impl<'a> IntoIterator for &'a AttrMap {
    type Item = (&'a AttrKey, &'a AttrValue);
    type IntoIter = std::collections::btree_map::Iter<'a, AttrKey, AttrValue>;
    fn into_iter(self) -> Self::IntoIter {
        self.0.iter()
    }
}

/// OpenTelemetry semantic-convention keys used across the platform.
///
/// Constants rather than string literals so a typo is a compile error, and so the set
/// of keys we actually rely on is enumerable — which matters because SPEC §M0.6
/// requires the frequently-grouped ones to be promoted to materialized `ClickHouse`
/// columns. That list is derived from this one.
pub mod semconv {
    pub const HOST_NAME: &str = "host.name";
    pub const HOST_ID: &str = "host.id";
    pub const SERVICE_NAME: &str = "service.name";
    pub const SERVICE_VERSION: &str = "service.version";

    pub const SERVER_ADDRESS: &str = "server.address";
    pub const SERVER_PORT: &str = "server.port";
    pub const CLIENT_ADDRESS: &str = "client.address";
    pub const SOURCE_ADDRESS: &str = "source.address";
    pub const SOURCE_PORT: &str = "source.port";
    pub const DESTINATION_ADDRESS: &str = "destination.address";
    pub const DESTINATION_PORT: &str = "destination.port";
    pub const NETWORK_PROTOCOL_NAME: &str = "network.protocol.name";
    pub const NETWORK_INTERFACE_NAME: &str = "network.interface.name";

    pub const EVENT_CATEGORY: &str = "event.category";
    pub const EVENT_NAME: &str = "event.name";

    /// Set by the pipeline when a message could not be parsed. The raw bytes are kept
    /// as the body rather than dropped — the malformed messages are exactly the ones
    /// that matter during an incident.
    pub const PARSE_ERROR: &str = "parse.error";

    /// Attributes promoted to real `ClickHouse` columns because they are routinely
    /// grouped on. W1 measured `GROUP BY attributes['host.name']` at 2252ms over 100M
    /// rows — worse than phrase search — because Map columns decompress in full per row.
    pub const MATERIALIZED: &[&str] = &[HOST_NAME, SERVICE_NAME];
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serialisation_is_deterministic() {
        // Same content inserted in different orders must produce identical bytes.
        // Golden tests and content hashing depend on this; a HashMap would break it.
        let a = AttrMap::new()
            .with(semconv::HOST_NAME, "rtr-01")
            .with(semconv::SERVICE_NAME, "core")
            .with(semconv::SOURCE_PORT, 443_i64);
        let b = AttrMap::new()
            .with(semconv::SOURCE_PORT, 443_i64)
            .with(semconv::SERVICE_NAME, "core")
            .with(semconv::HOST_NAME, "rtr-01");

        assert_eq!(
            serde_json::to_string(&a).unwrap(),
            serde_json::to_string(&b).unwrap()
        );
    }

    #[test]
    fn typed_access_does_not_coerce() {
        let m = AttrMap::new()
            .with(semconv::HOST_NAME, "rtr-01")
            .with(semconv::SOURCE_PORT, 443_i64);
        assert_eq!(m.get_str(semconv::HOST_NAME), Some("rtr-01"));
        // An int-valued attribute is not silently rendered as a string on read.
        assert_eq!(m.get_str(semconv::SOURCE_PORT), None);
        assert_eq!(
            m.get(semconv::SOURCE_PORT).unwrap().as_storage_string(),
            "443"
        );
    }

    #[test]
    fn materialized_keys_are_real_semconv_keys() {
        // Guards against the materialized list drifting from the constants when
        // someone renames one.
        for key in semconv::MATERIALIZED {
            assert!(key.contains('.'), "{key} is not a semconv-shaped key");
        }
        assert!(semconv::MATERIALIZED.contains(&semconv::HOST_NAME));
    }

    #[test]
    fn transparent_representation_is_a_plain_object() {
        let m = AttrMap::new().with(semconv::HOST_NAME, "x");
        assert_eq!(serde_json::to_string(&m).unwrap(), r#"{"host.name":"x"}"#);
    }
}
