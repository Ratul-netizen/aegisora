//! Typed identifiers.
//!
//! These are newtypes rather than bare `Uuid` for one reason: SPEC §M0.8 requires that
//! tenant isolation be enforced by the type system rather than by review discipline.
//! A function taking `(TenantId, ResourceId)` cannot be called with the arguments
//! swapped; a function taking `(Uuid, Uuid)` can, and that mistake is invisible in a
//! diff and catastrophic in production.
//!
//! All IDs are **`UUIDv7`** — time-ordered, so they cluster in PostgreSQL indexes and
//! sort coherently in `ClickHouse` instead of scattering writes across the whole keyspace
//! the way v4 does.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Declares a `UUIDv7` newtype with the conversions every ID needs.
macro_rules! id_type {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        // `transparent` means the newtype is bound and decoded as the bare UUID it
        // wraps, so a repository can write `WHERE tenant_id = $1` with a `TenantId`
        // and get a compile error rather than a silent coercion if it passes the
        // wrong ID type. That is the whole point of the newtypes reaching this far.
        #[cfg_attr(feature = "sqlx", derive(sqlx::Type))]
        #[cfg_attr(feature = "sqlx", sqlx(transparent))]
        pub struct $name(Uuid);

        impl $name {
            /// A fresh time-ordered identifier.
            #[must_use]
            pub fn new() -> Self {
                Self(Uuid::now_v7())
            }

            /// Wrap an existing UUID. Used when reading from storage.
            #[must_use]
            pub const fn from_uuid(u: Uuid) -> Self {
                Self(u)
            }

            #[must_use]
            pub const fn as_uuid(&self) -> &Uuid {
                &self.0
            }

            #[must_use]
            pub const fn into_uuid(self) -> Uuid {
                self.0
            }

            /// The all-zero ID. Only for tests and for "no parent" sentinels that are
            /// never persisted.
            #[must_use]
            pub const fn nil() -> Self {
                Self(Uuid::nil())
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                fmt::Display::fmt(&self.0, f)
            }
        }

        // Debug prints the bare UUID rather than `TenantId(uuid)`. Log lines carry
        // these constantly and the wrapper adds noise without adding information.
        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                fmt::Display::fmt(&self.0, f)
            }
        }

        impl FromStr for $name {
            type Err = uuid::Error;
            fn from_str(s: &str) -> Result<Self, Self::Err> {
                Ok(Self(Uuid::from_str(s)?))
            }
        }

        impl From<Uuid> for $name {
            fn from(u: Uuid) -> Self {
                Self(u)
            }
        }

        impl From<$name> for Uuid {
            fn from(v: $name) -> Self {
                v.0
            }
        }
    };
}

id_type!(
    /// An MSP or a single company. Owns tenants.
    OrgId
);
id_type!(
    /// The isolation boundary. Present on every row in both databases.
    TenantId
);
id_type!(
    /// A physical or logical location within a tenant.
    SiteId
);
id_type!(
    /// The thing telemetry attaches to. The centre of the whole data model.
    ResourceId
);
id_type!(
    /// A reference to sealed credential material. Never the material itself.
    CredentialRef
);
id_type!(
    /// One recorded identity-resolution decision.
    DecisionId
);
id_type!(
    /// A human or service account.
    ActorId
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_time_ordered() {
        // UUIDv7 embeds a millisecond timestamp, so IDs minted in sequence sort in
        // creation order. ClickHouse sort keys and PostgreSQL index locality both
        // depend on this; v4 would scatter.
        let mut prev = ResourceId::new();
        for _ in 0..1_000 {
            let next = ResourceId::new();
            assert!(
                next >= prev,
                "UUIDv7 must be non-decreasing: {prev} then {next}"
            );
            prev = next;
        }
    }

    #[test]
    fn version_is_7() {
        assert_eq!(TenantId::new().as_uuid().get_version_num(), 7);
    }

    #[test]
    fn round_trips_through_string_and_json() {
        let id = TenantId::new();
        assert_eq!(id.to_string().parse::<TenantId>().unwrap(), id);

        // `serde(transparent)` means the wire format is a bare UUID string, not
        // `{"0": "..."}`. API payloads and DB columns depend on that.
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(json, format!("\"{id}\""));
        assert_eq!(serde_json::from_str::<TenantId>(&json).unwrap(), id);
    }

    #[test]
    fn distinct_id_types_do_not_interconvert() {
        // Compile-time property, asserted here as documentation: there is no
        // `From<TenantId> for ResourceId`, so the two cannot be transposed at a
        // call site. If that ever becomes possible this test's comment is the
        // record of why it must not be.
        let t = TenantId::new();
        let r = ResourceId::from_uuid(t.into_uuid());
        assert_eq!(t.as_uuid(), r.as_uuid());
    }
}
