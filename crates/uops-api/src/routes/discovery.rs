//! Discovery jobs, runs and candidates — M5, `docs/M5-discovery.md`.
//!
//! # Roles
//!
//! Reading is `Viewer`. Writing a job, deleting one, and dismissing a candidate are
//! `Operator` — §2.7. A discovery job is an instruction to send packets across somebody's
//! network, and the list of ranges is a description of their estate; neither is a thing a
//! read-only account should be able to change or create.
//!
//! # Why the ranges are in the audit entry
//!
//! `searches.rs` deliberately keeps a saved search's *terms* out of the audit log: they
//! are the customer's hostnames and error strings, and an audit row carrying them would
//! put a second copy of the data in the audit table.
//!
//! Discovery is the opposite case and the contrast is worth stating, because the two
//! rules look contradictory. §2.7: *"the audit entry carries the ranges, because 'who
//! scanned 10.0.0.0/16 on Tuesday' is the question that gets asked."* The ranges are not
//! incidental data that happened to pass through — they are the *action*. An audit entry
//! that recorded "somebody created a discovery job" without saying what it would scan
//! answers nothing that a security team asks.
//!
//! # What is not here yet
//!
//! Running a sweep. `POST /discovery/jobs/{id}/run` needs a runner inside the server
//! process — a sweep takes minutes, so it cannot be the body of a request — and that
//! arrives with the scheduler. The store's `start_discovery_run` and
//! `finish_discovery_run` are already what it will call.

use axum::Json;
use axum::extract::{Path, Query as UrlQuery, State};
use axum::http::StatusCode;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uops_core::{CredentialRef, Role, SiteId};
use uops_discover::Range;
use uops_store_pg::discovery_jobs::{
    CandidateSource, CandidateState, DiscoveryCandidate, DiscoveryJob, DiscoveryRun, NewJob,
    RunStatus, Trigger,
};

use crate::csrf::CsrfChecked;
use crate::error::{ApiError, ApiResult};
use crate::extract::Caller;
use crate::state::AppState;

/// How many rows a list returns without being asked.
///
/// A candidate list is something an operator works down, not a dataset — if there are
/// more than a hundred outstanding candidates the answer is a discovery job, not a longer
/// page.
const DEFAULT_LIMIT: i64 = 100;

// ----------------------------------------------------------------------------
// Views
// ----------------------------------------------------------------------------

/// A job, as the UI shows it.
#[derive(Debug, Serialize)]
pub struct JobView {
    pub id: uuid::Uuid,
    pub name: String,
    pub description: String,
    /// CIDR strings. `Range` renders and parses these, so what goes out is what comes
    /// back in.
    pub ranges: Vec<String>,
    /// How many addresses this job would probe, so the UI can say "512 addresses" beside
    /// the ranges rather than making an operator do the arithmetic for a /22.
    pub addresses: u64,
    pub site_id: Option<SiteId>,
    /// How many credentials, never which. A credential reference is not secret, but a
    /// list of them on an inventory screen is an invitation to start correlating, and
    /// nothing on this screen needs them.
    pub credentials: usize,
    pub snmp_port: u16,
    pub skip_silent_hosts: bool,
    /// `None` means manual only.
    pub schedule_seconds: Option<u64>,
    pub enabled: bool,
    pub last_run_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

fn job_view(j: DiscoveryJob) -> JobView {
    JobView {
        addresses: j.ranges.iter().map(|r| r.addresses()).sum(),
        ranges: j.ranges.iter().map(ToString::to_string).collect(),
        id: j.id,
        name: j.name,
        description: j.description,
        site_id: j.site_id,
        credentials: j.credential_refs.len(),
        snmp_port: j.snmp_port,
        skip_silent_hosts: j.skip_silent_hosts,
        schedule_seconds: j.schedule.map(|d| d.as_secs()),
        enabled: j.enabled,
        last_run_at: j.last_run_at,
        created_at: j.created_at,
        updated_at: j.updated_at,
    }
}

/// A run, as the history shows it.
#[derive(Debug, Serialize)]
pub struct RunView {
    pub id: uuid::Uuid,
    pub job_id: Option<uuid::Uuid>,
    pub ranges: Vec<String>,
    pub trigger: String,
    pub status: String,
    pub error: Option<String>,
    pub started_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
    pub probed: i32,
    pub answered: i32,
    /// `probed - answered`: the addresses with nothing on them.
    ///
    /// Computed here rather than in the client because it is the number that tells an
    /// operator whether their credential list is right — 4 000 probed and 0 answered is a
    /// wrong community string, not an empty network — and a derived number that two
    /// clients might derive differently is a number nobody trusts.
    pub silent: i32,
    pub created: i32,
    pub merged: i32,
    pub for_review: i32,
    pub candidates: i32,
    pub edges: i32,
}

fn run_view(r: DiscoveryRun) -> RunView {
    RunView {
        ranges: r.ranges.iter().map(ToString::to_string).collect(),
        silent: r.counts.probed - r.counts.answered,
        id: r.id,
        job_id: r.job_id,
        trigger: trigger_str(r.trigger).to_owned(),
        status: status_str(r.status).to_owned(),
        error: r.error,
        started_at: r.started_at,
        finished_at: r.finished_at,
        probed: r.counts.probed,
        answered: r.counts.answered,
        created: r.counts.created,
        merged: r.counts.merged,
        for_review: r.counts.for_review,
        candidates: r.counts.candidates,
        edges: r.counts.edges,
    }
}

/// A candidate, as the list an operator works through shows it.
#[derive(Debug, Serialize)]
pub struct CandidateView {
    pub id: uuid::Uuid,
    pub source: String,
    pub address: Option<String>,
    pub chassis_id: Option<String>,
    pub port_id: Option<String>,
    pub platform: Option<String>,
    pub sys_name: Option<String>,
    pub sys_descr: Option<String>,
    pub mac: Option<String>,
    /// Which device reported it. The first thing an operator wants to know about an
    /// unexpected machine on their network.
    pub seen_from: Option<uops_core::ResourceId>,
    pub state: String,
    /// Why it is still here, in a sentence. Shown as-is: a candidate with no reason is
    /// one an operator can only shrug at.
    pub reason: String,
    pub first_seen: DateTime<Utc>,
    pub last_seen: DateTime<Utc>,
}

fn candidate_view(c: DiscoveryCandidate) -> CandidateView {
    CandidateView {
        address: c.address.map(|a| a.to_string()),
        source: source_str(c.source).to_owned(),
        state: state_str(c.state).to_owned(),
        id: c.id,
        chassis_id: c.chassis_id,
        port_id: c.port_id,
        platform: c.platform,
        sys_name: c.sys_name,
        sys_descr: c.sys_descr,
        mac: c.mac,
        seen_from: c.seen_from,
        reason: c.reason,
        first_seen: c.first_seen,
        last_seen: c.last_seen,
    }
}

const fn trigger_str(t: Trigger) -> &'static str {
    t.as_str()
}
const fn status_str(s: RunStatus) -> &'static str {
    s.as_str()
}
const fn source_str(s: CandidateSource) -> &'static str {
    s.as_str()
}
const fn state_str(s: CandidateState) -> &'static str {
    s.as_str()
}

// ----------------------------------------------------------------------------
// Jobs
// ----------------------------------------------------------------------------

/// `GET /api/v1/discovery/jobs`
pub async fn list_jobs(
    State(state): State<AppState>,
    caller: Caller,
) -> ApiResult<Json<Vec<JobView>>> {
    caller.require(Role::Viewer)?;

    let jobs = state.store.discovery_jobs(caller.scope()).await?;
    caller.audit().read(
        "discovery.jobs.list",
        Some(i64::try_from(jobs.len()).unwrap_or(i64::MAX)),
    );

    Ok(Json(jobs.into_iter().map(job_view).collect()))
}

/// `GET /api/v1/discovery/jobs/{id}`
pub async fn get_job(
    State(state): State<AppState>,
    caller: Caller,
    Path(id): Path<uuid::Uuid>,
) -> ApiResult<Json<JobView>> {
    caller.require(Role::Viewer)?;

    let job = state.store.discovery_job(caller.scope(), id).await?;
    caller.audit().read("discovery.jobs.get", None);

    Ok(Json(job_view(job)))
}

/// What a client sends to create a job.
#[derive(Debug, Deserialize)]
pub struct JobRequest {
    pub name: String,
    #[serde(default)]
    pub description: String,
    /// CIDR strings, as an operator types them.
    pub ranges: Vec<String>,
    #[serde(default)]
    pub site_id: Option<SiteId>,
    pub credential_refs: Vec<CredentialRef>,
    #[serde(default = "default_port")]
    pub snmp_port: u16,
    #[serde(default)]
    pub skip_silent_hosts: bool,
    #[serde(default)]
    pub schedule_seconds: Option<u64>,
}

const fn default_port() -> u16 {
    161
}

impl JobRequest {
    /// Parse the ranges, in the caller's own terms.
    ///
    /// The error names the range that failed. "not an IPv4 range" about a list of six is
    /// a message that sends somebody back to check all six.
    fn into_new(self) -> Result<NewJob, ApiError> {
        let ranges = self
            .ranges
            .iter()
            .map(|r| {
                r.parse::<Range>()
                    .map_err(|e| ApiError::from(uops_core::Error::Invalid(e.to_string())))
            })
            .collect::<Result<Vec<_>, _>>()?;

        Ok(NewJob {
            name: self.name,
            description: self.description,
            ranges,
            site_id: self.site_id,
            credential_refs: self.credential_refs,
            snmp_port: self.snmp_port,
            skip_silent_hosts: self.skip_silent_hosts,
            schedule: self.schedule_seconds.map(std::time::Duration::from_secs),
        })
    }
}

/// `POST /api/v1/discovery/jobs`
///
/// The ranges are checked here with the same gate the sweeper uses, so an operator who
/// typed a /8 is told to split it rather than shown the name of a CHECK constraint.
pub async fn create_job(
    State(state): State<AppState>,
    caller: Caller,
    _csrf: CsrfChecked,
    Json(body): Json<JobRequest>,
) -> ApiResult<(StatusCode, Json<JobView>)> {
    caller.require(Role::Operator)?;

    let new = body.into_new()?;
    let job = state
        .store
        .create_discovery_job(caller.scope(), Some(caller.user_id()), &new)
        .await?;

    // The ranges, deliberately. See the module docs: here they are the action rather than
    // incidental data, and §2.7 requires them.
    caller.audit().wrote(
        "discovery.jobs.create",
        format!("discovery_job:{}", job.id),
        None,
        Some(serde_json::json!({
            "name": job.name,
            "ranges": job.ranges.iter().map(ToString::to_string).collect::<Vec<_>>(),
            "addresses": job.ranges.iter().map(|r| r.addresses()).sum::<u64>(),
            "schedule_seconds": job.schedule.map(|d| d.as_secs()),
        })),
    );

    Ok((StatusCode::CREATED, Json(job_view(job))))
}

/// `DELETE /api/v1/discovery/jobs/{id}`
///
/// The job's runs survive it, with a null `job_id`: deleting a job must not delete the
/// record that it once scanned somebody's network.
pub async fn delete_job(
    State(state): State<AppState>,
    caller: Caller,
    Path(id): Path<uuid::Uuid>,
    _csrf: CsrfChecked,
) -> ApiResult<StatusCode> {
    caller.require(Role::Operator)?;

    // Read first, so the audit entry can say what was deleted. A row that is gone cannot
    // be described afterwards, and "deleted a discovery job" without the ranges is the
    // entry §2.7 exists to prevent.
    let before = state.store.discovery_job(caller.scope(), id).await?;
    state.store.delete_discovery_job(caller.scope(), id).await?;

    caller.audit().wrote(
        "discovery.jobs.delete",
        format!("discovery_job:{id}"),
        Some(serde_json::json!({
            "name": before.name,
            "ranges": before.ranges.iter().map(ToString::to_string).collect::<Vec<_>>(),
        })),
        None,
    );

    Ok(StatusCode::NO_CONTENT)
}

// ----------------------------------------------------------------------------
// Runs and candidates
// ----------------------------------------------------------------------------

/// How many rows to return.
#[derive(Debug, Deserialize)]
pub struct Limit {
    #[serde(default)]
    pub limit: Option<i64>,
}

impl Limit {
    const fn or_default(&self) -> i64 {
        match self.limit {
            Some(n) => n,
            None => DEFAULT_LIMIT,
        }
    }
}

/// `GET /api/v1/discovery/runs`
///
/// Newest first. The store clamps the limit; this does not re-clamp it, because two
/// places deciding the same bound is how they come to disagree.
pub async fn list_runs(
    State(state): State<AppState>,
    caller: Caller,
    UrlQuery(limit): UrlQuery<Limit>,
) -> ApiResult<Json<Vec<RunView>>> {
    caller.require(Role::Viewer)?;

    let runs = state
        .store
        .discovery_runs(caller.scope(), limit.or_default())
        .await?;
    caller.audit().read(
        "discovery.runs.list",
        Some(i64::try_from(runs.len()).unwrap_or(i64::MAX)),
    );

    Ok(Json(runs.into_iter().map(run_view).collect()))
}

/// `GET /api/v1/discovery/candidates`
///
/// Outstanding only — promoted and ignored rows are history and are the bulk of the table
/// after a month.
pub async fn list_candidates(
    State(state): State<AppState>,
    caller: Caller,
    UrlQuery(limit): UrlQuery<Limit>,
) -> ApiResult<Json<Vec<CandidateView>>> {
    caller.require(Role::Viewer)?;

    let candidates = state
        .store
        .discovery_candidates(caller.scope(), limit.or_default())
        .await?;
    caller.audit().read(
        "discovery.candidates.list",
        Some(i64::try_from(candidates.len()).unwrap_or(i64::MAX)),
    );

    Ok(Json(candidates.into_iter().map(candidate_view).collect()))
}

/// Why an operator is dismissing a candidate.
#[derive(Debug, Deserialize)]
pub struct IgnoreRequest {
    /// Replaces the reason discovery wrote.
    ///
    /// Required and not defaulted: the list is worked through by people, and "ignored"
    /// with no note is a row the next person cannot act on either. A sentence costs
    /// nothing now and is the whole value of the row in six months.
    pub reason: String,
}

/// `POST /api/v1/discovery/candidates/{id}/ignore`
///
/// Not a delete. The next run would re-create it, and an operator would dismiss the same
/// printer every morning.
pub async fn ignore_candidate(
    State(state): State<AppState>,
    caller: Caller,
    Path(id): Path<uuid::Uuid>,
    _csrf: CsrfChecked,
    Json(body): Json<IgnoreRequest>,
) -> ApiResult<StatusCode> {
    caller.require(Role::Operator)?;

    if body.reason.trim().is_empty() {
        return Err(uops_core::Error::Invalid(
            "say why this is being ignored — the next person to read the list needs it \
             more than you do"
                .to_owned(),
        )
        .into());
    }

    state
        .store
        .ignore_candidate(caller.scope(), id, Some(caller.user_id()), &body.reason)
        .await?;

    caller.audit().wrote(
        "discovery.candidates.ignore",
        format!("discovery_candidate:{id}"),
        None,
        Some(serde_json::json!({ "reason": body.reason })),
    );

    Ok(StatusCode::NO_CONTENT)
}
