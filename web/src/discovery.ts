/**
 * Discovery: jobs, runs, and the candidates an operator works through.
 *
 * Three shapes for three questions, and they are deliberately not one:
 *
 * - a **job** is a standing instruction — what to scan, how often, with which
 *   credentials. It changes rarely and is read as an inventory.
 * - a **run** is one execution. It is append-only and read newest-first.
 * - a **candidate** is the residue: everything a run found and could not turn into a
 *   device, with a sentence saying why. It is a worklist.
 *
 * The server does the arithmetic that two clients could otherwise do differently —
 * `addresses` on a job and `silent` on a run are computed there, because a derived number
 * that disagrees between two screens is a number nobody trusts.
 */

import { request } from "./api";

/** A standing instruction to sweep some ranges. */
export interface DiscoveryJob {
  id: string;
  name: string;
  description: string;
  /** CIDR strings, normalised: what an operator typed with host bits set comes back as
   *  the network it meant. */
  ranges: string[];
  /** How many addresses this job would probe, summed across its ranges. */
  addresses: number;
  site_id: string | null;
  /** How many credentials, never which. */
  credentials: number;
  snmp_port: number;
  skip_silent_hosts: boolean;
  /** `null` means manual only. */
  schedule_seconds: number | null;
  enabled: boolean;
  last_run_at: string | null;
  created_at: string;
  updated_at: string;
}

export interface JobRequest {
  name: string;
  description?: string;
  ranges: string[];
  site_id?: string | null;
  credential_refs: string[];
  snmp_port?: number;
  skip_silent_hosts?: boolean;
  schedule_seconds?: number | null;
}

export type RunStatus = "running" | "succeeded" | "failed" | "cancelled";
export type RunTrigger = "schedule" | "manual" | "probe";

/** One execution of a sweep. */
export interface DiscoveryRun {
  id: string;
  job_id: string | null;
  /** What was actually scanned, snapshotted — not read back through the job, whose
   *  ranges are editable. */
  ranges: string[];
  trigger: RunTrigger;
  status: RunStatus;
  error: string | null;
  started_at: string;
  finished_at: string | null;
  probed: number;
  /** Including refusals: an agent that rejected the credentials proved it exists. */
  answered: number;
  /** `probed - answered` — the addresses with nothing on them. */
  silent: number;
  created: number;
  merged: number;
  for_review: number;
  candidates: number;
  edges: number;
}

export type CandidateSource = "sweep" | "lldp" | "cdp" | "arp";
export type CandidateState =
  | "unidentified"
  | "ambiguous"
  | "unreachable"
  | "promoted"
  | "ignored";

/** Something a run found and did not turn into a device. */
export interface DiscoveryCandidate {
  id: string;
  source: CandidateSource;
  address: string | null;
  chassis_id: string | null;
  port_id: string | null;
  platform: string | null;
  sys_name: string | null;
  sys_descr: string | null;
  mac: string | null;
  /** Which device reported it. The first thing to want to know about an unexpected
   *  machine. */
  seen_from: string | null;
  state: CandidateState;
  /** Why it is still here, in a sentence. Shown as the server wrote it. */
  reason: string;
  first_seen: string;
  last_seen: string;
}

export function listJobs(tenant: string) {
  return request<DiscoveryJob[]>("/api/v1/discovery/jobs", { tenant });
}

export function createJob(tenant: string, body: JobRequest) {
  return request<DiscoveryJob>("/api/v1/discovery/jobs", {
    method: "POST",
    body,
    tenant,
  });
}

export function deleteJob(tenant: string, id: string) {
  return request<void>(`/api/v1/discovery/jobs/${id}`, {
    method: "DELETE",
    tenant,
  });
}

export function listRuns(tenant: string, limit = 50) {
  return request<DiscoveryRun[]>(`/api/v1/discovery/runs?limit=${limit}`, {
    tenant,
  });
}

export function listCandidates(tenant: string, limit = 100) {
  return request<DiscoveryCandidate[]>(
    `/api/v1/discovery/candidates?limit=${limit}`,
    { tenant },
  );
}

export function ignoreCandidate(tenant: string, id: string, reason: string) {
  return request<void>(`/api/v1/discovery/candidates/${id}/ignore`, {
    method: "POST",
    body: { reason },
    tenant,
  });
}

/**
 * A schedule as a phrase, or null for a job that only runs by hand.
 *
 * Whole units only, because the schema already refuses anything under an hour or over
 * thirty days — so "every 90 minutes" cannot occur and does not need a spelling.
 */
export function scheduleLabel(seconds: number | null): string | null {
  if (seconds === null) return null;
  const hours = Math.round(seconds / 3600);
  if (hours < 24) return hours === 1 ? "hourly" : `every ${hours} hours`;
  const days = Math.round(hours / 24);
  if (days === 1) return "daily";
  if (days === 7) return "weekly";
  return `every ${days} days`;
}

/**
 * How many addresses, written the way somebody says it out loud.
 *
 * 65 534 is the interesting number — it is the documented ceiling — so it stays exact
 * rather than becoming "66k". Nothing here is large enough to need a different unit.
 */
export function addressCount(n: number): string {
  return n.toLocaleString();
}

/**
 * What the run's outcome was, in one word, for the status dot.
 *
 * Mapped rather than passed through, because `RunStatus` is the server's vocabulary and
 * the token system's is `ok | warn | crit | unknown` — the same four states every other
 * status indicator in the app uses. A run that found nothing is still a run that worked.
 */
export function runTone(run: DiscoveryRun): "ok" | "warn" | "crit" | "unknown" {
  switch (run.status) {
    case "succeeded":
      // Probed a network and nothing answered at all. Not a failure of the run, and not
      // a result to show in the same colour as one that worked: it is almost always a
      // credential the job does not have.
      return run.probed > 0 && run.answered === 0 ? "warn" : "ok";
    case "failed":
      return "crit";
    case "running":
      return "unknown";
    case "cancelled":
      return "warn";
  }
}

/** How much a sighting from this source is worth, for the candidate list's badge. */
export function sourceWeight(source: CandidateSource): string {
  switch (source) {
    case "lldp":
      return "a neighbour reported it over LLDP";
    case "cdp":
      return "a neighbour reported it over CDP";
    case "arp":
      // Worth saying on the row itself. An ARP entry proves an address is in use on a
      // subnet and nothing more, and most of them are laptops.
      return "seen in an ARP table — weak evidence";
    case "sweep":
      return "answered a sweep of its address";
  }
}
