/**
 * Finding devices, on three screens.
 *
 * * **Jobs** — the standing instructions. What to scan, how often, and how big it is.
 * * **Runs** — what each sweep did, as six numbers an operator reads across.
 * * **Candidates** — everything discovery found and could not turn into a device, with a
 *   sentence saying why. This is the worklist, and it is the screen that matters.
 *
 * # Why candidates are a first-class screen and not a tab
 *
 * A product that silently drops what it cannot classify is one whose inventory an
 * operator cannot trust — `docs/M5-discovery.md` §3. The corollary is that the residue
 * has to be somewhere a person actually looks. Hiding it behind a job would make it
 * findable only by somebody who already suspected it existed.
 *
 * # Why nothing here starts a sweep yet
 *
 * A sweep takes minutes, so it cannot be the body of a request. The runner that will own
 * it arrives with the scheduler; until then the jobs screen says so plainly rather than
 * offering a button that would appear broken.
 */

import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Link } from "@tanstack/react-router";
import { useState } from "react";

import { ago } from "./alerting";
import {
  addressCount,
  createJob,
  deleteJob,
  ignoreCandidate,
  listCandidates,
  listJobs,
  listRuns,
  runTone,
  scheduleLabel,
  sourceWeight,
  type DiscoveryCandidate,
  type DiscoveryJob,
  type DiscoveryRun,
} from "./discovery";
import { message } from "./query";
import { useShell } from "./shell";

function mayWrite(role: string): boolean {
  return role === "operator" || role === "admin";
}

/** The three screens, linked from each of them. */
function Tabs() {
  return (
    <p className="dim">
      <Link to="/discovery">Jobs</Link> · <Link to="/discovery/runs">Runs</Link> ·{" "}
      <Link to="/discovery/candidates">Candidates</Link>
    </p>
  );
}

// ----------------------------------------------------------------------------
// Jobs
// ----------------------------------------------------------------------------

export function DiscoveryPage() {
  const { tenant } = useShell();
  const client = useQueryClient();
  const [problem, setProblem] = useState<string | null>(null);
  const [adding, setAdding] = useState(false);

  const jobs = useQuery({
    queryKey: ["discovery-jobs", tenant.tenant_id],
    queryFn: () => listJobs(tenant.tenant_id),
    retry: false,
  });

  const refresh = async () => {
    setProblem(null);
    await client.invalidateQueries({
      queryKey: ["discovery-jobs", tenant.tenant_id],
    });
  };

  const remove = useMutation({
    mutationFn: (id: string) => deleteJob(tenant.tenant_id, id),
    onSuccess: refresh,
    onError: (error) => setProblem(message(error)),
  });

  const rows = jobs.data ?? [];

  return (
    <>
      <h1>Discovery</h1>
      <Tabs />

      {jobs.isError && (
        <div className="problem" role="alert">
          {message(jobs.error)}
        </div>
      )}
      {problem && (
        <div className="problem" role="alert">
          {problem}
        </div>
      )}

      <p className="dim">
        {rows.length === 0
          ? "No discovery jobs. A job is a list of ranges to sweep and the credentials it may try."
          : `${rows.length} job${rows.length === 1 ? "" : "s"}.`}{" "}
        Jobs do not run on a schedule yet — the scheduler is the next piece.
      </p>

      {mayWrite(tenant.role) && (
        <p>
          <button type="button" onClick={() => setAdding((open) => !open)}>
            {adding ? "Cancel" : "New job"}
          </button>
        </p>
      )}

      {adding && (
        <NewJob
          onSaved={async () => {
            setAdding(false);
            await refresh();
          }}
          onProblem={setProblem}
        />
      )}

      {rows.length > 0 && (
        <div className="scroll-x">
          <table>
            <thead>
              <tr>
                <th>Name</th>
                <th>Ranges</th>
                <th>Addresses</th>
                <th>Schedule</th>
                <th>Last run</th>
                <th />
              </tr>
            </thead>
            <tbody>
              {rows.map((job) => (
                <JobRow
                  key={job.id}
                  job={job}
                  mayWrite={mayWrite(tenant.role)}
                  onDelete={() => remove.mutate(job.id)}
                />
              ))}
            </tbody>
          </table>
        </div>
      )}
    </>
  );
}

function JobRow({
  job,
  mayWrite: writable,
  onDelete,
}: {
  job: DiscoveryJob;
  mayWrite: boolean;
  onDelete: () => void;
}) {
  const schedule = scheduleLabel(job.schedule_seconds);
  return (
    <tr>
      <td>
        {job.name}
        {!job.enabled && <span className="dim"> · disabled</span>}
      </td>
      <td className="mono">{job.ranges.join(" ")}</td>
      {/* The number an operator would otherwise work out from a /22 in their head, and
          the one that decides whether a job is a few minutes or most of an hour. */}
      <td>{addressCount(job.addresses)}</td>
      <td>{schedule ?? <span className="dim">manual only</span>}</td>
      <td>{job.last_run_at ? ago(job.last_run_at) : <span className="dim">never</span>}</td>
      <td>
        {writable && (
          <button
            type="button"
            onClick={onDelete}
            title="Delete this job. Its runs are kept: the record of what was scanned outlives the instruction."
          >
            Delete
          </button>
        )}
      </td>
    </tr>
  );
}

/**
 * The new-job form.
 *
 * Ranges are typed as text, one per line or space-separated, because that is how they
 * arrive — pasted out of an IPAM export or a spreadsheet. They are sent as typed and the
 * server refuses what it cannot take, with the sentence that says what to do instead: a
 * client-side CIDR validator here would be a second opinion that could disagree with the
 * one that matters.
 */
function NewJob({
  onSaved,
  onProblem,
}: {
  onSaved: () => Promise<void>;
  onProblem: (problem: string | null) => void;
}) {
  const { tenant } = useShell();
  const [name, setName] = useState("");
  const [ranges, setRanges] = useState("");
  const [credential, setCredential] = useState("");
  const [schedule, setSchedule] = useState("");

  const save = useMutation({
    mutationFn: () =>
      createJob(tenant.tenant_id, {
        name: name.trim(),
        ranges: ranges.split(/[\s,]+/).filter(Boolean),
        credential_refs: credential.trim() ? [credential.trim()] : [],
        schedule_seconds: schedule ? Number(schedule) : null,
      }),
    onSuccess: async () => {
      setName("");
      setRanges("");
      setCredential("");
      setSchedule("");
      await onSaved();
    },
    onError: (error) => onProblem(message(error)),
  });

  return (
    <form
      className="panel"
      onSubmit={(event) => {
        event.preventDefault();
        onProblem(null);
        save.mutate();
      }}
    >
      <label>
        Name
        <input
          value={name}
          onChange={(event) => setName(event.target.value)}
          placeholder="Branch offices"
          required
        />
      </label>

      <label>
        Ranges
        <textarea
          value={ranges}
          onChange={(event) => setRanges(event.target.value)}
          placeholder={"192.168.1.0/24\n10.4.0.0/22"}
          rows={3}
          required
        />
      </label>
      <p className="dim">
        One CIDR per line. Nothing wider than a /16, and 65,536 addresses across the whole
        job — a sweep must not be the reason somebody's network monitoring alerts.
      </p>

      <label>
        Credential
        <input
          value={credential}
          onChange={(event) => setCredential(event.target.value)}
          placeholder="credential id"
          required
        />
      </label>
      {/* The sentence that explains the whole of §2.2, on the screen where somebody
          might otherwise wonder why there is no "try common community strings" box. */}
      <p className="dim">
        Discovery only ever tries credentials a job names. It does not guess: an address
        that answers nothing is recorded as unreachable, not retried with a wordlist.
      </p>

      <label>
        Schedule
        <select value={schedule} onChange={(event) => setSchedule(event.target.value)}>
          <option value="">Manual only</option>
          <option value="3600">Hourly</option>
          <option value="86400">Daily</option>
          <option value="604800">Weekly</option>
        </select>
      </label>

      <p>
        <button type="submit" disabled={save.isPending}>
          {save.isPending ? "Saving…" : "Create job"}
        </button>
      </p>
    </form>
  );
}

// ----------------------------------------------------------------------------
// Runs
// ----------------------------------------------------------------------------

export function DiscoveryRunsPage() {
  const { tenant } = useShell();

  const runs = useQuery({
    queryKey: ["discovery-runs", tenant.tenant_id],
    queryFn: () => listRuns(tenant.tenant_id),
    retry: false,
  });

  const rows = runs.data ?? [];

  return (
    <>
      <h1>Discovery runs</h1>
      <Tabs />

      {runs.isError && (
        <div className="problem" role="alert">
          {message(runs.error)}
        </div>
      )}

      <p className="dim">
        {rows.length === 0
          ? "Nothing has run yet."
          : `${rows.length} run${rows.length === 1 ? "" : "s"}, newest first.`}
      </p>

      {rows.length > 0 && (
        <div className="scroll-x">
          <table>
            <thead>
              <tr>
                <th>Started</th>
                <th>Ranges</th>
                <th>Probed</th>
                <th>Answered</th>
                <th>New</th>
                <th>Known</th>
                <th>Review</th>
                <th>Candidates</th>
                <th>Links</th>
              </tr>
            </thead>
            <tbody>
              {rows.map((run) => (
                <RunRow key={run.id} run={run} />
              ))}
            </tbody>
          </table>
        </div>
      )}
    </>
  );
}

function RunRow({ run }: { run: DiscoveryRun }) {
  const tone = runTone(run);
  // The one number that diagnoses a wrong credential list: a network where everything
  // was probed and nothing at all replied is almost never an empty network.
  const suspicious = run.status === "succeeded" && run.probed > 0 && run.answered === 0;

  return (
    <tr>
      <td>
        <span className={`dot ${tone}`} aria-hidden="true" /> {ago(run.started_at)}
        {run.status === "running" && <span className="dim"> · running</span>}
        {run.error && <div className="dim">{run.error}</div>}
      </td>
      <td className="mono">{run.ranges.join(" ")}</td>
      <td>{run.probed}</td>
      <td>
        {run.answered}
        {suspicious && (
          <div className="dim">
            nothing answered — check the job&rsquo;s credentials
          </div>
        )}
      </td>
      <td>{run.created}</td>
      <td>{run.merged}</td>
      <td>{run.for_review}</td>
      <td>
        {run.candidates > 0 ? (
          <Link to="/discovery/candidates">{run.candidates}</Link>
        ) : (
          run.candidates
        )}
      </td>
      <td>{run.edges}</td>
    </tr>
  );
}

// ----------------------------------------------------------------------------
// Candidates
// ----------------------------------------------------------------------------

export function DiscoveryCandidatesPage() {
  const { tenant } = useShell();
  const client = useQueryClient();
  const [problem, setProblem] = useState<string | null>(null);

  const candidates = useQuery({
    queryKey: ["discovery-candidates", tenant.tenant_id],
    queryFn: () => listCandidates(tenant.tenant_id),
    retry: false,
  });

  const dismiss = useMutation({
    mutationFn: ({ id, reason }: { id: string; reason: string }) =>
      ignoreCandidate(tenant.tenant_id, id, reason),
    onSuccess: async () => {
      setProblem(null);
      await client.invalidateQueries({
        queryKey: ["discovery-candidates", tenant.tenant_id],
      });
    },
    onError: (error) => setProblem(message(error)),
  });

  const rows = candidates.data ?? [];

  return (
    <>
      <h1>Candidates</h1>
      <Tabs />

      {candidates.isError && (
        <div className="problem" role="alert">
          {message(candidates.error)}
        </div>
      )}
      {problem && (
        <div className="problem" role="alert">
          {problem}
        </div>
      )}

      <p className="dim">
        {rows.length === 0
          ? "Nothing outstanding. Everything discovery found became a device or was dismissed."
          : `${rows.length} thing${rows.length === 1 ? "" : "s"} discovery found and could not turn into a device.`}
      </p>

      {rows.length > 0 && (
        <div className="scroll-x">
          <table>
            <thead>
              <tr>
                <th>Address</th>
                <th>What it says it is</th>
                <th>How we know</th>
                <th>Why it is here</th>
                <th>Last seen</th>
                <th />
              </tr>
            </thead>
            <tbody>
              {rows.map((candidate) => (
                <CandidateRow
                  key={candidate.id}
                  candidate={candidate}
                  mayWrite={mayWrite(tenant.role)}
                  busy={dismiss.isPending}
                  onIgnore={(reason) => dismiss.mutate({ id: candidate.id, reason })}
                />
              ))}
            </tbody>
          </table>
        </div>
      )}
    </>
  );
}

/**
 * What a candidate says it is, in whatever it actually told us.
 *
 * The fields arrive in descending order of usefulness and any of them can be absent, so
 * the first one present becomes the heading rather than every row starting with an em
 * dash. A printer that reported only a `sysDescr` should read "HP LaserJet MFP M428",
 * not "—" with the description underneath it.
 */
function Describes({ candidate }: { candidate: DiscoveryCandidate }) {
  const heading = candidate.sys_name ?? candidate.platform ?? candidate.sys_descr;
  // Only when it is not already the heading, so nothing is said twice.
  const detail = candidate.sys_descr === heading ? null : candidate.sys_descr;

  return (
    <>
      {heading ?? <span className="dim">—</span>}
      {detail && <div className="dim">{detail}</div>}
      {/* A chassis ID is the strongest thing a neighbour reports and is worth showing: it
          is what an operator matches against the label on the front of a box. */}
      {candidate.chassis_id && <div className="dim mono">{candidate.chassis_id}</div>}
    </>
  );
}

function CandidateRow({
  candidate,
  mayWrite: writable,
  busy,
  onIgnore,
}: {
  candidate: DiscoveryCandidate;
  mayWrite: boolean;
  busy: boolean;
  onIgnore: (reason: string) => void;
}) {
  const [reason, setReason] = useState("");
  const [asking, setAsking] = useState(false);

  return (
    <tr className={candidate.state === "unreachable" ? "warn" : undefined}>
      <td className="mono">
        {candidate.address ?? <span className="dim">no address</span>}
        {candidate.mac && <div className="dim mono">{candidate.mac}</div>}
      </td>
      <td>
        <Describes candidate={candidate} />
      </td>
      <td>
        {sourceWeight(candidate.source)}
        {candidate.seen_from && (
          <div className="dim">
            reported by{" "}
            <Link to="/resources/$id" params={{ id: candidate.seen_from }}>
              this device
            </Link>
            {candidate.port_id && <span className="mono"> {candidate.port_id}</span>}
          </div>
        )}
      </td>
      {/* Shown as the server wrote it. Every reason is a sentence that says what to do,
          and rewording it here would put a second voice on the screen — which is also why
          the cell wraps instead of ellipsing: half an instruction is not one. */}
      <td className="reason">{candidate.reason}</td>
      <td>{ago(candidate.last_seen)}</td>
      <td>
        {writable &&
          (asking ? (
            <form
              onSubmit={(event) => {
                event.preventDefault();
                onIgnore(reason);
              }}
            >
              <input
                value={reason}
                onChange={(event) => setReason(event.target.value)}
                placeholder="it is a printer"
                aria-label="Why this is being dismissed"
                required
              />
              <button type="submit" disabled={busy}>
                Dismiss
              </button>
            </form>
          ) : (
            <button
              type="button"
              onClick={() => setAsking(true)}
              title="Stop showing this. It stays in the database, so the next run does not bring it back."
            >
              Dismiss
            </button>
          ))}
      </td>
    </tr>
  );
}
