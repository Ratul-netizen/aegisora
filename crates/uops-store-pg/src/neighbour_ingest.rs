//! Turning neighbour tables into edges and candidates — M5 §2.5, §2.6.
//!
//! # Why this looks up rather than resolves
//!
//! [`PgStore::record_sweep`] calls the resolver, which creates a resource when nothing
//! matches. This must not: §2.5's whole argument is that a chassis ID is an identifier
//! and not a device, and that inventing the far end of a link produces an inventory full
//! of half-devices nothing knows how to reach. So an unknown neighbour goes through
//! `IdentityStore::lookup` — which reads and never writes — and becomes a candidate.
//!
//! The next sweep, or an operator pressing *probe*, turns it into a device. That is the
//! order that keeps the inventory honest: a device is created when something has actually
//! talked to it.
//!
//! # Why an edge has no direction
//!
//! `resource_relationship` is `UNIQUE (tenant_id, source_id, target_id, kind)`, which
//! stops one walk writing the same edge twice and does nothing about the real duplicate:
//! walk *both* ends of a cable and you get A→B and B→A, two rows for one link, and every
//! topology view draws it twice.
//!
//! `ConnectedTo` is symmetric — unlike `DependsOn`, `Hosts` and `Runs`, which are not —
//! so the pair is sorted before it is written. One cable, one row, whichever end was
//! walked first. The ordering is by UUID and is arbitrary on purpose: it is a
//! canonical form, not a claim about which device matters.
//!
//! # Why `connected_to` and nothing else
//!
//! `RelationshipKind::ConnectedTo` is already excluded from blast-radius traversal, and
//! `resource.rs` says why: L2 adjacency is not causation. A switch being cabled to a
//! server does not mean the server depends on that switch in any sense an incident cares
//! about — it may well have a second path. `depends_on` is a judgement about services,
//! and M9's correlation engine is what will infer it.

use uops_core::{Identifier, IdentifierKind, ResourceId, TenantId, TenantScope};
use uops_discover::Neighbour;
use uops_discover::neighbour::Protocol;
use uops_identity::IdentityStore;

use crate::discovery_jobs::{CandidateSource, CandidateState, NewCandidate};
use crate::error::map;
use crate::store::PgStore;
use crate::sweep_ingest::SweepContext;

/// How a `connected_to` edge is labelled in `resource_relationship.discovered_by`.
///
/// The column is free text on purpose — migration 0003 says a new discovery source
/// should not need a migration — and these are the values discovery writes.
const DISCOVERED_BY: [&str; 3] = ["lldp", "cdp", "arp"];

/// What a neighbour walk produced.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct NeighbourOutcome {
    /// `connected_to` edges written or refreshed.
    pub edges: i32,
    /// Neighbours that are not resources yet.
    pub candidates: i32,
}

impl PgStore {
    /// Record what one device said about the devices next to it.
    ///
    /// `seen_from` is the device whose tables these are. Every candidate records it, so
    /// an operator looking at an unexpected device can see which switch reported it —
    /// which is the first thing they will want to know.
    ///
    /// # Errors
    ///
    /// Storage failures.
    pub async fn record_neighbours(
        &self,
        scope: &TenantScope,
        seen_from: ResourceId,
        neighbours: &[Neighbour],
        context: SweepContext<'_>,
    ) -> uops_core::Result<NeighbourOutcome> {
        let mut outcome = NeighbourOutcome::default();

        for neighbour in neighbours {
            let identifiers = identifiers_of(neighbour);
            if identifiers.is_empty() {
                continue;
            }

            match self.identify(scope.tenant_id(), &identifiers).await? {
                // A link between two devices that both exist. §2.6.
                Known(resource) if resource != seen_from => {
                    self.cable(scope, seen_from, resource, neighbour.protocol)
                        .await?;
                    outcome.edges += 1;
                }
                // A device reporting itself. Some agents list their own chassis in
                // `lldpRemTable` when two of their ports are patched together, and the
                // schema refuses a self-loop anyway — `relationship_is_not_a_self_loop`.
                // Silently skipped rather than counted: it is not a finding.
                Known(_) => {}
                Unknown => {
                    self.record_candidate(
                        scope,
                        context.run_id,
                        source_of(neighbour.protocol),
                        &NewCandidate {
                            address: neighbour.address,
                            chassis_id: neighbour.chassis_id.clone(),
                            port_id: neighbour.port_id.clone(),
                            platform: neighbour.platform.clone(),
                            sys_name: neighbour.sys_name.clone(),
                            sys_descr: neighbour.sys_descr.clone(),
                            mac: neighbour.mac.clone(),
                            seen_from: Some(seen_from),
                            state: CandidateState::Unidentified,
                            reason: reason_for(neighbour),
                            ..NewCandidate::default()
                        },
                    )
                    .await?;
                    outcome.candidates += 1;
                }
                Ambiguous => {
                    self.record_candidate(
                        scope,
                        context.run_id,
                        source_of(neighbour.protocol),
                        &NewCandidate {
                            address: neighbour.address,
                            chassis_id: neighbour.chassis_id.clone(),
                            port_id: neighbour.port_id.clone(),
                            platform: neighbour.platform.clone(),
                            sys_name: neighbour.sys_name.clone(),
                            sys_descr: neighbour.sys_descr.clone(),
                            mac: neighbour.mac.clone(),
                            seen_from: Some(seen_from),
                            state: CandidateState::Ambiguous,
                            reason: "more than one resource matches what this neighbour \
                                     reported, so no edge could be drawn without guessing \
                                     which one"
                                .to_owned(),
                            ..NewCandidate::default()
                        },
                    )
                    .await?;
                    outcome.candidates += 1;
                }
            }
        }

        Ok(outcome)
    }

    /// Which resource a neighbour already is, if any.
    ///
    /// Read-only, which is the point — see the module docs.
    ///
    /// A tier-1 hit decides on its own: a chassis ID is globally unique by specification,
    /// so one is proof rather than evidence. Failing that, the hits must all agree; two
    /// resources answering to one neighbour is [`Ambiguous`], not a coin toss. That is
    /// the "core-01 in every building" case reaching the topology, and drawing a cable to
    /// the wrong building is worse than drawing none.
    async fn identify(
        &self,
        tenant: TenantId,
        identifiers: &[Identifier],
    ) -> uops_core::Result<Match> {
        let hits = self.lookup(tenant, identifiers).await?;
        if hits.is_empty() {
            return Ok(Unknown);
        }

        if let Some(hit) = hits.iter().find(|h| h.identifier.kind.is_tier_one()) {
            return Ok(Known(hit.resource_id));
        }

        let mut distinct: Vec<ResourceId> = hits.iter().map(|h| h.resource_id).collect();
        distinct.sort_unstable();
        distinct.dedup();

        match distinct.as_slice() {
            [one] => Ok(Known(*one)),
            _ => Ok(Ambiguous),
        }
    }

    /// Write, or refresh, the cable between two devices.
    ///
    /// Sorted before it is written, so walking both ends produces one row. See the module
    /// docs.
    async fn cable(
        &self,
        scope: &TenantScope,
        a: ResourceId,
        b: ResourceId,
        protocol: Protocol,
    ) -> uops_core::Result<()> {
        let (source, target) = if a <= b { (a, b) } else { (b, a) };
        let discovered_by = DISCOVERED_BY[protocol as usize];

        // `connected_to` is written as a literal rather than bound. `relationship_kind`
        // is a PostgreSQL enum and `RelationshipKind` carries no sqlx mapping, so the
        // only alternatives are a cast on every call site or a derive on a core type for
        // one query's benefit. Migration 0003's other writer does the same with
        // `member_of`.
        //
        // tenant-exempt: the tenant is a bound parameter, from the scope.
        sqlx::query!(
            r#"
            INSERT INTO resource_relationship
                (id, tenant_id, source_id, target_id, kind, discovered_by)
            VALUES ($1, $2, $3, $4, 'connected_to', $5)
            ON CONFLICT (tenant_id, source_id, target_id, kind) DO UPDATE
               SET last_seen = now(),
                   -- The protocol that most recently confirmed the link. LLDP and CDP
                   -- both reporting one cable is normal, and whichever spoke last is a
                   -- fact about the walk rather than about the cable.
                   discovered_by = EXCLUDED.discovered_by
            "#,
            uuid::Uuid::now_v7(),
            scope.tenant_id() as TenantId,
            source as ResourceId,
            target as ResourceId,
            discovered_by,
        )
        .execute(self.pool())
        .await
        .map_err(|e| map("resource_relationship", source.to_string(), e))?;

        Ok(())
    }
}

/// Whether a neighbour is already something we know about.
enum Match {
    Known(ResourceId),
    Unknown,
    /// More than one resource answers to it.
    Ambiguous,
}
use Match::{Ambiguous, Known, Unknown};

/// What a neighbour sighting proves about who it is.
///
/// A chassis ID is tier 1 — globally unique by specification — and is the reason LLDP is
/// worth more than the other two protocols put together. The address and the name are
/// tier 3 and 4 and are here to catch a device already known from a sweep.
///
/// A MAC from ARP is deliberately *not* included. `IdentifierKind::Mac` is tier 2, and an
/// ARP table's MAC belongs to whichever interface answered on that subnet — frequently a
/// router's, not the device's. Treating it as an identity would attach a laptop's
/// telemetry to the gateway.
fn identifiers_of(neighbour: &Neighbour) -> Vec<Identifier> {
    let mut out = Vec::new();
    if let Some(chassis) = &neighbour.chassis_id {
        out.push(Identifier::new(IdentifierKind::ChassisId, chassis.clone()));
    }
    if let Some(address) = neighbour.address {
        out.push(Identifier::new(IdentifierKind::MgmtIp, address.to_string()));
    }
    if let Some(name) = &neighbour.sys_name {
        out.push(Identifier::new(IdentifierKind::Hostname, name.clone()));
    }
    out
}

const fn source_of(protocol: Protocol) -> CandidateSource {
    match protocol {
        Protocol::Lldp => CandidateSource::Lldp,
        Protocol::Cdp => CandidateSource::Cdp,
        Protocol::Arp => CandidateSource::Arp,
    }
}

/// The sentence on the candidate list.
///
/// Different per protocol, because what an operator should do about it is different. An
/// LLDP neighbour with a chassis ID is a real device worth adding; an ARP entry is
/// probably somebody's laptop.
fn reason_for(neighbour: &Neighbour) -> String {
    match neighbour.protocol {
        Protocol::Lldp | Protocol::Cdp => {
            if neighbour.address.is_some() {
                "reported as a neighbour and not in the inventory — add its range to a \
                 discovery job, or probe it directly"
                    .to_owned()
            } else {
                // The case §2.5 is about. Nothing can reach it, so nothing can create it.
                "reported as a neighbour with no management address, so there is nothing \
                 to probe — it needs an address before it can become a device"
                    .to_owned()
            }
        }
        Protocol::Arp => "seen in an ARP table, which proves only that this address is in \
                          use on that subnet"
            .to_owned(),
    }
}
