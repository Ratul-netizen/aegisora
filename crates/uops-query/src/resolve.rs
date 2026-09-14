//! Resource resolution — SPEC §M0.5 requirement 2.
//!
//! `ResourceSelector` is expanded through `resource_alias` **before** SQL generation,
//! never during it. Two reasons, and the second is the one that shaped the API:
//!
//!   1. Alias collapse is a `PostgreSQL` question, and the telemetry query is a
//!      `ClickHouse` one. Resolving inside codegen would put a control-plane round trip
//!      in the middle of building a string.
//!   2. Compilation stays **pure and synchronous**. Resolution is a database round trip
//!      and [`ResourceCatalog`] is therefore async, but [`crate::compile`] takes the
//!      *result* — so the golden tests compile real queries with no database, which is
//!      what makes them cheap enough to run on every commit.
//!
//! So [`ResolvedResources`] is the currency: it can only be produced by resolving a
//! selector under a scope, it remembers which tenant it was resolved for, and
//! [`crate::compile`] refuses to use one that does not match the scope it was handed.

use std::collections::BTreeSet;

use async_trait::async_trait;
use uops_core::{ResourceId, ResourceKind, SiteId, TenantId, TenantScope};

use crate::ast::ResourceSelector;
use crate::error::{Error, Result};

/// The control-plane lookups resolution needs. Implemented over `PostgreSQL` in M1;
/// implemented in memory by tests.
///
/// Every method takes the tenant, and implementations must filter on it. This trait is
/// the boundary where type-level tenant safety hands over to SQL that a human wrote, so
/// it is worth stating plainly: **these five queries are where a missing `tenant_id`
/// would actually hurt.**
///
/// Async because every implementation is a database: the `PostgreSQL` one in
/// `uops-store-pg` is four `sqlx` queries. A synchronous trait would force that
/// implementation to block a runtime thread on I/O, which at ingest rates is how a
/// collector stalls.
#[async_trait]
pub trait ResourceCatalog: Sync {
    /// Collapse aliases to canonical resource IDs, dropping any ID that does not belong
    /// to this tenant. Dropping rather than erroring is deliberate — see
    /// `unknown_ids_are_dropped_not_reported` in the tests.
    async fn canonical(&self, tenant: TenantId, ids: &[ResourceId]) -> Result<Vec<ResourceId>>;

    async fn of_kind(&self, tenant: TenantId, kind: ResourceKind) -> Result<Vec<ResourceId>>;

    async fn at_site(&self, tenant: TenantId, site: SiteId) -> Result<Vec<ResourceId>>;

    /// `resource_dependents()`, bounded by `max_depth`.
    async fn descendants(
        &self,
        tenant: TenantId,
        root: ResourceId,
        max_depth: u8,
    ) -> Result<Vec<ResourceId>>;
}

/// A resolved, tenant-bound resource set. Only [`resolve`] can produce one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedResources {
    tenant: TenantId,
    /// `None` means "every resource in the tenant" — no `resource_id` predicate.
    /// `Some(empty)` means "nothing matched", which is a completely different query.
    ids: Option<Vec<ResourceId>>,
}

impl ResolvedResources {
    #[must_use]
    pub const fn tenant(&self) -> TenantId {
        self.tenant
    }

    #[must_use]
    pub fn ids(&self) -> Option<&[ResourceId]> {
        self.ids.as_deref()
    }

    /// Whether this resolved to nothing at all.
    #[must_use]
    pub fn is_empty_set(&self) -> bool {
        self.ids.as_ref().is_some_and(Vec::is_empty)
    }

    /// Whether this spans the whole tenant.
    #[must_use]
    pub const fn is_whole_tenant(&self) -> bool {
        self.ids.is_none()
    }

    /// The whole tenant, without consulting the catalog.
    ///
    /// Safe to expose because [`ResourceSelector::All`] has nothing to resolve: it
    /// emits no `resource_id` predicate at all, so there is no alias chain to collapse
    /// and no chance of widening a narrower selector by accident. It still carries the
    /// tenant, so it is still refused under another scope.
    #[must_use]
    pub fn whole_tenant(scope: &TenantScope) -> Self {
        Self {
            tenant: scope.tenant_id(),
            ids: None,
        }
    }

    /// A set that was resolved somewhere else.
    ///
    /// Named to make the bypass visible: this does **not** collapse aliases, so a
    /// caller using it is asserting that expansion already happened — a cached set from
    /// a previous `resolve`, or a test that is not about resolution. Reaching for it to
    /// avoid a database round trip on a fresh selector is how a merged-away resource
    /// stops resolving.
    #[must_use]
    pub fn already_resolved(scope: &TenantScope, ids: Vec<ResourceId>) -> Self {
        let ids: Vec<ResourceId> = ids
            .into_iter()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        Self {
            tenant: scope.tenant_id(),
            ids: Some(ids),
        }
    }

    /// Test-only shortcut for compiler tests that are not about resolution.
    #[cfg(test)]
    pub(crate) fn for_test(tenant: TenantId, ids: Option<Vec<ResourceId>>) -> Self {
        Self { tenant, ids }
    }
}

/// Expand a selector into concrete resource IDs, within one tenant.
pub async fn resolve(
    selector: &ResourceSelector,
    scope: &TenantScope,
    catalog: &impl ResourceCatalog,
) -> Result<ResolvedResources> {
    let tenant = scope.tenant_id();

    let raw = match selector {
        ResourceSelector::All => return Ok(ResolvedResources { tenant, ids: None }),
        ResourceSelector::Ids { ids } => catalog.canonical(tenant, ids).await?,
        ResourceSelector::Kind { kind } => catalog.of_kind(tenant, *kind).await?,
        ResourceSelector::Site { site } => catalog.at_site(tenant, *site).await?,
        ResourceSelector::Descendants { root, max_depth } => {
            if *max_depth == 0 {
                return Err(Error::Invalid(
                    "descendants requires max_depth >= 1; an unbounded walk over a topology \
                     graph does not terminate"
                        .into(),
                ));
            }
            catalog.descendants(tenant, *root, *max_depth).await?
        }
    };

    // Sorted and deduplicated: the sort key leads with resource_id, so a sorted IN list
    // reads contiguous ranges, and deduplication keeps an alias chain that collapses
    // onto one canonical resource from asking for it twice.
    let ids: Vec<ResourceId> = raw
        .into_iter()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();

    Ok(ResolvedResources {
        tenant,
        ids: Some(ids),
    })
}

#[cfg(test)]
pub(crate) mod testing {
    use std::collections::HashMap;

    use async_trait::async_trait;

    use super::{ResourceCatalog, ResourceId, ResourceKind, Result, SiteId, TenantId};

    /// An in-memory catalog. Also the shape the `PostgreSQL` implementation must match.
    #[derive(Debug, Default)]
    pub(crate) struct FakeCatalog {
        pub tenant: Option<TenantId>,
        /// alias → canonical
        pub aliases: HashMap<ResourceId, ResourceId>,
        pub members: Vec<ResourceId>,
        pub tree: HashMap<ResourceId, Vec<ResourceId>>,
    }

    impl FakeCatalog {
        pub(crate) fn new(tenant: TenantId) -> Self {
            Self {
                tenant: Some(tenant),
                ..Self::default()
            }
        }

        fn mine(&self, tenant: TenantId) -> bool {
            self.tenant.is_none_or(|t| t == tenant)
        }
    }

    #[async_trait]
    impl ResourceCatalog for FakeCatalog {
        async fn canonical(&self, tenant: TenantId, ids: &[ResourceId]) -> Result<Vec<ResourceId>> {
            if !self.mine(tenant) {
                return Ok(Vec::new());
            }
            Ok(ids
                .iter()
                .filter_map(|id| {
                    let c = self.aliases.get(id).copied().unwrap_or(*id);
                    self.members.contains(&c).then_some(c)
                })
                .collect())
        }

        async fn of_kind(&self, tenant: TenantId, _kind: ResourceKind) -> Result<Vec<ResourceId>> {
            Ok(if self.mine(tenant) {
                self.members.clone()
            } else {
                Vec::new()
            })
        }

        async fn at_site(&self, tenant: TenantId, _site: SiteId) -> Result<Vec<ResourceId>> {
            self.of_kind(tenant, ResourceKind::Device).await
        }

        async fn descendants(
            &self,
            tenant: TenantId,
            root: ResourceId,
            max_depth: u8,
        ) -> Result<Vec<ResourceId>> {
            let mut out = vec![root];
            let mut frontier = vec![root];
            for _ in 0..max_depth {
                let mut next = Vec::new();
                for node in frontier.drain(..) {
                    for child in self.tree.get(&node).into_iter().flatten() {
                        // The cycle guard is the reason max_depth is not enough on its
                        // own: A → B → A revisits forever within the depth budget.
                        if !out.contains(child) {
                            out.push(*child);
                            next.push(*child);
                        }
                    }
                }
                frontier = next;
            }
            let _ = tenant;
            Ok(out)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testing::FakeCatalog;
    use super::*;

    fn setup() -> (TenantScope, FakeCatalog, Vec<ResourceId>) {
        let tenant = TenantId::new();
        let ids: Vec<ResourceId> = (0..4).map(|_| ResourceId::new()).collect();
        let mut cat = FakeCatalog::new(tenant);
        cat.members = ids.clone();
        (TenantScope::system(tenant), cat, ids)
    }

    #[tokio::test]
    async fn aliases_collapse_before_any_sql_exists() {
        let (scope, mut cat, ids) = setup();
        let alias = ResourceId::new();
        cat.aliases.insert(alias, ids[1]);

        let r = resolve(
            &ResourceSelector::Ids {
                ids: vec![alias, ids[1]],
            },
            &scope,
            &cat,
        )
        .await
        .unwrap();

        // The alias and its canonical target are the same device. Asking for both must
        // not read that device's telemetry twice.
        assert_eq!(r.ids().unwrap(), &[ids[1]]);
    }

    #[tokio::test]
    async fn unknown_ids_are_dropped_not_reported() {
        // A resource ID from another tenant must not produce a distinguishable error:
        // "that ID exists but is not yours" confirms the resource exists, which for an
        // MSP is one customer learning about another's inventory. Same reasoning as
        // uops_core::Error::TenantMismatch returning 404.
        let (scope, cat, ids) = setup();
        let foreign = ResourceId::new();
        let r = resolve(
            &ResourceSelector::Ids {
                ids: vec![ids[0], foreign],
            },
            &scope,
            &cat,
        )
        .await
        .unwrap();
        assert_eq!(r.ids().unwrap(), &[ids[0]]);
    }

    #[tokio::test]
    async fn an_empty_match_is_not_the_same_as_no_filter() {
        // The whole point of the Option: if "matched nothing" degraded into "no
        // resource predicate", a selector that matched nothing would read the entire
        // tenant. That is the most expensive possible answer to the narrowest possible
        // question, and it would look like the query worked.
        let (scope, cat, _) = setup();
        let none = resolve(
            &ResourceSelector::Ids {
                ids: vec![ResourceId::new()],
            },
            &scope,
            &cat,
        )
        .await
        .unwrap();
        assert!(none.is_empty_set() && !none.is_whole_tenant());

        let all = resolve(&ResourceSelector::All, &scope, &cat).await.unwrap();
        assert!(all.is_whole_tenant() && !all.is_empty_set());
    }

    #[tokio::test]
    async fn resolution_is_bound_to_the_tenant_it_ran_under() {
        let (scope, cat, _) = setup();
        let r = resolve(&ResourceSelector::All, &scope, &cat).await.unwrap();
        assert_eq!(r.tenant(), scope.tenant_id());
    }

    #[tokio::test]
    async fn ids_come_back_sorted_so_the_in_list_reads_contiguous_ranges() {
        let (scope, cat, ids) = setup();
        let mut reversed = ids.clone();
        reversed.reverse();
        let r = resolve(&ResourceSelector::Ids { ids: reversed }, &scope, &cat)
            .await
            .unwrap();
        let got = r.ids().unwrap();
        assert!(got.windows(2).all(|w| w[0] < w[1]), "{got:?}");
    }

    #[tokio::test]
    async fn a_cyclic_topology_terminates() {
        let (scope, mut cat, ids) = setup();
        cat.tree.insert(ids[0], vec![ids[1]]);
        cat.tree.insert(ids[1], vec![ids[2], ids[0]]); // back-edge
        cat.tree.insert(ids[2], vec![ids[1]]);

        let r = resolve(
            &ResourceSelector::Descendants {
                root: ids[0],
                max_depth: 8,
            },
            &scope,
            &cat,
        )
        .await
        .unwrap();
        assert_eq!(r.ids().unwrap().len(), 3);
    }

    #[tokio::test]
    async fn depth_zero_is_rejected_rather_than_meaning_unbounded() {
        let (scope, cat, ids) = setup();
        let err = resolve(
            &ResourceSelector::Descendants {
                root: ids[0],
                max_depth: 0,
            },
            &scope,
            &cat,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, Error::Invalid(_)), "{err}");
    }
}
