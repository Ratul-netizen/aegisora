//! An in-memory `IdentityStore`.
//!
//! Two jobs. It is what the resolver's rules are tested against — exhaustively, and
//! without a database, which is what makes it reasonable to test every threshold and
//! every contradiction rather than a representative sample. And it is the shape the
//! `PostgreSQL` implementation has to match, so a behaviour asserted here is a behaviour
//! the real store owes.
//!
//! It enforces `UNIQUE (tenant_id, kind, value)` the way the schema does, because that
//! constraint is not an optimisation — it is the mechanism resolution rests on, and a
//! fake that ignored it would let the resolver's tests pass on writes the real database
//! rejects.

use std::collections::HashMap;
use std::sync::Mutex;

use async_trait::async_trait;
use uops_core::{Error, Identifier, ResourceId, ResourceKind, Result, TenantId};

use crate::store::{Decision, DecisionOutcome, Hit, IdentityStore, ReviewItem};

#[derive(Debug, Default)]
struct Inner {
    /// `(tenant, kind, value) → resource`, mirroring the unique index.
    identifiers: HashMap<(TenantId, &'static str, String), ResourceId>,
    resources: HashMap<ResourceId, (TenantId, ResourceKind, String)>,
    aliases: HashMap<(TenantId, ResourceId), ResourceId>,
    decisions: Vec<Decision>,
    /// How many times the store was asked to look something up. The resolver's cache
    /// is only worth having if this stops growing, and that is a test, not a claim.
    lookups: u64,
}

/// In-memory identity storage.
#[derive(Debug, Default)]
pub struct MemoryIdentityStore {
    inner: Mutex<Inner>,
}

impl MemoryIdentityStore {
    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// How many times [`IdentityStore::lookup`] has been called.
    #[must_use]
    pub fn lookup_count(&self) -> u64 {
        self.lock().lookups
    }

    /// Every decision recorded, in order.
    #[must_use]
    pub fn decisions(&self) -> Vec<Decision> {
        self.lock().decisions.clone()
    }

    /// Where an alias points, if the resource has been merged away.
    #[must_use]
    pub fn alias_of(&self, tenant: TenantId, historical: ResourceId) -> Option<ResourceId> {
        self.lock().aliases.get(&(tenant, historical)).copied()
    }

    #[must_use]
    pub fn resource_count(&self) -> usize {
        self.lock().resources.len()
    }
}

#[async_trait]
impl IdentityStore for MemoryIdentityStore {
    async fn lookup(&self, tenant: TenantId, identifiers: &[Identifier]) -> Result<Vec<Hit>> {
        let mut inner = self.lock();
        inner.lookups += 1;

        Ok(identifiers
            .iter()
            .filter_map(|i| {
                inner
                    .identifiers
                    .get(&(tenant, i.kind.as_str(), i.value.clone()))
                    .map(|resource_id| Hit {
                        resource_id: *resource_id,
                        identifier: i.clone(),
                    })
            })
            .collect())
    }

    async fn identifiers_of(
        &self,
        tenant: TenantId,
        resource: ResourceId,
    ) -> Result<Vec<Identifier>> {
        let inner = self.lock();
        Ok(inner
            .identifiers
            .iter()
            .filter(|((t, _, _), r)| *t == tenant && **r == resource)
            .filter_map(|((_, kind, value), _)| {
                kind_from_str(kind).map(|k| Identifier::new(k, value.clone()))
            })
            .collect())
    }

    async fn create_provisional(
        &self,
        tenant: TenantId,
        kind: ResourceKind,
        name: &str,
    ) -> Result<ResourceId> {
        let id = ResourceId::new();
        self.lock()
            .resources
            .insert(id, (tenant, kind, name.to_owned()));
        Ok(id)
    }

    async fn attach_identifiers(
        &self,
        tenant: TenantId,
        resource: ResourceId,
        identifiers: &[Identifier],
        _source: &str,
    ) -> Result<()> {
        let mut inner = self.lock();
        for i in identifiers {
            // Insert-or-ignore, exactly like `ON CONFLICT DO NOTHING`: an identifier
            // that already belongs to another resource stays with it. Overwriting here
            // would make every repeated observation silently re-home the device.
            inner
                .identifiers
                .entry((tenant, i.kind.as_str(), i.value.clone()))
                .or_insert(resource);
        }
        Ok(())
    }

    async fn reassign_identifiers(
        &self,
        tenant: TenantId,
        resource: ResourceId,
        identifiers: &[Identifier],
        _source: &str,
    ) -> Result<()> {
        let mut inner = self.lock();
        for i in identifiers {
            inner
                .identifiers
                .insert((tenant, i.kind.as_str(), i.value.clone()), resource);
        }
        Ok(())
    }

    async fn record_decision(&self, decision: &Decision) -> Result<()> {
        self.lock().decisions.push(decision.clone());
        Ok(())
    }

    async fn merge(
        &self,
        tenant: TenantId,
        historical: ResourceId,
        surviving: ResourceId,
        decision: &Decision,
    ) -> Result<()> {
        let mut inner = self.lock();
        if !inner.resources.contains_key(&surviving) {
            return Err(Error::NotFound {
                kind: "resource",
                id: surviving.to_string(),
            });
        }

        for (_, owner) in inner
            .identifiers
            .iter_mut()
            .filter(|((t, _, _), _)| *t == tenant)
        {
            if *owner == historical {
                *owner = surviving;
            }
        }

        // Collapse on write, the way the trigger in 0004_identity.sql does: anything
        // already pointing at the resource being merged away follows it.
        let chained: Vec<(TenantId, ResourceId)> = inner
            .aliases
            .iter()
            .filter(|((t, _), current)| *t == tenant && **current == historical)
            .map(|(k, _)| *k)
            .collect();
        for k in chained {
            inner.aliases.insert(k, surviving);
        }
        inner.aliases.insert((tenant, historical), surviving);
        inner.decisions.push(decision.clone());
        Ok(())
    }

    async fn split(
        &self,
        tenant: TenantId,
        from: ResourceId,
        identifiers: &[Identifier],
        decision: &Decision,
    ) -> Result<ResourceId> {
        let new_id = ResourceId::new();
        let name = identifiers
            .first()
            .map_or_else(|| "split".to_owned(), |i| i.value.clone());

        let mut inner = self.lock();
        inner
            .resources
            .insert(new_id, (tenant, ResourceKind::Device, name));

        for i in identifiers {
            let key = (tenant, i.kind.as_str(), i.value.clone());
            match inner.identifiers.get(&key) {
                Some(owner) if *owner == from => {
                    inner.identifiers.insert(key, new_id);
                }
                // Splitting an identifier that is not on the resource being split is a
                // caller error, not something to silently do anyway.
                _ => {
                    return Err(Error::Invalid(format!(
                        "{} {} does not belong to {from}",
                        i.kind.as_str(),
                        i.value
                    )));
                }
            }
        }

        // The merge that created this alias is undone, so the alias must go — otherwise
        // telemetry written before the merge would keep resolving to the wrong side.
        inner.aliases.retain(|(t, _), current| {
            !(*t == tenant && *current == from && inner_alias_should_drop(identifiers))
        });
        inner.decisions.push(decision.clone());
        Ok(new_id)
    }

    async fn find_pending_review(
        &self,
        tenant: TenantId,
        observed: &[Identifier],
    ) -> Result<Option<ReviewItem>> {
        let inner = self.lock();
        let resolved: Vec<&Decision> = inner
            .decisions
            .iter()
            .filter(|d| {
                d.tenant_id == tenant
                    && matches!(
                        d.outcome,
                        DecisionOutcome::ManualMerge | DecisionOutcome::ManualSplit
                    )
            })
            .collect();

        Ok(inner
            .decisions
            .iter()
            // Newest first, and a review a human has since acted on is no longer
            // pending.
            .rfind(|d| {
                d.tenant_id == tenant
                    && d.outcome == DecisionOutcome::Review
                    && same_identifiers(&d.observed, observed)
                    && !resolved
                        .iter()
                        .any(|r| r.resource_id == d.resource_id && r.decided_at >= d.decided_at)
            })
            .and_then(|d| {
                d.resource_id.map(|provisional_id| ReviewItem {
                    decision: d.clone(),
                    provisional_id,
                })
            }))
    }

    async fn pending_reviews(&self, tenant: TenantId, limit: i64) -> Result<Vec<ReviewItem>> {
        let inner = self.lock();
        let mut items: Vec<ReviewItem> = inner
            .decisions
            .iter()
            .filter(|d| d.tenant_id == tenant && d.outcome == DecisionOutcome::Review)
            .filter_map(|d| {
                d.resource_id.map(|provisional_id| ReviewItem {
                    decision: d.clone(),
                    provisional_id,
                })
            })
            .collect();

        items.reverse(); // newest first
        items.truncate(usize::try_from(limit).unwrap_or(0));
        Ok(items)
    }
}

/// Order-independent comparison: collectors do not promise a stable identifier order,
/// and a set that differs only in order is the same question.
fn same_identifiers(a: &[Identifier], b: &[Identifier]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let key = |i: &Identifier| (i.kind.as_str(), i.value.clone());
    let mut a: Vec<_> = a.iter().map(key).collect();
    let mut b: Vec<_> = b.iter().map(key).collect();
    a.sort_unstable();
    b.sort_unstable();
    a == b
}

/// A split always undoes the alias, because the only thing that creates one is a merge.
const fn inner_alias_should_drop(_identifiers: &[Identifier]) -> bool {
    true
}

fn kind_from_str(s: &str) -> Option<uops_core::IdentifierKind> {
    use uops_core::IdentifierKind as K;
    Some(match s {
        "serial" => K::Serial,
        "chassis_id" => K::ChassisId,
        "snmp_engine_id" => K::SnmpEngineId,
        "otel_host_id" => K::OtelHostId,
        "mac" => K::Mac,
        "mgmt_ip" => K::MgmtIp,
        "flow_exporter" => K::FlowExporter,
        "hostname" => K::Hostname,
        "service_name" => K::ServiceName,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use uops_core::IdentifierKind;

    #[tokio::test]
    async fn attaching_does_not_steal_an_identifier_from_another_resource() {
        // The fake has to enforce UNIQUE (tenant, kind, value) the way the schema does,
        // or the resolver's tests pass on writes PostgreSQL would reject.
        let store = MemoryIdentityStore::default();
        let tenant = TenantId::new();
        let first = store
            .create_provisional(tenant, ResourceKind::Device, "a")
            .await
            .unwrap();
        let second = store
            .create_provisional(tenant, ResourceKind::Device, "b")
            .await
            .unwrap();
        let ids = vec![Identifier::new(IdentifierKind::Hostname, "rtr-01")];

        store
            .attach_identifiers(tenant, first, &ids, "syslog")
            .await
            .unwrap();
        store
            .attach_identifiers(tenant, second, &ids, "snmp")
            .await
            .unwrap();

        let hits = store.lookup(tenant, &ids).await.unwrap();
        assert_eq!(hits[0].resource_id, first, "the first owner keeps it");
    }

    #[tokio::test]
    async fn reassigning_does_take_it() {
        let store = MemoryIdentityStore::default();
        let tenant = TenantId::new();
        let old = store
            .create_provisional(tenant, ResourceKind::Device, "old")
            .await
            .unwrap();
        let new = store
            .create_provisional(tenant, ResourceKind::Device, "new")
            .await
            .unwrap();
        let ids = vec![Identifier::new(IdentifierKind::MgmtIp, "10.0.0.1")];

        store
            .attach_identifiers(tenant, old, &ids, "snmp")
            .await
            .unwrap();
        store
            .reassign_identifiers(tenant, new, &ids, "snmp")
            .await
            .unwrap();

        let hits = store.lookup(tenant, &ids).await.unwrap();
        assert_eq!(hits[0].resource_id, new);
    }

    #[tokio::test]
    async fn identifiers_are_scoped_to_their_tenant() {
        let store = MemoryIdentityStore::default();
        let mine = TenantId::new();
        let theirs = TenantId::new();
        let ids = vec![Identifier::new(IdentifierKind::Hostname, "rtr-01")];

        let r = store
            .create_provisional(mine, ResourceKind::Device, "rtr-01")
            .await
            .unwrap();
        store
            .attach_identifiers(mine, r, &ids, "syslog")
            .await
            .unwrap();

        assert!(
            store.lookup(theirs, &ids).await.unwrap().is_empty(),
            "two customers each have an rtr-01"
        );
    }
}
