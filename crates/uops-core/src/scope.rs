//! `TenantScope` — tenant isolation enforced by the type system.
//!
//! SPEC §M0.8 is explicit that this must not rely on "remember to add
//! `WHERE tenant_id = $1`". Every query builder and repository function takes a
//! `&TenantScope`, and a `TenantScope` cannot be constructed from nothing — so a
//! missing tenant filter is a compile error rather than a silent cross-tenant read.
//!
//! The constructors are deliberately few, deliberately named, and deliberately
//! greppable:
//!
//! | constructor | when |
//! |---|---|
//! | [`TenantScope::from_authenticated`] | normal path, from a verified session |
//! | [`TenantScope::system`] | background workers with no user (pollers, alert eval) |
//! | [`TenantScope::for_test`] | `#[cfg(test)]` only — cannot exist in a release build |

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::ids::{ActorId, TenantId};

/// What a user may do within one tenant — SPEC §M1 RBAC.
///
/// Three, and resist adding more until a customer asks. The role is per
/// `(user, tenant)`, so one MSP engineer is admin on one customer and viewer on another
/// with a single account.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "sqlx", derive(sqlx::Type))]
#[cfg_attr(
    feature = "sqlx",
    sqlx(type_name = "tenant_role", rename_all = "snake_case")
)]
pub enum Role {
    /// Read resources, telemetry and dashboards.
    Viewer,
    /// Also acknowledge alerts, resolve identity review, edit resources and rules.
    Operator,
    /// Also manage credentials, users, roles and tenants, and read the audit log.
    Admin,
}

impl Role {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Viewer => "viewer",
            Self::Operator => "operator",
            Self::Admin => "admin",
        }
    }

    /// Whether this role includes everything `needed` allows.
    ///
    /// The roles are ordered, which is why this is a comparison rather than a matrix.
    /// If a future role stops being a superset of the one below it, this becomes a
    /// lookup table and the `Ord` derive comes off.
    #[must_use]
    pub fn allows(self, needed: Self) -> bool {
        self >= needed
    }

    /// Managing credentials, users and roles, and reading the audit log.
    #[must_use]
    pub fn is_admin(self) -> bool {
        self == Self::Admin
    }
}

/// Who is acting. Recorded on every audit and access-log row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Actor {
    /// A human or service account.
    User(ActorId),
    /// A collector running without a user session.
    Collector,
    /// The platform itself — alert evaluation, retention, migrations.
    System,
}

impl Actor {
    #[must_use]
    pub fn as_audit_str(self) -> String {
        match self {
            Self::User(id) => format!("user:{id}"),
            Self::Collector => "collector".to_owned(),
            Self::System => "system".to_owned(),
        }
    }
}

/// Proof that the caller is entitled to act within a tenant.
///
/// Not `Copy`: it should be passed by reference and threaded through, so that a
/// function which needs tenant access has to say so in its signature.
///
/// ```
/// # use uops_core::{TenantScope, TenantId};
/// let tenant = TenantId::new();
/// let scope = TenantScope::system(tenant);
/// assert_eq!(scope.tenant_id(), tenant);
/// ```
///
/// # Enforced negatives
///
/// Asserted here rather than beside the unit tests because rustdoc does not collect
/// doctests from private items in `#[cfg(test)]` modules.
///
/// Not `Display`, so it cannot be interpolated into a query string — precisely the
/// mistake this type exists to prevent:
///
/// ```compile_fail
/// # use uops_core::{TenantScope, TenantId};
/// let s = TenantScope::system(TenantId::nil());
/// let _ = format!("WHERE tenant = {}", s);
/// ```
///
/// No `Default` — a scope must always come from somewhere real:
///
/// ```compile_fail
/// # use uops_core::TenantScope;
/// let _ = TenantScope::default();
/// ```
#[derive(Clone)]
pub struct TenantScope {
    tenant_id: TenantId,
    actor: Actor,
}

impl TenantScope {
    /// Build a scope from a verified session.
    ///
    /// The only non-test way to scope to a specific user. The caller is asserting that
    /// authentication has already succeeded and that this actor belongs to this tenant
    /// — which is why it lives behind the API's auth extractor and nowhere else.
    #[must_use]
    pub const fn from_authenticated(tenant_id: TenantId, actor: ActorId) -> Self {
        Self {
            tenant_id,
            actor: Actor::User(actor),
        }
    }

    /// For background work with no user: pollers, alert evaluation, retention.
    ///
    /// Still tenant-bound. There is no "all tenants" scope, because a cross-tenant
    /// query should never be expressible; anything that genuinely must span tenants
    /// iterates over them explicitly and visibly.
    #[must_use]
    pub const fn system(tenant_id: TenantId) -> Self {
        Self {
            tenant_id,
            actor: Actor::System,
        }
    }

    /// A collector ingesting on behalf of a tenant.
    #[must_use]
    pub const fn collector(tenant_id: TenantId) -> Self {
        Self {
            tenant_id,
            actor: Actor::Collector,
        }
    }

    /// Test-only constructor.
    ///
    /// `#[cfg(test)]` rather than a feature flag, so it cannot be reached from a
    /// release build even by accident.
    #[cfg(test)]
    #[must_use]
    pub const fn for_test(tenant_id: TenantId) -> Self {
        Self {
            tenant_id,
            actor: Actor::System,
        }
    }

    #[must_use]
    pub const fn tenant_id(&self) -> TenantId {
        self.tenant_id
    }

    #[must_use]
    pub const fn actor(&self) -> Actor {
        self.actor
    }
}

/// Prints the tenant and actor but is not `Display`, so a scope cannot be interpolated
/// into a SQL string by accident.
impl fmt::Debug for TenantScope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TenantScope")
            .field("tenant_id", &self.tenant_id)
            .field("actor", &self.actor)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scope_carries_tenant_and_actor() {
        let t = TenantId::new();
        let a = ActorId::new();
        let s = TenantScope::from_authenticated(t, a);
        assert_eq!(s.tenant_id(), t);
        assert_eq!(s.actor(), Actor::User(a));
    }

    #[test]
    fn actor_renders_for_the_audit_log() {
        let a = ActorId::new();
        assert_eq!(
            TenantScope::from_authenticated(TenantId::new(), a)
                .actor()
                .as_audit_str(),
            format!("user:{a}")
        );
        assert_eq!(Actor::System.as_audit_str(), "system");
        assert_eq!(Actor::Collector.as_audit_str(), "collector");
    }

    // The negative invariants (not Display, no Default) are asserted as `compile_fail`
    // doctests on `TenantScope` itself — see the note there.
}
