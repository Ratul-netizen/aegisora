# Status — pick up from here

Last updated: 2026-09-14 · repo: `github.com/Ratul-netizen/aegisora`

> Read this first on a new machine. [PLAN.md](./PLAN.md) is strategy,
> [SPEC.md](./SPEC.md) is the M0–M4 implementation spec, this is *where we are*.

---

## One-paragraph summary

Building a unified infrastructure observability platform — network monitoring, logs,
metrics, traces, flows and topology sharing **one resource identity and one correlation
model**, rather than an NMS bolted to a log stack. Rust/Axum + React, PostgreSQL for the
control plane and ClickHouse for telemetry, OpenTelemetry Collector instead of a bespoke
agent. Both on-premise and hosted are first-class; buyers are unrestricted, including
government and defence, which is why on-prem is not a downgrade. The W1 storage
benchmark is **complete and validated the architecture**. M0 is under way: the workspace,
**M0 is complete** — every component built, and the W1 go/no-go written up at
[`docs/benchmarks/w1.md`](./docs/benchmarks/w1.md). M1 has started: `uops-store-pg`
puts the resource repository and the query layer's `ResourceCatalog` over real
PostgreSQL.

---

## Where we are

| Phase | State |
|---|---|
| Strategy & architecture | ✅ Frozen (PLAN.md) |
| M0–M4 specification | ✅ Written (SPEC.md) |
| **W1 storage benchmark** | ✅ **Complete — architecture validated** |
| **M0 · workspace + CI** | ✅ Done |
| **M0 · `uops-core`** | ✅ Done — 34 tests, 3 doctests, 5 compile_fail |
| **M0 · `uops-secrets`** | ✅ Done — 33 tests |
| **M0 · `uops-query`** | ✅ Done — 48 tests, 12 golden fixtures |
| **M0 · PostgreSQL migrations** | ✅ Done — 5 migrations, 22 asserted invariants |
| **M0 · ClickHouse migration runner** | ✅ Done — 34 tests, applied against 26.8 |
| **M0 · `uops-bus`** | ✅ Done — 18 tests + an 11-case conformance suite |
| **M0 acceptance criteria** | ✅ **All met** |
| **M1 · `uops-store-pg`** | 🟡 resources + catalog done — 19 tests, 8 against a real server |
| **M1 · `uops-identity`** | ✅ Done — 24 tests on the rules, 11 more against PostgreSQL |
| **M1 · auth foundations** | ✅ Done — passwords, session tokens, roles, 18 tests |
| **M1 · `uops-api` scope extractor** | ✅ Done — 24 tests, mutation-guarded in CI |
| **M1 · auth routes** | ✅ Done — login/logout/me, CSRF, 22 tests |
| **M1 · resource routes + audit trail** | ✅ Done — 14 tests |
| **M1 · `uops-store-ch`** | ✅ Done — 24 tests, 11 against real ClickHouse |
| **M1 · `POST /query`** | ✅ Done — 8 tests, the full stack over HTTP |
| **M1 · first-run bootstrap** | ✅ Done — 5 store tests, 3 boot tests, 2 mutation guards |
| **M1 · `uops-server`** | ✅ Done — it runs, and you can log into it |
| **M1 · web shell** | ✅ Done — shell, auth, switcher, inventory, detail, explorer |
| **M1 · cross-tenant acceptance test** | ✅ Done — 5 tests over every route, 2 mutation guards |
| **M1 · 10 000-resource p95** | ✅ Done — measured through the router, worst p95 75 ms |
| **M1 · `docker compose up`** | ✅ Done — one 30 MB image, migrations as their own step, CI-verified |
| **M1 · identity end to end** | ✅ Done — the SPEC sentence as one sequence; found a real bug |
| **M2 · monitoring profiles** | ✅ Done — 5 built-ins, 40 tests, schema + resolution |
| **M2 · scheduler + counters** | ✅ Done — time wheel, jitter, wrap detection; 19 tests |
| **M2 · SNMP walk + simulator** | ✅ Done — 19 tests, a 1 000-agent fleet in a Vec |
| **M2 · v3 credential handling** | 🟡 protocols, policy, access context — 9 tests |
| **M2 · net-snmp test agent** | ✅ Done — SHA-256/AES-256 authPriv, verified end to end |
| **M2 · the executor** | ✅ Done — limits, budgets, and the measured criterion |
| **M2 · the planner** | ✅ Done — jobs grouped by interval, 9 tests |
| **M2 · the real transport** | ✅ Done — snmp2 over UDP, 6 tests against net-snmp |
| M2 · the executor | ⬜ |
| M2–M4 | ⬜ |

## Resume in three commands

```bash
git clone https://github.com/Ratul-netizen/aegisora && cd aegisora
cargo test --workspace --all-targets && cargo test --workspace --doc   # 498 tests, green
cd web && npm ci && npm test                                            # 13 more
cargo clippy --workspace --all-targets -- -D warnings
```

To bring the benchmark stack back (only needed to re-run W1):

```bash
docker compose -f bench/docker-compose.yml up -d     # ClickHouse 26.8, data persists in a volume
bash bench/scripts/load.sh logs 10000000 --truncate  # ~3 min
bash bench/scripts/run.sh 5                          # query suite → bench/results/
```

The benchmark data is **regenerable, never committed**. Seed 42 reproduces it exactly.

---

## The five decisions that everything else follows from

1. **Resource identity + correlation is the product.** Storage, polling and ingestion are
   commodities. When `router-01` appears in SNMP, syslog, NetFlow, LLDP, a config backup
   and an alert, all six must resolve to one `resource_id`. Everything downstream depends
   on it.
2. **Two storage engines, not four.** PostgreSQL (control) + ClickHouse (telemetry).
   ClickHouse full-text search went GA March 2026, so logs, metrics, traces and flows
   live in one engine. Rejected: OpenSearch (~10x nodes at log volume), Quickwit
   (Datadog-owned since Jan 2025), TimescaleDB for metrics (557K→159K rows/s at 10M hosts).
3. **OpenTelemetry Collector, not a bespoke agent.** It already does host metrics, files,
   journald, Windows Event Log and syslog.
4. **Both deployment models, customer's choice.** Two named profiles, `onprem` and
   `hosted`, each selecting a coherent set of implementations — not a pile of config flags.
5. **AGPL-3.0 + CLA.** AGPL blocks a hosted clone; the CLA keeps commercial
   dual-licensing available for on-prem buyers whose legal teams block AGPL outright.

---

## W1 results — the numbers that justify the architecture

ClickHouse 26.8.2.7, single node, warm cache. The written verdict is
[`docs/benchmarks/w1.md`](./docs/benchmarks/w1.md); the raw evidence behind it is
[`bench/results/FINDINGS.md`](./bench/results/FINDINGS.md).

### The core bet, confirmed

**Q05 — "all signals for one resource in a window" — ran at 9 ms reading 16,380 rows at
BOTH 10M and 100M.** Ten times the data, identical latency, identical work. Resource-scoped
investigation is independent of table size. **The sort key
`(tenant_id, resource_id, observed_at)` must not change.**

Selective token search behaves the same way: 8,190 rows read at both scales to find one
hit in a hundred million.

### What the benchmark changed in the spec

| Finding | Amendment |
|---|---|
| Tail query read the **entire tenant** (2,303 ms) | `p_by_time` projection → **72 ms, 254 K rows. 32x.** Cost ~1.9x storage |
| Explorer histogram at 1,066 ms, and the projection **does not fix it** (rows read barely moved) | Pre-aggregated `logs_counts_5m`. Re-renders on every search, so it matters more than the tail |
| Slowest query was `GROUP BY attributes['host.name']` at **2,252 ms** — worse than phrase search | Materialize grouped semconv attributes as real columns |
| Text index = **71% of compressed data**, and the overhead *grew* with scale | Per-source opt-out is **required**, not optional |
| `LIKE` 1,928 ms, phrase 2,378 ms, both full scans | No index-accelerated substring or phrase search. `QueryWarning` in the AST |

Also learned: time-ordering compresses **58% worse** than resource-ordering, because
sorting by resource groups rows sharing vendor/source/host.

### Harness artifacts — not ClickHouse numbers

Recorded so nobody mistakes them for storage limits later:
`docker exec -i` stdin load was **18x slower** than HTTP (182 s vs 9.9 s per 1M rows), and
`curl --data-binary @-` buffers the whole stream — it hit 6.7 GB RSS climbing toward
~32 GB before being killed. Loads now run in bounded batches.

---

## What exists in code

```
crates/uops-core/
├── ids.rs        UUIDv7 newtypes — (TenantId, ResourceId) cannot be transposed
├── scope.rs      TenantScope — a missing tenant filter is a COMPILE ERROR
├── secret.rs     Secret<T> — not Display/Serialize/Clone, zeroizes on drop
├── identity.rs   noisy-OR confidence + tier-1 contradiction (the moat)
├── resource.rs   resource model + relationship edges
├── envelope.rs   the one telemetry shape all signals arrive in
├── attr.rs       OTel semconv attributes, BTreeMap for deterministic bytes
└── error.rs      TenantMismatch → 404, indistinguishable from NotFound

crates/uops-secrets/
├── aead.rs       AeadProvider trait — crypto backend chosen at BUILD time
├── kek.rs        KekRing — the KEK never enters the database
├── record.rs     SealedCredential + AAD binding tenant‖credential‖version
├── vault.rs      LocalVault — envelope encryption, rotation, revocation
├── audit.rs      AccessLog — records grants AND denials
├── serialize.rs  length-prefixed framing for credential material
└── memory.rs     in-memory SealedStore, for tests and the dev profile

crates/uops-query/
├── ast.rs        the ONE Query AST — UI, API, alerts and M6 text search share it
├── resolve.rs    ResourceSelector → ResolvedResources, through resource_alias
├── plan.rs       which table answers this — where the W1 fixes take effect
├── compile.rs    codegen; tenant_id is written HERE, never by the caller
├── sql.rs        parameterised text — the crate has no escaping function
├── warning.rs    QueryWarning: correct-but-slow is reported, not hidden
└── tests/golden/ 12 Query JSON → expected SQL fixtures

migrations/                       PostgreSQL control plane — SPEC M0.1/M0.2/M0.4
├── 0001_foundation.sql   organization → tenant → site, updated_at trigger
├── 0002_resource.sql     the resource model; composite FKs carry tenant_id
├── 0003_relationships.sql edges + resource_dependents(tenant, root, depth)
├── 0004_identity.sql     identifiers, decisions, aliases that collapse on write
├── 0005_credentials.sql  sealed credentials + access log that outlives them
└── tests/invariants.sql  the properties the schema exists for, asserted in SQL

ch-migrations/                    ClickHouse telemetry schema — SPEC M0.6 + W1
├── 0001_logs.sql          logs, text index, materialised semconv columns
├── 0002_..._projection    p_by_time — W1 FIX 1, the tail (2 303 ms → 72 ms)
├── 0003_logs_counts_5m    the Explorer histogram — W1 FIX 2
├── 0004_metrics.sql       metrics + the 5m rollup
├── 0005_metrics_1h.sql    the hourly rollup uops-query already plans onto
├── 0006_events_states     events, state transitions
└── deferred/              traces and flows: declared, created in M7/M8

crates/uops-ch-migrate/           versioned · idempotent · resumable
├── statement.rs  splits files properly — ClickHouse takes ONE statement per request
├── migration.rs  load + checksum; deferred/ is not part of the applied set
├── plan.rs       pure: resume point, and four refusals decided before anything is sent
├── ledger.rs     ReplacingMergeTree + FINAL — ClickHouse has no unique constraint
├── runner.rs     applies, recording EVERY statement as it lands
└── http.rs       the whole protocol: one POST per statement

crates/uops-bus/                  keep the boundary, defer the daemon
├── subject.rs    telemetry.{tenant}.{signal}.{source} — NATS matching rules exactly
├── bus.rs        TelemetryBus + Delivery + AckHandle (a no-op that must exist)
├── inprocess.rs  bounded tokio channel per subscriber; a full channel BLOCKS
└── conformance.rs the contract, executable — shipped so NatsBus runs these same cases

crates/uops-store-pg/             M1 — the control plane over PostgreSQL
├── store.rs      pool, statement timeout, a Debug that cannot print the password
├── resource.rs   the repository; every method takes &TenantScope
├── catalog.rs    ResourceCatalog over Postgres — alias collapse, topology walk
├── page.rs       keyset pagination on UUIDv7 ids. Never OFFSET
└── enforced.rs   reads this crate's OWN source: no query without a tenant predicate

crates/uops-identity/             M1 — one resource_id per device, whatever it is called
├── resolver.rs   the service around uops-core's rules: ordering, writes, merge/split
├── cache.rs      (tenant, kind, value) → resource_id. Answers only unambiguous cases
├── store.rs      the narrow persistence interface
└── memory.rs     in-memory store that enforces UNIQUE the way the schema does

migrations/0006_auth.sql           users, roles, sessions, audit_log, access_log

crates/uops-secrets/src/password.rs  argon2id, m=19456 t=2 p=1 — SPEC M0.8
crates/uops-secrets/src/session.rs   opaque tokens; only the HASH is stored
crates/uops-store-pg/src/auth.rs     users, roles, sessions; one statement per request

crates/uops-store-ch/               M1 — the other half of the query layer
├── client.rs     async HTTP on hyper, which axum already pulls
├── store.rs      the M0.6 traits; nothing here writes SQL
└── rows.rs       columns mirror ch-migrations exactly

crates/uops-api/
├── extract.rs    THE file: the only caller of TenantScope::from_authenticated
├── error.rs      RFC 7807. A tenant you cannot see is 404, never 403
├── cookie.rs     session cookie HttpOnly, CSRF cookie deliberately not
├── csrf.rs       double-submit; the header an attacker's page cannot produce
├── audit.rs      the trail hangs off the SCOPE extractor, not a list of routes
├── routes/auth.rs  login must cost the same whether or not the address exists
├── routes/resources.rs  thin: roles in, repository out, audit recorded
└── state.rs      what every handler is given

crates/uops-store-pg/src/identity.rs
   IdentityStore over PostgreSQL. Merge and split are one transaction each:
   half a merge orphans every row of telemetry under the old resource_id.
```

CI enforces fmt, clippy `-D warnings`, tests, doctests, plus: a grep that fails the build
if `.expose()` appears inside a logging macro; a grep that fails if a crypto primitive is
used outside `uops-secrets`; `cargo-deny`; a CycloneDX SBOM; and a matrix building **both**
the standard and FIPS crypto artifacts.

### Trap worth remembering

The `compile_fail` doctests asserting `Secret<T>` is not serialisable were initially
**not running at all** — rustdoc does not collect doctests from private items inside
`#[cfg(test)]` modules, so they passed vacuously. They now live on the public items.
A security invariant asserted in a test that never executes is worse than no test,
because it reads as covered.

---

## Next, in dependency order

1. **The review queue has no way to say "no"** — found while writing the scenario
   above. A case leaves the queue when the provisional is merged away, and that is the
   only way out. An operator who decides two resources are genuinely *different* has no
   action to take: the question comes back tomorrow and every day after. This needs a
   dismissal — a decision outcome, a store method and a route — and it is a product
   decision about what "not the same" means for a provisional resource that already has
   telemetry attached, so it is not something to invent silently.
2. **No UI for the review queue.** `pending_reviews` exists and is tested; nothing
   surfaces it. Blocked on the above, since a queue you can only agree with is worse
   than no queue.
3. **M2 · the `snmp2`-backed transport.** Everything it needs now exists: the
   `Transport` trait, a real agent to talk to, and a credential type that carries its
   protocols. What it still needs is a socket-reuse decision — one UDP socket per
   poller or one per device — which is a per-device concurrency question and so belongs
   with the executor rather than ahead of it.
4. **M2 · the real transport.** `Transport` has one implementation and it is the
   simulator. The `snmp2`-backed one is next to it and small; what it needs first is a
   decision about socket reuse — one UDP socket per poller or one per device — which is
   a per-device concurrency question and so belongs with the executor.
4. **M2 · the poller binary.** Every part exists and the transport now reaches real
   hardware. What is missing is the process: read pollable resources from `PostgreSQL`,
   resolve each to a profile, plan the jobs, load them into the wheel, run the due ones
   through the executor, and write the samples to `ClickHouse`. Interface discovery —
   creating child resources and `member_of` edges — hangs off the same loop and is the
   last M2 acceptance criterion with nothing behind it.

## How to pick this up

```bash
docker compose -f deploy/docker-compose.yml up -d          # postgres + clickhouse
bash scripts/db.sh migrate && bash scripts/db.sh test      # 22 schema invariants
bash scripts/ch.sh apply && bash scripts/ch.sh verify      # 6 migrations, 12 golden
DATABASE_URL=postgres://uops:uops@localhost:5432/uops   CLICKHOUSE_USER=uops CLICKHOUSE_PASSWORD=uops   bash scripts/serve.sh                                      # http://127.0.0.1:8080
DATABASE_URL=postgres://uops:uops@localhost:5432/uops   CLICKHOUSE_USER=uops CLICKHOUSE_PASSWORD=uops   cargo test --workspace --all-targets                     # 498, all green
```

The integration tests need both containers. The unit tests do not, and the workspace
builds with no database at all — `.sqlx/` holds the recorded query metadata.

**The resource model is now settled** — it is in PostgreSQL, in `uops-core`, and in the
ClickHouse sort key. The frontend and the collectors were held back until it was, and
M1 is where they start.

---

## Open items

| Item | Blocks | Note |
|---|---|---|
| **Shared-database contamination** | intermittent local failures | The scale test seeded 10 000 resources and did not remove them; four runs left 40 400 rows in the database every other suite shares, which changes what the planner chooses for all of them. It cleans up after itself now, and `db.sh sweep` removes what an interrupted run leaves. This is the likely cause of the "one unreproduced failure" recorded earlier — both occurrences followed scale-test runs. Not proven, because it has not recurred since the purge |
| **Row-level security** | M1 API | Tenant isolation currently rests on `TenantScope`, composite foreign keys and sqlx. RLS would be a fourth layer and is worth having, but it needs an app role and a per-transaction `SET LOCAL` — a decision about connection pooling and the request lifecycle, so it belongs with the API |
| **CLA reviewed by a lawyer** | accepting outside contributions | Draft is in `CLA.md`, modelled on Apache ICLA. **The only irreversible item** — an unsigned contribution permanently forecloses dual-licensing |
| Product name | crate publishing only | `uops` codename unblocks everything else. Repo is still named `aegisora`, which was rejected (`aegisora-ai` is an active org in an adjacent market) |
| Buyer focus: MSP-first? | credential scoping depth in M1 | My recommendation was MSP-first; your read on Bangladesh/SEA overrides mine |
| Metrics + rollup ingest cost | M4, not M0 | The one W1 measurement not run |
| **Tiered storage policy** | deployment profiles | SPEC §M0.6 shows `TTL … TO VOLUME 'warm'/'cold'` against a `tiered` policy that does not exist on a default install — those migrations would fail outright. Retention is a plain `DELETE` TTL for now; tiering is a later migration, written alongside the profile that configures the policy |

## Decided since the last update

**CI was red and nothing said so.** The `check` job ran `cargo test --workspace
--all-targets` with no databases, so every integration test added since `uops-store-pg`
failed there — five commits' worth. The workspace suite now runs once, in the job that
has both engines; `check` keeps fmt, clippy (which still *compiles* every target) and
the doctests. The API tests had also drifted into the ClickHouse-only job, where the
PostgreSQL half of them could not have worked. One integration job now owns both
engines, because `POST /query` spans them and a test that crosses that seam otherwise
has no home.

**A pre-aggregated query floors its window to the bucket.** Found by running a real
histogram against a real `logs_counts_5m`: it returned nothing, because every bucket in
the fixture began a few minutes before the window did. A bucket is the unit of storage,
so a window starting partway through one either includes it or loses it — and losing it
drops the leftmost bar of every histogram. An Explorer opened at 14:37 would silently
omit 14:35. Base-table queries are untouched.

**The audit hook hangs off the scope extractor, not a list of routes.** SPEC says
"middleware over the query and resource routes", and a layer wrapped around a chosen list
has one failure mode: someone adds a twenty-first route and forgets it. Attaching the
hook to `Caller` — the only way to obtain a `TenantScope`, and therefore the only way to
reach tenant data — means authorisation and auditing share a chokepoint. A handler cannot
read a customer's data without having already been attributed.

**`POST /query` is deliberately not built yet.** Compiling a `Query` to ClickHouse SQL
works and is golden-tested, but nothing executes it: that needs the ClickHouse client,
which is its own piece of work. An endpoint that compiled a query and returned nothing
would be worse than no endpoint.

**The tenant is a request header, not a path segment.** `/api/v1/resources` with
`X-Uops-Tenant`, rather than `/api/v1/tenants/{id}/resources`. An MSP engineer's session
spans several customers, and the alternative — a "currently selected" tenant on the
session — makes a request's meaning depend on invisible state, makes an audit row
ambiguous about which customer was read, and gives a stolen cookie a selection to carry.
A custom header is also a CSRF defence in its own right, since a cross-origin form cannot
set one; that is a second layer under the double-submit token, not a replacement.

**`create_resource` was two different operations with one name.** The repository's is an
operator deliberately adding a device with a name and a site; the resolver's is "something
is sending telemetry and I cannot yet say what it is". On `PgStore` they collided, and the
inherent method silently won. The resolver's is now `create_provisional`, which is what it
always meant.

**A repeated review reuses its provisional resource.** SPEC §M0.2 gives the outcome bands
but does not say what happens on the *second* identical observation — and a device sends
thousands of messages an hour. The first implementation minted a provisional resource and
a queue item per message. Reviews are now deduplicated by observed identifier set, so one
unanswered question is one queue item, and telemetry keeps landing somewhere stable.

**Two sources discovering the same device usually produce one review item.** Worth
knowing before it surprises someone in a demo. A hostname match alone is 0.65, and
hostname + mgmt_ip is 0.93 — both below the 0.95 auto-merge bar, which SPEC chose
deliberately. Automatic joining needs a shared tier-1 identifier (serial, chassis ID,
SNMP engine ID, OTel host ID) or enough weaker ones to clear 0.95. That is the
conservative side to err on: a wrong merge silently corrupts every correlation
downstream, a queue item costs ten seconds.

**`ResourceCatalog` is async now.** It was synchronous in M0 because compilation is pure
and nothing implemented it yet. Every real implementation is a database — the PostgreSQL
one is four `sqlx` queries — and a synchronous trait would have forced it to block a
runtime thread on I/O. `compile()` is untouched and still pure, which is what keeps the
golden tests free of a database.

**PostgreSQL queries are compile-time checked, with the metadata committed.**
`cargo sqlx prepare` writes `.sqlx/`, builds use `SQLX_OFFLINE=true`, and CI fails if the
recorded metadata has drifted from the queries in the tree. So a clone with no database
still builds, and a renamed column is a compile error rather than a 500 in production.

**The bus contract is JetStream's, not a channel's.** `InProcessBus` is a `tokio` channel
per subscriber, but subjects follow NATS matching rules exactly (`*`, `>`), `ack` exists
from the first commit doing nothing, and a new subscriber gets no history. Each of those
would otherwise change *which messages a consumer receives* when the transport changes,
which is not a wiring change. The conformance suite ships in the library as public
functions so `NatsBus` runs the same eleven cases rather than a copy that has drifted.

**Backpressure blocks, and there is a test that fails if it stops.** A full channel makes
`publish` wait rather than dropping. CI mutates `send().await` to `try_send()` and
requires the suite to fail — the same trick as the Postgres and ClickHouse guards.

**`searchAll()` and `searchAny()` do not exist.** `uops-query` was emitting them for
token search — the names the text-index beta announcements used. ClickHouse 26.8 answers
`Function with name 'searchAll' does not exist (UNKNOWN_FUNCTION)`. The real functions
are **`hasAllTokens()`** and **`hasAnyTokens()`**, now emitted and verified against a
running server. Every unit test on both sides passed the whole time this was wrong;
`scripts/ch.sh verify` — which runs uops-query's golden SQL against the live schema — is
what caught it, and is now a CI job.

**Alias chain depth (was open in SPEC §M0.2): collapse on write.** A trigger in
`0004_identity.sql` rewrites A→B to A→C when B→C is created. Merges are rare and reads
are constant, so transitive resolution would put a recursive lookup on the hot path of
every telemetry query. The collapse buys one flat invariant — no `historical_id` is ever
also a `current_id` — which is what lets alias expansion be a single lookup. It lives in
the database because that is only true if *every* writer collapses, including a DBA
fixing something by hand.

## Housekeeping

ClickHouse is pinned to **26.8** in `deploy/docker-compose.yml` and in CI, matching the
version W1 was measured on. `bash scripts/ch.sh apply|verify|smoke|reset`.

The PostgreSQL dev database lives in a Docker volume:
`docker compose -f deploy/docker-compose.yml up -d`, then `bash scripts/db.sh migrate`
and `bash scripts/db.sh test`. `bash scripts/db.sh reset` re-applies from empty.

The benchmark ClickHouse volume has been removed — the data is gone, and that is fine:
it is regenerable from seed 42 and nothing depends on it. `bench/scripts/load.sh`
rebuilds it if W1 ever needs re-running. Earlier versions of this file said the volume
still held ~12 GiB; it does not.
