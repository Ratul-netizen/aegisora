//! Discovery jobs, runs and candidates — M5, `docs/M5-discovery.md`.
//!
//! Not to be confused with [`crate::discovery`], which is also called discovery and is a
//! different thing: it keys the *children* of a device already known. This is about
//! finding the device.
//!
//! # Why the ranges are validated twice
//!
//! [`PgStore::create_discovery_job`] asks [`Sweep::check`] before it writes the row, and
//! migration 0017 has CHECK constraints saying the same things. That is deliberate rather
//! than redundant.
//!
//! The constraint is what makes the limit true — of every writer, including a future one
//! that forgets, and including `psql`. But a CHECK produces `violates
//! discovery_job_no_range_wider_than_a_16`, and an operator who reads that has learned
//! only that they are wrong. [`SweepError`] produces the sentence that says what to do
//! instead, and §4's fourth acceptance criterion is about the sentence, not the refusal.
//!
//! So: the crate gives the message, the schema gives the guarantee.
//!
//! # Why the ranges cross the boundary as text
//!
//! `cidr[]` has no `sqlx` mapping without the `ipnetwork` feature, and adding a
//! dependency to move a value that [`uops_discover::Range`] already models properly would
//! be the wrong trade. They are cast in the SQL and parsed on the way back, so everything
//! above this module holds a `Range` and nothing holds a string that might not be one.

use std::str::FromStr;

use uops_core::{
    ActorId, CredentialRef, Error as CoreError, Result, SiteId, TenantId, TenantScope,
};
use uops_discover::{Range, Sweep};

use crate::error::map;
use crate::store::PgStore;

/// A standing instruction to sweep some ranges.
#[derive(Clone, Debug)]
pub struct DiscoveryJob {
    pub id: uuid::Uuid,
    pub tenant_id: TenantId,
    pub name: String,
    pub description: String,
    pub ranges: Vec<Range>,
    /// Where the resources this job creates belong.
    ///
    /// Discovery cannot infer a site from an address — the same RFC 1918 range is in use
    /// in every building on earth — so this is the operator's answer to a question the
    /// network cannot answer.
    pub site_id: Option<SiteId>,
    /// What this job may try, in order. The whole of it: there is no fallback.
    pub credential_refs: Vec<CredentialRef>,
    pub snmp_port: u16,
    pub skip_silent_hosts: bool,
    /// `None` means manual only, which is a normal thing to want for a one-off sweep of
    /// a newly acquired site.
    pub schedule: Option<std::time::Duration>,
    pub enabled: bool,
    pub last_run_at: Option<chrono::DateTime<chrono::Utc>>,
    pub created_by: Option<ActorId>,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

/// What a caller supplies to create one.
#[derive(Clone, Debug)]
pub struct NewJob {
    pub name: String,
    pub description: String,
    pub ranges: Vec<Range>,
    pub site_id: Option<SiteId>,
    pub credential_refs: Vec<CredentialRef>,
    pub snmp_port: u16,
    pub skip_silent_hosts: bool,
    pub schedule: Option<std::time::Duration>,
}

/// One execution of a sweep.
#[derive(Clone, Debug)]
pub struct DiscoveryRun {
    pub id: uuid::Uuid,
    pub tenant_id: TenantId,
    pub job_id: Option<uuid::Uuid>,
    /// What was actually scanned, snapshotted at the time.
    ///
    /// Not read back through the job, whose ranges are editable. "Who scanned
    /// 10.0.0.0/16 on Tuesday" is a question about the ranges as they were.
    pub ranges: Vec<Range>,
    pub trigger: Trigger,
    pub started_by: Option<ActorId>,
    pub started_at: chrono::DateTime<chrono::Utc>,
    pub finished_at: Option<chrono::DateTime<chrono::Utc>>,
    pub status: RunStatus,
    pub error: Option<String>,
    pub counts: RunCounts,
}

/// What a run did. Every field is a number an operator reads on the run list.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RunCounts {
    pub probed: i32,
    /// Including refusals: an agent that rejected the credentials proved it exists, so
    /// `probed - answered` is the empty addresses and nothing else.
    pub answered: i32,
    pub created: i32,
    pub merged: i32,
    pub for_review: i32,
    pub candidates: i32,
    pub edges: i32,
}

/// Why a run started.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Trigger {
    Schedule,
    /// An operator pressed the button.
    Manual,
    /// An operator probed one address from the candidate list. No job behind it.
    Probe,
}

impl Trigger {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Schedule => "schedule",
            Self::Manual => "manual",
            Self::Probe => "probe",
        }
    }
}

/// How a run ended, or that it has not.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RunStatus {
    Running,
    Succeeded,
    Failed,
    Cancelled,
}

impl RunStatus {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }
}

/// Something a run found and did not turn into a resource.
#[derive(Clone, Debug)]
pub struct DiscoveryCandidate {
    pub id: uuid::Uuid,
    pub tenant_id: TenantId,
    pub last_run_id: Option<uuid::Uuid>,
    pub source: CandidateSource,
    /// `None` for a neighbour that reported no management address, which many platforms
    /// do — and which is exactly why §2.5 refuses to invent a resource from one.
    pub address: Option<std::net::IpAddr>,
    pub chassis_id: Option<String>,
    pub port_id: Option<String>,
    pub platform: Option<String>,
    pub sys_name: Option<String>,
    pub sys_descr: Option<String>,
    pub sys_object_id: Option<String>,
    pub mac: Option<String>,
    /// The resource whose neighbour table this came from. `None` for a sweep.
    pub seen_from: Option<uops_core::ResourceId>,
    pub state: CandidateState,
    pub resource_id: Option<uops_core::ResourceId>,
    /// Why it is still here, in a sentence an operator can act on.
    pub reason: String,
    pub first_seen: chrono::DateTime<chrono::Utc>,
    pub last_seen: chrono::DateTime<chrono::Utc>,
}

/// What a caller supplies to record one.
///
/// `Default` so that a sweep candidate — address and a reason — does not have to spell
/// out six neighbour fields it knows nothing about.
#[derive(Clone, Debug, Default)]
pub struct NewCandidate {
    pub address: Option<std::net::IpAddr>,
    pub chassis_id: Option<String>,
    pub port_id: Option<String>,
    pub platform: Option<String>,
    pub sys_name: Option<String>,
    pub sys_descr: Option<String>,
    pub sys_object_id: Option<String>,
    pub mac: Option<String>,
    pub seen_from: Option<uops_core::ResourceId>,
    pub state: CandidateState,
    pub reason: String,
}

/// Which of the four things told us about this.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CandidateSource {
    Sweep,
    Lldp,
    Cdp,
    Arp,
}

impl CandidateSource {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Sweep => "sweep",
            Self::Lldp => "lldp",
            Self::Cdp => "cdp",
            Self::Arp => "arp",
        }
    }
}

/// Why a candidate is still a candidate.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CandidateState {
    /// Answered, and nothing could say what it is.
    #[default]
    Unidentified,
    /// Matched more than one existing resource. The review queue's case.
    Ambiguous,
    /// Refused the credentials the job named. §2.2 — recorded, never guessed at.
    Unreachable,
    /// Became a resource.
    Promoted,
    /// An operator said "I know, it is a printer, stop showing me".
    Ignored,
}

impl CandidateState {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Unidentified => "unidentified",
            Self::Ambiguous => "ambiguous",
            Self::Unreachable => "unreachable",
            Self::Promoted => "promoted",
            Self::Ignored => "ignored",
        }
    }

    /// Whether this is still on the operator's list.
    #[must_use]
    pub const fn is_outstanding(self) -> bool {
        matches!(
            self,
            Self::Unidentified | Self::Ambiguous | Self::Unreachable
        )
    }
}

// ----------------------------------------------------------------------------
// Rows
// ----------------------------------------------------------------------------

/// The job as `PostgreSQL` holds it.
struct JobRow {
    id: uuid::Uuid,
    tenant_id: TenantId,
    name: String,
    description: String,
    ranges: Vec<String>,
    site_id: Option<SiteId>,
    credential_refs: Vec<uuid::Uuid>,
    snmp_port: i32,
    skip_silent_hosts: bool,
    schedule_seconds: Option<i64>,
    enabled: bool,
    last_run_at: Option<chrono::DateTime<chrono::Utc>>,
    created_by: Option<ActorId>,
    created_at: chrono::DateTime<chrono::Utc>,
    updated_at: chrono::DateTime<chrono::Utc>,
}

/// Parse text back into the types the rest of the code holds.
///
/// A row that will not parse is surfaced rather than skipped. A job that silently
/// disappears from a list because one of its ranges no longer parses is a support case
/// nobody can reproduce — and the value came out of a `cidr` column, so it parsing is a
/// property of this code rather than of the data.
fn ranges_from(text: &[String], what: &str) -> Result<Vec<Range>> {
    text.iter()
        .map(|r| {
            // PostgreSQL renders `cidr` as `10.0.0.0/8`, which `Range` parses, but an
            // unsuffixed host address is possible if somebody wrote the column by hand.
            Range::from_str(r).or_else(|_| Range::from_str(&format!("{r}/32")))
        })
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|e| CoreError::Storage(format!("{what} holds a range that will not parse: {e}")))
}

impl JobRow {
    fn parse(self) -> Result<DiscoveryJob> {
        Ok(DiscoveryJob {
            ranges: ranges_from(&self.ranges, "discovery_job")?,
            id: self.id,
            tenant_id: self.tenant_id,
            name: self.name,
            description: self.description,
            site_id: self.site_id,
            credential_refs: self
                .credential_refs
                .into_iter()
                .map(CredentialRef::from_uuid)
                .collect(),
            snmp_port: u16::try_from(self.snmp_port).unwrap_or(161),
            skip_silent_hosts: self.skip_silent_hosts,
            schedule: self
                .schedule_seconds
                .and_then(|s| u64::try_from(s).ok())
                .map(std::time::Duration::from_secs),
            enabled: self.enabled,
            last_run_at: self.last_run_at,
            created_by: self.created_by,
            created_at: self.created_at,
            updated_at: self.updated_at,
        })
    }
}

/// The run as `PostgreSQL` holds it.
struct RunRow {
    id: uuid::Uuid,
    tenant_id: TenantId,
    job_id: Option<uuid::Uuid>,
    ranges: Vec<String>,
    trigger: String,
    started_by: Option<ActorId>,
    started_at: chrono::DateTime<chrono::Utc>,
    finished_at: Option<chrono::DateTime<chrono::Utc>>,
    status: String,
    error: Option<String>,
    probed: i32,
    answered: i32,
    created: i32,
    merged: i32,
    for_review: i32,
    candidates: i32,
    edges: i32,
}

impl RunRow {
    fn parse(self) -> Result<DiscoveryRun> {
        Ok(DiscoveryRun {
            ranges: ranges_from(&self.ranges, "discovery_run")?,
            id: self.id,
            tenant_id: self.tenant_id,
            job_id: self.job_id,
            trigger: match self.trigger.as_str() {
                "schedule" => Trigger::Schedule,
                "probe" => Trigger::Probe,
                _ => Trigger::Manual,
            },
            started_by: self.started_by,
            started_at: self.started_at,
            finished_at: self.finished_at,
            status: match self.status.as_str() {
                "succeeded" => RunStatus::Succeeded,
                "failed" => RunStatus::Failed,
                "cancelled" => RunStatus::Cancelled,
                _ => RunStatus::Running,
            },
            error: self.error,
            counts: RunCounts {
                probed: self.probed,
                answered: self.answered,
                created: self.created,
                merged: self.merged,
                for_review: self.for_review,
                candidates: self.candidates,
                edges: self.edges,
            },
        })
    }
}

/// The candidate as `PostgreSQL` holds it.
struct CandidateRow {
    id: uuid::Uuid,
    tenant_id: TenantId,
    last_run_id: Option<uuid::Uuid>,
    source: String,
    address: Option<String>,
    chassis_id: Option<String>,
    port_id: Option<String>,
    platform: Option<String>,
    sys_name: Option<String>,
    sys_descr: Option<String>,
    sys_object_id: Option<String>,
    mac: Option<String>,
    seen_from: Option<uops_core::ResourceId>,
    state: String,
    resource_id: Option<uops_core::ResourceId>,
    reason: String,
    first_seen: chrono::DateTime<chrono::Utc>,
    last_seen: chrono::DateTime<chrono::Utc>,
}

impl CandidateRow {
    fn parse(self) -> Result<DiscoveryCandidate> {
        Ok(DiscoveryCandidate {
            address: self
                .address
                .as_deref()
                .map(str::parse)
                .transpose()
                .map_err(|e| {
                    CoreError::Storage(format!("discovery_candidate holds a bad address: {e}"))
                })?,
            id: self.id,
            tenant_id: self.tenant_id,
            last_run_id: self.last_run_id,
            source: match self.source.as_str() {
                "lldp" => CandidateSource::Lldp,
                "cdp" => CandidateSource::Cdp,
                "arp" => CandidateSource::Arp,
                _ => CandidateSource::Sweep,
            },
            chassis_id: self.chassis_id,
            port_id: self.port_id,
            platform: self.platform,
            sys_name: self.sys_name,
            sys_descr: self.sys_descr,
            sys_object_id: self.sys_object_id,
            mac: self.mac,
            seen_from: self.seen_from,
            state: match self.state.as_str() {
                "ambiguous" => CandidateState::Ambiguous,
                "unreachable" => CandidateState::Unreachable,
                "promoted" => CandidateState::Promoted,
                "ignored" => CandidateState::Ignored,
                _ => CandidateState::Unidentified,
            },
            resource_id: self.resource_id,
            reason: self.reason,
            first_seen: self.first_seen,
            last_seen: self.last_seen,
        })
    }
}

/// The ranges as the SQL wants them: text, to be cast to `cidr[]`.
fn as_text(ranges: &[Range]) -> Vec<String> {
    ranges.iter().map(Range::to_string).collect()
}

// ----------------------------------------------------------------------------
// Jobs
// ----------------------------------------------------------------------------

impl PgStore {
    /// Create a discovery job.
    ///
    /// # Errors
    ///
    /// `Invalid`, carrying [`SweepError`]'s own sentence, when the ranges are outside
    /// what one job may probe. The schema would refuse the same row; this is refused
    /// first so that the operator is told what to do instead rather than which constraint
    /// they broke.
    ///
    /// `Invalid` again when the tenant already has a job by that name, or when the site
    /// or a credential belongs to another tenant.
    ///
    /// [`SweepError`]: uops_discover::SweepError
    pub async fn create_discovery_job(
        &self,
        scope: &TenantScope,
        by: Option<ActorId>,
        new: &NewJob,
    ) -> Result<DiscoveryJob> {
        // The same gate `uops-discover` applies before it sends a packet, asked without
        // expanding the ranges -- this is a yes-or-no question, not a plan.
        Sweep::check(&new.ranges).map_err(|e| CoreError::Invalid(e.to_string()))?;

        if new.credential_refs.is_empty() {
            // The schema says this too. Said here as a sentence, because "violates
            // discovery_job_has_a_credential" does not tell an operator that a job with
            // no credentials would scan their network and find nothing.
            return Err(CoreError::Invalid(
                "a discovery job needs at least one credential to try — without one it \
                 would probe every address and report an empty network"
                    .to_owned(),
            ));
        }

        let ranges = as_text(&new.ranges);
        let credentials: Vec<uuid::Uuid> =
            new.credential_refs.iter().map(|c| c.into_uuid()).collect();
        let schedule = new
            .schedule
            .map(|d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX));

        // tenant-exempt: the tenant is a bound parameter, from the scope.
        let row = sqlx::query_as!(
            JobRow,
            r#"
            INSERT INTO discovery_job
                (tenant_id, name, description, ranges, site_id, credential_refs,
                 snmp_port, skip_silent_hosts, schedule, created_by)
            VALUES
                ($1, $2, $3, $4::text[]::cidr[], $5, $6, $7, $8,
                 CASE WHEN $9::bigint IS NULL THEN NULL
                      ELSE make_interval(secs => $9::bigint) END,
                 $10)
            RETURNING
                id,
                tenant_id         AS "tenant_id: TenantId",
                name,
                description,
                ranges::text[]    AS "ranges!: Vec<String>",
                site_id           AS "site_id: SiteId",
                credential_refs,
                snmp_port,
                skip_silent_hosts,
                EXTRACT(EPOCH FROM schedule)::bigint AS "schedule_seconds: i64",
                enabled,
                last_run_at,
                created_by        AS "created_by: ActorId",
                created_at,
                updated_at
            "#,
            scope.tenant_id() as TenantId,
            new.name.trim(),
            new.description,
            &ranges,
            new.site_id as Option<SiteId>,
            &credentials,
            i32::from(new.snmp_port),
            new.skip_silent_hosts,
            schedule,
            by as Option<ActorId>,
        )
        .fetch_one(self.pool())
        .await
        .map_err(|e| map("discovery_job", new.name.clone(), e))?;

        row.parse()
    }

    /// A tenant's discovery jobs, by name.
    ///
    /// By name rather than by recency: a job list is an inventory of standing
    /// instructions that an operator reads down looking for one, not a feed.
    pub async fn discovery_jobs(&self, scope: &TenantScope) -> Result<Vec<DiscoveryJob>> {
        // tenant-exempt: the tenant is the only bound parameter, from the scope.
        let rows = sqlx::query_as!(
            JobRow,
            r#"
            SELECT
                id,
                tenant_id         AS "tenant_id: TenantId",
                name,
                description,
                ranges::text[]    AS "ranges!: Vec<String>",
                site_id           AS "site_id: SiteId",
                credential_refs,
                snmp_port,
                skip_silent_hosts,
                EXTRACT(EPOCH FROM schedule)::bigint AS "schedule_seconds: i64",
                enabled,
                last_run_at,
                created_by        AS "created_by: ActorId",
                created_at,
                updated_at
              FROM discovery_job
             WHERE tenant_id = $1
             ORDER BY name
            "#,
            scope.tenant_id() as TenantId,
        )
        .fetch_all(self.pool())
        .await
        .map_err(|e| map("discovery_job", "list".to_owned(), e))?;

        rows.into_iter().map(JobRow::parse).collect()
    }

    /// One job.
    ///
    /// # Errors
    ///
    /// `NotFound` for a job in another tenant, which is the same answer as for one that
    /// does not exist. 404-never-403: a distinguishable response is a way to enumerate
    /// another customer's jobs by id — and a discovery job names their networks.
    pub async fn discovery_job(&self, scope: &TenantScope, id: uuid::Uuid) -> Result<DiscoveryJob> {
        // tenant-exempt: the tenant is a bound parameter, from the scope.
        let row = sqlx::query_as!(
            JobRow,
            r#"
            SELECT
                id,
                tenant_id         AS "tenant_id: TenantId",
                name,
                description,
                ranges::text[]    AS "ranges!: Vec<String>",
                site_id           AS "site_id: SiteId",
                credential_refs,
                snmp_port,
                skip_silent_hosts,
                EXTRACT(EPOCH FROM schedule)::bigint AS "schedule_seconds: i64",
                enabled,
                last_run_at,
                created_by        AS "created_by: ActorId",
                created_at,
                updated_at
              FROM discovery_job
             WHERE id = $1 AND tenant_id = $2
            "#,
            id,
            scope.tenant_id() as TenantId,
        )
        .fetch_one(self.pool())
        .await
        .map_err(|e| map("discovery_job", id.to_string(), e))?;

        row.parse()
    }

    /// Delete a job.
    ///
    /// Its runs survive, with a null `job_id`. §2.7: deleting a job must not delete the
    /// record that it once scanned somebody's network.
    ///
    /// # Errors
    ///
    /// `NotFound` when there is no such job in this tenant.
    pub async fn delete_discovery_job(&self, scope: &TenantScope, id: uuid::Uuid) -> Result<()> {
        // tenant-exempt: the tenant is a bound parameter, from the scope.
        let done = sqlx::query!(
            "DELETE FROM discovery_job WHERE id = $1 AND tenant_id = $2",
            id,
            scope.tenant_id() as TenantId,
        )
        .execute(self.pool())
        .await
        .map_err(|e| map("discovery_job", id.to_string(), e))?;

        if done.rows_affected() == 0 {
            return Err(CoreError::NotFound {
                kind: "discovery_job",
                id: id.to_string(),
            });
        }
        Ok(())
    }
}

// ----------------------------------------------------------------------------
// Runs
// ----------------------------------------------------------------------------

impl PgStore {
    /// Record that a sweep has started.
    ///
    /// The ranges are snapshotted here rather than read back through the job later,
    /// because a job's ranges are editable and the audit question — "who scanned
    /// 10.0.0.0/16 on Tuesday" — is about the ranges as they were.
    ///
    /// # Errors
    ///
    /// `Invalid` when the ranges exceed what one run may probe, or when `job_id` names a
    /// job in another tenant.
    pub async fn start_discovery_run(
        &self,
        scope: &TenantScope,
        job_id: Option<uuid::Uuid>,
        ranges: &[Range],
        trigger: Trigger,
        by: Option<ActorId>,
    ) -> Result<DiscoveryRun> {
        Sweep::check(ranges).map_err(|e| CoreError::Invalid(e.to_string()))?;
        let text = as_text(ranges);

        // tenant-exempt: the tenant is a bound parameter, from the scope.
        let row = sqlx::query_as!(
            RunRow,
            r#"
            INSERT INTO discovery_run (tenant_id, job_id, ranges, trigger, started_by)
            VALUES ($1, $2, $3::text[]::cidr[], $4, $5)
            RETURNING
                id,
                tenant_id      AS "tenant_id: TenantId",
                job_id,
                ranges::text[] AS "ranges!: Vec<String>",
                trigger,
                started_by     AS "started_by: ActorId",
                started_at,
                finished_at,
                status,
                error,
                probed, answered, created, merged, for_review, candidates, edges
            "#,
            scope.tenant_id() as TenantId,
            job_id,
            &text,
            trigger.as_str(),
            by as Option<ActorId>,
        )
        .fetch_one(self.pool())
        .await
        .map_err(|e| map("discovery_run", "start".to_owned(), e))?;

        row.parse()
    }

    /// Record that a sweep has ended, with what it did.
    ///
    /// `error` is required when `status` is [`RunStatus::Failed`] and forbidden
    /// otherwise — the schema has the same CHECK, because a failed run with no reason is
    /// a red row an operator cannot act on.
    ///
    /// Also stamps the job's `last_run_at`, which is what the scheduler reads to decide
    /// what is due. Done in the same statement rather than by the caller, so a run that
    /// finished cannot leave a job looking as though it never ran.
    ///
    /// # Errors
    ///
    /// `NotFound` for a run in another tenant. `Invalid` when the counters contradict
    /// themselves — more answered than probed, a failure with no reason.
    pub async fn finish_discovery_run(
        &self,
        scope: &TenantScope,
        id: uuid::Uuid,
        status: RunStatus,
        counts: RunCounts,
        error: Option<&str>,
    ) -> Result<DiscoveryRun> {
        if status == RunStatus::Running {
            return Err(CoreError::Invalid(
                "a run cannot be finished as still running".to_owned(),
            ));
        }

        let mut tx = self
            .pool()
            .begin()
            .await
            .map_err(|e| map("discovery_run", id.to_string(), e))?;

        // tenant-exempt: the tenant is a bound parameter, from the scope.
        let row = sqlx::query_as!(
            RunRow,
            r#"
            UPDATE discovery_run
               SET status = $3, finished_at = now(), error = $4,
                   probed = $5, answered = $6, created = $7, merged = $8,
                   for_review = $9, candidates = $10, edges = $11
             WHERE id = $1 AND tenant_id = $2 AND status = 'running'
            RETURNING
                id,
                tenant_id      AS "tenant_id: TenantId",
                job_id,
                ranges::text[] AS "ranges!: Vec<String>",
                trigger,
                started_by     AS "started_by: ActorId",
                started_at,
                finished_at,
                status,
                error,
                probed, answered, created, merged, for_review, candidates, edges
            "#,
            id,
            scope.tenant_id() as TenantId,
            status.as_str(),
            error,
            counts.probed,
            counts.answered,
            counts.created,
            counts.merged,
            counts.for_review,
            counts.candidates,
            counts.edges,
        )
        .fetch_one(&mut *tx)
        .await
        .map_err(|e| map("discovery_run", id.to_string(), e))?;

        if let Some(job_id) = row.job_id {
            // tenant-exempt: the tenant is a bound parameter, from the scope.
            sqlx::query!(
                "UPDATE discovery_job SET last_run_at = now() WHERE id = $1 AND tenant_id = $2",
                job_id,
                scope.tenant_id() as TenantId,
            )
            .execute(&mut *tx)
            .await
            .map_err(|e| map("discovery_job", job_id.to_string(), e))?;
        }

        tx.commit()
            .await
            .map_err(|e| map("discovery_run", id.to_string(), e))?;

        row.parse()
    }

    /// A tenant's runs, newest first.
    pub async fn discovery_runs(
        &self,
        scope: &TenantScope,
        limit: i64,
    ) -> Result<Vec<DiscoveryRun>> {
        // tenant-exempt: the tenant is a bound parameter, from the scope.
        let rows = sqlx::query_as!(
            RunRow,
            r#"
            SELECT
                id,
                tenant_id      AS "tenant_id: TenantId",
                job_id,
                ranges::text[] AS "ranges!: Vec<String>",
                trigger,
                started_by     AS "started_by: ActorId",
                started_at,
                finished_at,
                status,
                error,
                probed, answered, created, merged, for_review, candidates, edges
              FROM discovery_run
             WHERE tenant_id = $1
             ORDER BY started_at DESC
             LIMIT $2
            "#,
            scope.tenant_id() as TenantId,
            limit.clamp(1, 500),
        )
        .fetch_all(self.pool())
        .await
        .map_err(|e| map("discovery_run", "list".to_owned(), e))?;

        rows.into_iter().map(RunRow::parse).collect()
    }
}

// ----------------------------------------------------------------------------
// Candidates
// ----------------------------------------------------------------------------

impl PgStore {
    /// Record something a run found and could not turn into a resource.
    ///
    /// Upserts on the fingerprint, so the same printer found by a nightly sweep is one
    /// row that keeps its `first_seen` and updates its `last_seen`. A row per sighting
    /// would put 14 600 rows a year in front of an operator to describe 40 printers.
    ///
    /// An `ignored` candidate stays ignored. That is the whole point of the state: an
    /// operator who has said "stop showing me this" must not be shown it again by the
    /// next run, and a plain upsert would reset it every night.
    ///
    /// # Errors
    ///
    /// `Invalid` when the candidate has neither an address nor a chassis ID — it is then
    /// not a thing, and its fingerprint would collide with every other empty row.
    pub async fn record_candidate(
        &self,
        scope: &TenantScope,
        run_id: Option<uuid::Uuid>,
        source: CandidateSource,
        new: &NewCandidate,
    ) -> Result<DiscoveryCandidate> {
        if new.address.is_none() && new.chassis_id.is_none() {
            return Err(CoreError::Invalid(
                "a discovery candidate needs an address or a chassis id — without either \
                 there is nothing to look for again"
                    .to_owned(),
            ));
        }

        let address = new.address.map(|a| a.to_string());

        // tenant-exempt: the tenant is a bound parameter, from the scope.
        let row = sqlx::query_as!(
            CandidateRow,
            r#"
            INSERT INTO discovery_candidate
                (tenant_id, last_run_id, source, address, chassis_id, port_id, platform,
                 sys_name, sys_descr, sys_object_id, mac, seen_from, state, reason)
            VALUES
                ($1, $2, $3, $4::text::inet, $5, $6, $7, $8, $9, $10, $11::text::macaddr,
                 $12, $13, $14)
            ON CONFLICT (tenant_id, fingerprint) DO UPDATE
               SET last_run_id   = EXCLUDED.last_run_id,
                   last_seen     = now(),
                   port_id       = COALESCE(EXCLUDED.port_id, discovery_candidate.port_id),
                   platform      = COALESCE(EXCLUDED.platform, discovery_candidate.platform),
                   sys_name      = COALESCE(EXCLUDED.sys_name, discovery_candidate.sys_name),
                   sys_descr     = COALESCE(EXCLUDED.sys_descr, discovery_candidate.sys_descr),
                   sys_object_id = COALESCE(EXCLUDED.sys_object_id,
                                            discovery_candidate.sys_object_id),
                   mac           = COALESCE(EXCLUDED.mac, discovery_candidate.mac),
                   seen_from     = COALESCE(EXCLUDED.seen_from, discovery_candidate.seen_from),
                   -- An operator's decision outlives the next run. Promoted stays
                   -- promoted and ignored stays ignored; only a candidate still on the
                   -- list can have its state rewritten by what a sweep just saw.
                   state         = CASE
                                     WHEN discovery_candidate.state IN ('promoted', 'ignored')
                                     THEN discovery_candidate.state
                                     ELSE EXCLUDED.state
                                   END,
                   reason        = CASE
                                     WHEN discovery_candidate.state IN ('promoted', 'ignored')
                                     THEN discovery_candidate.reason
                                     ELSE EXCLUDED.reason
                                   END
            RETURNING
                id,
                tenant_id     AS "tenant_id: TenantId",
                last_run_id,
                source,
                -- host() rather than a cast: `inet::text` renders 192.168.1.9/32, mask
                -- and all, which is not an address any client library will parse.
                host(address) AS "address: String",
                chassis_id,
                port_id,
                platform,
                sys_name,
                sys_descr,
                sys_object_id,
                mac::text     AS "mac: String",
                seen_from     AS "seen_from: uops_core::ResourceId",
                state,
                resource_id   AS "resource_id: uops_core::ResourceId",
                reason,
                first_seen,
                last_seen
            "#,
            scope.tenant_id() as TenantId,
            run_id,
            source.as_str(),
            address,
            new.chassis_id,
            new.port_id,
            new.platform,
            new.sys_name,
            new.sys_descr,
            new.sys_object_id,
            new.mac,
            new.seen_from as Option<uops_core::ResourceId>,
            new.state.as_str(),
            new.reason,
        )
        .fetch_one(self.pool())
        .await
        .map_err(|e| map("discovery_candidate", "record".to_owned(), e))?;

        row.parse()
    }

    /// The candidates an operator still has to deal with, newest sighting first.
    ///
    /// Outstanding only. Promoted and ignored rows are history and are the bulk of the
    /// table after a month; `discovery_candidate_outstanding_idx` is partial for the same
    /// reason.
    pub async fn discovery_candidates(
        &self,
        scope: &TenantScope,
        limit: i64,
    ) -> Result<Vec<DiscoveryCandidate>> {
        // tenant-exempt: the tenant is a bound parameter, from the scope.
        let rows = sqlx::query_as!(
            CandidateRow,
            r#"
            SELECT
                id,
                tenant_id     AS "tenant_id: TenantId",
                last_run_id,
                source,
                -- host() rather than a cast: `inet::text` renders 192.168.1.9/32, mask
                -- and all, which is not an address any client library will parse.
                host(address) AS "address: String",
                chassis_id,
                port_id,
                platform,
                sys_name,
                sys_descr,
                sys_object_id,
                mac::text     AS "mac: String",
                seen_from     AS "seen_from: uops_core::ResourceId",
                state,
                resource_id   AS "resource_id: uops_core::ResourceId",
                reason,
                first_seen,
                last_seen
              FROM discovery_candidate
             WHERE tenant_id = $1
               AND state IN ('unidentified', 'ambiguous', 'unreachable')
             ORDER BY last_seen DESC
             LIMIT $2
            "#,
            scope.tenant_id() as TenantId,
            limit.clamp(1, 500),
        )
        .fetch_all(self.pool())
        .await
        .map_err(|e| map("discovery_candidate", "list".to_owned(), e))?;

        rows.into_iter().map(CandidateRow::parse).collect()
    }

    /// Stop showing a candidate.
    ///
    /// Not a delete: the next run would re-create it, and the operator would have to
    /// dismiss the same printer every morning.
    ///
    /// # Errors
    ///
    /// `NotFound` when there is no such candidate in this tenant.
    pub async fn ignore_candidate(
        &self,
        scope: &TenantScope,
        id: uuid::Uuid,
        by: Option<ActorId>,
        reason: &str,
    ) -> Result<()> {
        // tenant-exempt: the tenant is a bound parameter, from the scope.
        let done = sqlx::query!(
            r#"
            UPDATE discovery_candidate
               SET state = 'ignored', ignored_at = now(), ignored_by = $3, reason = $4
             WHERE id = $1 AND tenant_id = $2 AND state <> 'promoted'
            "#,
            id,
            scope.tenant_id() as TenantId,
            by as Option<ActorId>,
            reason,
        )
        .execute(self.pool())
        .await
        .map_err(|e| map("discovery_candidate", id.to_string(), e))?;

        if done.rows_affected() == 0 {
            return Err(CoreError::NotFound {
                kind: "discovery_candidate",
                id: id.to_string(),
            });
        }
        Ok(())
    }
}
