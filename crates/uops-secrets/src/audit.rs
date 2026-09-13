//! Credential access logging — SPEC §M0.8.
//!
//! Defence, law-enforcement and regulated buyers audit who **saw** what, not only who
//! changed what (PLAN §0b). Every attempt to open a credential is recorded, including
//! failures: a burst of denied reads is precisely the signal an auditor needs, and a
//! success-only log hides exactly the event worth seeing.
//!
//! This log is never sampled. High-volume read auditing elsewhere (dashboard polling)
//! may be, but credential access is low-volume and high-consequence.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uops_core::{CredentialRef, TenantId, scope::Actor};

/// Why a credential is being opened. Required, and deliberately a `&'static str` so it
/// comes from a fixed vocabulary in the code rather than an arbitrary runtime string.
#[derive(Clone, Debug)]
pub struct AccessContext {
    pub actor: Actor,
    /// Which resource it is being used for, when that is known.
    pub resource_id: Option<uops_core::ResourceId>,
    /// `"snmp-poll"`, `"ssh-runbook"`, `"config-backup"`.
    pub purpose: &'static str,
}

impl AccessContext {
    #[must_use]
    pub const fn new(actor: Actor, purpose: &'static str) -> Self {
        Self {
            actor,
            resource_id: None,
            purpose,
        }
    }

    #[must_use]
    pub const fn for_resource(mut self, id: uops_core::ResourceId) -> Self {
        self.resource_id = Some(id);
        self
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AccessOutcome {
    Granted,
    Denied,
}

/// One recorded credential access.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AccessRecord {
    pub tenant_id: TenantId,
    pub credential_id: CredentialRef,
    /// Rendered via `Actor::as_audit_str` — `user:<uuid>`, `collector`, `system`.
    pub actor: String,
    pub resource_id: Option<uops_core::ResourceId>,
    pub purpose: String,
    pub outcome: AccessOutcome,
    pub at: DateTime<Utc>,
}

/// Sink for access records.
///
/// Infallible by design: `record` returns nothing and cannot fail the caller. An audit
/// backend that is down must not become a way to *prevent* credential use — that turns
/// a logging outage into a platform outage. Implementations buffer and surface their
/// own health instead.
pub trait AccessLog: Send + Sync {
    fn record(
        &self,
        tenant: TenantId,
        credential: CredentialRef,
        ctx: &AccessContext,
        outcome: AccessOutcome,
    );
}

impl<T: AccessLog> AccessLog for std::sync::Arc<T> {
    fn record(
        &self,
        tenant: TenantId,
        credential: CredentialRef,
        ctx: &AccessContext,
        outcome: AccessOutcome,
    ) {
        (**self).record(tenant, credential, ctx, outcome);
    }
}

/// In-memory log, for tests and for a single-node deployment before the PostgreSQL
/// sink exists.
#[derive(Debug, Default)]
pub struct MemoryAccessLog {
    records: std::sync::Mutex<Vec<AccessRecord>>,
}

impl MemoryAccessLog {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn records(&self) -> Vec<AccessRecord> {
        self.records
            .lock()
            .map_or_else(|e| e.into_inner().clone(), |g| g.clone())
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.records().len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl AccessLog for MemoryAccessLog {
    fn record(
        &self,
        tenant: TenantId,
        credential: CredentialRef,
        ctx: &AccessContext,
        outcome: AccessOutcome,
    ) {
        let rec = AccessRecord {
            tenant_id: tenant,
            credential_id: credential,
            actor: ctx.actor.as_audit_str(),
            resource_id: ctx.resource_id,
            purpose: ctx.purpose.to_owned(),
            outcome,
            at: Utc::now(),
        };
        // A poisoned mutex must not lose the record: recover the guard rather than
        // unwrapping, since dropping audit entries on a panic elsewhere is exactly the
        // circumstance in which they matter most.
        match self.records.lock() {
            Ok(mut g) => g.push(rec),
            Err(e) => e.into_inner().push(rec),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_carry_actor_purpose_and_outcome() {
        let log = MemoryAccessLog::new();
        let t = TenantId::new();
        let c = CredentialRef::new();
        log.record(
            t,
            c,
            &AccessContext::new(Actor::Collector, "snmp-poll"),
            AccessOutcome::Granted,
        );

        let recs = log.records();
        assert_eq!(recs.len(), 1);
        assert_eq!(recs[0].actor, "collector");
        assert_eq!(recs[0].purpose, "snmp-poll");
        assert_eq!(recs[0].outcome, AccessOutcome::Granted);
        assert_eq!(recs[0].credential_id, c);
    }

    #[test]
    fn survives_a_poisoned_mutex() {
        // Audit entries matter most when something else has already gone wrong, so a
        // panic elsewhere must not silently stop the log from recording.
        use std::sync::Arc;
        let log = Arc::new(MemoryAccessLog::new());
        let l2 = Arc::clone(&log);
        let _ = std::thread::spawn(move || {
            let _g = l2.records.lock().unwrap();
            panic!("poison the mutex");
        })
        .join();

        log.record(
            TenantId::new(),
            CredentialRef::new(),
            &AccessContext::new(Actor::System, "after-poison"),
            AccessOutcome::Granted,
        );
        assert_eq!(log.records().len(), 1, "record must survive poisoning");
    }
}
