# M5 Discovery — where it stands

Last updated: 2026-09-18. Written to be picked up cold.

The specification is [`M5-discovery.md`](./M5-discovery.md); its §4 acceptance criteria
all pass. This file is the *state*, which is a different question: what is built, what
remains, and what a person walking back in needs to know before touching it.

---

## Done

| | Where |
|---|---|
| Specification, decisions closed before building | `docs/M5-discovery.md` |
| Schema — jobs, runs, candidates | `migrations/0017`, `0019` |
| Sweep planning, probe, run driver, credential loop | `crates/uops-discover/src/{sweep,probe,run}.rs` |
| Neighbour decode — LLDP, CDP, ARP | `crates/uops-discover/src/neighbour.rs` |
| Persistence — jobs, runs, candidates | `crates/uops-store-pg/src/discovery_jobs.rs` |
| Sweep → inventory, via identity resolution | `crates/uops-store-pg/src/sweep_ingest.rs` |
| Neighbours → edges and candidates | `crates/uops-store-pg/src/neighbour_ingest.rs` |
| Seven routes, tenant-isolated | `crates/uops-api/src/routes/discovery.rs` |
| Three screens — Jobs, Runs, Candidates | `web/src/discovery.ts`, `web/src/discoverypages.tsx` |

All eight acceptance criteria pass. 47 tests in `uops-discover`, 38 across the two store
test files, 7 isolation cases, and the whole workspace at 104 test binaries.

---

## Not done

**The scheduler and the runner.** This is the last piece, and it is one piece rather than
two:

- `discovery_job.schedule` and `discovery_job_due_idx` exist; nothing reads them.
- Nothing inside the server can *run* a sweep, which is why there is no
  `POST /api/v1/discovery/jobs/{id}/run` and why the jobs screen says jobs do not run on
  a schedule yet rather than offering a button that would appear broken.

What it needs to do, and the pieces are all already there:

1. A wheel-driven loop, the way `uops-alert`'s scheduler does it — `uops_poll::Wheel`,
   `RELOAD`, `TICK`. That crate is the worked example to copy from.
2. For a due job: read its credentials from the vault, build one `UdpTransport` per
   credential, call `PgStore::start_discovery_run`, then `uops_discover::run_with`, then
   `PgStore::record_sweep`, then `PgStore::finish_discovery_run` with the counters
   `record_sweep` returned.
3. The audit entry §2.7 requires, carrying the ranges. `routes/discovery.rs` shows the
   shape; a scheduled run has no `Caller`, so it needs the system-actor path.
4. Then the neighbour walk on each known device, into `PgStore::record_neighbours`.

**Two things to decide when starting it**, neither closed:

- **Where the loop lives.** `uops-alert` runs inside `uops-server`. Discovery could too,
  or it could be its own binary like `uops-poller`. The argument for its own binary is
  that a sweep is minutes of work and a server restart mid-sweep leaves a `running` row
  with no process behind it. The argument against is one more thing to deploy.
- **What happens to a run whose process died.** `discovery_run` has a `running` status
  and `discovery_run_finishes_iff_it_is_over` makes a stuck run findable without a
  heuristic, but nothing reaps one. A run still `running` after some multiple of its
  expected duration is `cancelled`, and that multiple is a decision.

---

## What to know before touching it

**The three caps are chosen against each other.** `IN_FLIGHT`, `PROBES_PER_SECOND` and
`PROBE_TIMEOUT` are not independent: a sweep of empty addresses runs at
`IN_FLIGHT / PROBE_TIMEOUT` probes per second *whatever the rate cap says*. Pick them
separately and the smaller binds silently while the documented one is decoration — which
is what the first version did, promising 5½ minutes for a /16 and delivering 85.
`the_three_caps_agree_with_each_other` is the test that stops it happening again.

**A sweep takes as long as its credential list is long.** A wrong SNMPv2c community is
*silence*, not a refusal, so every credential must be tried against every address that has
not answered. Four credentials over a /16 is twenty-two minutes. This is why
`MAX_CREDENTIALS` is 4 and why it is in the schema too.

**Rediscovery does not work by confidence.** An address and a hostname combine to 0.93,
under `AUTO_MERGE_THRESHOLD` — so a second sweep would send the whole estate to review if
confidence were the test. What saves it is the resolver's *exclusive match*: every
identifier pointing at one resource and nothing else is a repeat sighting. Anything that
changes what a probe observes must not break that.

**Discovery caches the `sysObjectID`; it does not pin a profile.** `resource.profile_id`
is a human's override. Writing it from discovery would make every swept device look like
one somebody had decided about.

**A neighbour is matched by lookup, never by resolution.** Resolution creates, and §2.5
forbids creating the far end of a link.

---

## Running it by hand today

There is no runner, so a sweep is driven from a test. `crates/uops-store-pg/tests/sweep_ingest.rs`
is the end-to-end example: build a `Fleet`, `Sweep::new`, `run`, `record_sweep`. Against
real equipment, substitute `UdpTransport::new(credential)` for the `Fleet`.

The demo instance has a job, a run and three candidates seeded so the screens have
content — the run and candidates were inserted as SQL, not produced by a sweep, because
nothing can run one yet.
