//! Turning what a sweep found into inventory — M5 §2.4, §2.5.
//!
//! [`uops_discover::run`] produces [`Findings`]: a list of agents that answered, a list
//! that refused, and counts. This is the other half — deciding which of those become
//! resources, which become candidates, and what the run's counters say.
//!
//! # Why the resolver rather than a create
//!
//! `uops_store_pg::discovery` — interface discovery — deliberately bypasses identity
//! resolution, and its comment says why: an interface's parent is not in question. A
//! swept device is the opposite case. It is exactly *"a resource that turned up"*, which
//! is what the resolver and its review queue exist for, and a sweep that called
//! `create_resource` would produce a second copy of every device already known from
//! syslog, OTLP or a previous sweep.
//!
//! # What a probe proves, and what it does not
//!
//! Two identifiers, and both are weak: an address DHCP can move and a name that is
//! duplicated across every site in an estate built from one template. Neither is tier
//! one, so a sweep *cannot* auto-merge on its own evidence and lands most sightings in
//! review. That is the correct outcome rather than a limitation — the review queue is
//! where "the same hostname in two sites" belongs — and the tier-1 identifiers arrive
//! with the first poll and upgrade the resolution then.
//!
//! # Why a swept device is pollable with no further step
//!
//! [`PgStore::pollable_devices`] joins `resource_identifier` on `kind = 'mgmt_ip'`, and
//! `Sighting::observed` emits exactly that. So attaching the identity *is* making the
//! device pollable; there is no second registration to forget. The `sysObjectID` goes to
//! the same attribute key the poller caches it under, so the first poll uses the right
//! profile rather than falling back to `generic-snmp` for a cycle.
//!
//! It does **not** set `resource.profile_id`. That column is a human's pin — an explicit
//! override that beats `sysObjectID` matching — and writing it from discovery would turn
//! every swept device into one an operator appears to have made a decision about.

use uops_core::{CredentialRef, Resolution, ResourceId, SiteId, TenantId, TenantScope};
use uops_discover::Findings;
use uops_identity::{IdentityStore, Resolver};

use crate::discovery_jobs::{CandidateSource, CandidateState, NewCandidate, RunCounts};
use crate::error::map;
use crate::pollable::SYSOBJECTID_KEY;
use crate::store::PgStore;

/// The attribute key a device's `sysDescr` is kept under.
///
/// Worth storing even though nothing reads it yet: it is the only thing that identifies
/// a device with no profile, and the sentence an operator reads when asking why
/// discovery could not classify something.
pub const SYSDESCR_KEY: &str = "snmp.sysdescr";

/// What a sweep's results should be attributed to.
#[derive(Clone, Copy, Debug, Default)]
pub struct SweepContext {
    /// The run these findings belong to, so every candidate says which sweep last saw it.
    pub run_id: Option<uuid::Uuid>,
    /// Where the job says these devices are.
    ///
    /// Applied with `COALESCE`, so rediscovering a device never moves it between sites —
    /// an operator who corrected a site assignment must not have it undone tonight.
    pub site_id: Option<SiteId>,
    /// The credential that answered.
    ///
    /// Also `COALESCE`d, for the same reason. `None` until the multi-credential probe
    /// loop exists; a device with no credential is created and simply not polled, which
    /// is the honest state for one nothing has proved it can talk to.
    pub credential: Option<CredentialRef>,
}

impl PgStore {
    /// Record what a sweep found.
    ///
    /// Every device that answered goes through identity resolution. Everything that did
    /// not become a resource becomes a candidate, with a reason: §3's argument is that a
    /// product which silently drops what it cannot classify is one whose inventory an
    /// operator cannot trust.
    ///
    /// Returns the counters for [`PgStore::finish_discovery_run`].
    ///
    /// # Errors
    ///
    /// Storage failures. A single sighting that fails to resolve is *not* one of them —
    /// see the loop.
    pub async fn record_sweep<S: IdentityStore>(
        &self,
        scope: &TenantScope,
        resolver: &Resolver<S>,
        findings: &Findings,
        context: SweepContext,
    ) -> uops_core::Result<RunCounts> {
        let mut counts = RunCounts {
            probed: i32::try_from(findings.probed).unwrap_or(i32::MAX),
            answered: i32::try_from(findings.answered()).unwrap_or(i32::MAX),
            ..RunCounts::default()
        };

        for sighting in &findings.devices {
            // An agent that answered with neither a name nor a sysObjectID is real —
            // UPSs, PDUs and environmental sensors do it — and it is not inventory. A
            // resource with no name and no profile is a row nothing can poll and nobody
            // can act on, which is the same argument §2.5 makes about inventing a device
            // from a chassis id. It goes on the candidate list, where an operator can see
            // that something is there and decide.
            if sighting.sys_name.is_none() && sighting.sys_object_id.is_none() {
                self.record_candidate(
                    scope,
                    context.run_id,
                    CandidateSource::Sweep,
                    &NewCandidate {
                        address: Some(sighting.address.ip()),
                        sys_descr: sighting.sys_descr.clone(),
                        state: CandidateState::Unidentified,
                        reason: "answered SNMP but reported neither a name nor a \
                                 sysObjectID, so nothing can classify or name it"
                            .to_owned(),
                        ..NewCandidate::default()
                    },
                )
                .await?;
                counts.candidates += 1;
                continue;
            }

            let resolution = resolver
                .resolve(scope.tenant_id(), &sighting.observed())
                .await?;

            let resource_id = match resolution {
                Resolution::Matched { resource_id, .. } => {
                    counts.merged += 1;
                    resource_id
                }
                Resolution::Created { resource_id } => {
                    counts.created += 1;
                    resource_id
                }
                // A provisional resource *and* a review item. That is the resolver's
                // contract everywhere in this product — ingestion is never blocked on a
                // human — and discovery does not get its own variant of it. The
                // provisional row is what a reviewer merges *from*; without one there
                // would be nothing to point the merge at.
                Resolution::Review { provisional_id, .. } => {
                    counts.for_review += 1;
                    provisional_id
                }
            };

            self.describe_swept_device(scope, resource_id, sighting, context)
                .await?;
        }

        // §2.2, as it reaches the database. Never retried with a guess; written down with
        // a sentence that tells the operator what to supply.
        for address in &findings.refused {
            self.record_candidate(
                scope,
                context.run_id,
                CandidateSource::Sweep,
                &NewCandidate {
                    address: Some(address.ip()),
                    state: CandidateState::Unreachable,
                    reason: "an SNMP agent is here and refused every credential this job \
                             names — add the right one to the job and run it again"
                        .to_owned(),
                    ..NewCandidate::default()
                },
            )
            .await?;
            counts.candidates += 1;
        }

        Ok(counts)
    }

    /// Fill in what the probe learned, without undoing anything a human decided.
    ///
    /// `name` is written outright: migration 0002 calls it *"canonical and system-chosen.
    /// Identity resolution may rewrite it"*, and `display_name` is the operator's
    /// override, which nothing here touches. Everything else is `COALESCE`d, so
    /// rediscovering a device tonight cannot move it to another site or replace the
    /// credential somebody fixed this afternoon.
    async fn describe_swept_device(
        &self,
        scope: &TenantScope,
        resource: ResourceId,
        sighting: &uops_discover::Sighting,
        context: SweepContext,
    ) -> uops_core::Result<()> {
        let mut attributes = serde_json::Map::new();
        if let Some(oid) = &sighting.sys_object_id {
            attributes.insert(SYSOBJECTID_KEY.to_owned(), oid.to_string().into());
        }
        if let Some(descr) = &sighting.sys_descr {
            attributes.insert(SYSDESCR_KEY.to_owned(), descr.clone().into());
        }
        let attributes = serde_json::Value::Object(attributes);

        // tenant-exempt: the tenant is a bound parameter, from the scope.
        sqlx::query!(
            r#"
            UPDATE resource
               SET name           = COALESCE($3, name),
                   -- Merged rather than replaced: this device may carry attributes from
                   -- OTLP or from a poll, and a sweep knows about two of them.
                   attributes     = attributes || $4::jsonb,
                   site_id        = COALESCE(site_id, $5),
                   credential_ref = COALESCE(credential_ref, $6),
                   last_seen      = now(),
                   updated_at     = now()
             WHERE id = $1 AND tenant_id = $2
            "#,
            resource as ResourceId,
            scope.tenant_id() as TenantId,
            sighting.sys_name.as_deref(),
            attributes,
            context.site_id as Option<SiteId>,
            context.credential as Option<CredentialRef>,
        )
        .execute(self.pool())
        .await
        .map_err(|e| map("resource", resource.to_string(), e))?;

        Ok(())
    }
}
