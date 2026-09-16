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

Counts are tests that actually run, per crate, from `cargo test --all-targets`.

| Phase | State |
|---|---|
| Strategy & architecture | ✅ Frozen (PLAN.md) |
| M0–M4 specification | ✅ Written (SPEC.md) |
| **W1 storage benchmark** | ✅ **Complete — architecture validated** |
| **M0 — all acceptance criteria met** | ✅ |
| `uops-core` | ✅ 36 tests, incl. 5 `compile_fail` |
| `uops-secrets` | ✅ 51 |
| `uops-query` | ✅ 49, 12 golden fixtures |
| `uops-bus` | ✅ 29, incl. an 11-case conformance suite |
| PostgreSQL migrations | ✅ 8 migrations, 22 asserted invariants |
| `uops-ch-migrate` | ✅ 34, applied against ClickHouse 26.8 |
| **M1 — all acceptance criteria met** | ✅ |
| `uops-store-pg` | ✅ 74 |
| `uops-identity` | ✅ 24 on the rules, more against PostgreSQL |
| `uops-api` | ✅ 83 — auth, resources, query, audit, cross-tenant |
| `uops-store-ch` | ✅ 25, against real ClickHouse |
| `uops-server` | ✅ 5 — it runs, and you can log into it |
| web shell | ✅ shell, auth, tenant switcher, inventory, detail, explorer |
| 10 000-resource p95 | ✅ measured through the router, worst 75 ms |
| `docker compose up` | ✅ one 30 MB image, migrations as their own step, CI-verified |
| **M2 — all 6 acceptance criteria met** | ✅ |
| `uops-oui` | ✅ 9 — IEEE MAC assignments, all four registries |
| device identity | ✅ make, model, serial, OS from a profile's `identity` block |
| **M6 · the map** | ✅ site coordinates, status rollup, a tile-free world map |
| `uops-profile` | ✅ 40 — 5 built-ins, schema, resolution |
| `uops-poll` | ✅ 56 — wheel, jitter, counters, executor, planner, samples |
| `uops-snmp` | ✅ 42 — walk, simulator, `snmp2` over UDP, real net-snmp |
| `uops-poller` | ✅ 32 — the binary, end to end against real everything |
| interface discovery | ✅ children + `member_of`, matched on name |
| counter wrap → no negative rate | ✅ computed in `ClickHouse` at query time |
| ICMP availability | ✅ unprivileged datagram socket, no capability needed |
| p95 through the binary | ⬜ measured in the library only |
| M3–M4 | ⬜ |

## Resume in three commands

```bash
git clone https://github.com/Ratul-netizen/aegisora && cd aegisora
cargo test --workspace --all-targets && cargo test --workspace --doc   # 527 tests, green
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

crates/uops-store-pg/src/discovery.rs
   What a discovery walk becomes: child resources and member_of edges, in one
   transaction. Does NOT go through identity resolution — an interface's parent
   is not in question, so resolving it would mean manufacturing a confidence
   score for a fact and putting interfaces in the review queue because two
   switches both have a Gi0/1. Identifiers are attached, because a flow record
   or an LLDP neighbour arrives later with a MAC and nothing else.

crates/uops-poller/
   The polling binary. main.rs is the order things happen in; the pieces are
   config (no default KEK — a poller that cannot open a credential polls
   nothing, so it refuses to start naming the variable), credentials (one
   UdpTransport per credential, not per device), fleet (rows to devices; the
   crossing between tenants is a visible `for` loop, because TenantScope has
   no "all tenants" and should not), poll (request, convert, write) and run
   (the loop, and what it says when a device fails).
   Proven end to end in tests/live.rs: a device in PostgreSQL, polled over
   real SNMP against the net-snmp container, becoming rows in ClickHouse.

crates/uops-store-pg/src/sealed.rs
   SealedStore over the credential table. Until now every credential in the
   system lived in MemorySealedStore and did not survive a restart, which the
   poller cannot work with. Sees no plaintext: the wrapping is LocalVault's and
   the KEK is KekRing's, neither reachable from this file. SealedStore is
   synchronous and sqlx is not, so exactly one file pays for the bridge
   (block_in_place), in the place that chose it.
```

CI enforces fmt, clippy `-D warnings`, tests, doctests, plus: a grep that fails the build
if `.expose()` appears inside a logging macro; a grep that fails if a crypto primitive is
used outside `uops-secrets`; `cargo-deny`; a CycloneDX SBOM; and a matrix building **both**
the standard and FIPS crypto artifacts.

### The map does not use tiles

A tile layer means a tile server. This product is deployed on-premise and often
air-gapped — the same argument `uops-oui` makes about the IEEE registry — so a map that
fetched tiles would be a blank rectangle for exactly the customers most likely to have
forty sites across a country. It would also send every viewer's map extent to a third
party, which is a data-protection conversation nobody wants to have about a status board.

So the coastline is Natural Earth's 110m land outline, converted by `scripts/worldmap.py`
into one SVG path and bundled: 54 KB, about 20 KB gzipped, no network. Natural Earth is
public domain and states that no permission or credit is required, which is a materially
different position from the IEEE registry beside it.

The cost is that this is a *locator* map — coastlines, no roads, no labels, no zoom into
a street. For "which of my forty sites is red" that is the whole requirement.

Equirectangular, because `x = lon + 180` and `y = 90 - lat` in a 360×180 viewBox means
the browser needs no projection code and the pins are positioned by the same arithmetic
as the coastline. A projection the pins and the map disagreed about would put every site
slightly in the sea, and slightly is the hardest kind of wrong to notice.

### Where a site is, and why not where a device is

Coordinates are on `site`, not on `resource`: fifty switches in one building are not
fifty places, and a coordinate per device is the same two numbers fifty times, invited to
disagree.

Not derived from an IP address either. Geo-IP answers a different question and for this
product usually answers nothing — a management address is RFC 1918 and resolves nowhere,
a public one resolves to whoever registered the block. An operator typing a coordinate
once per site is more accurate than a lookup that is wrong invisibly.

A pin's colour is worst-first and deliberately not a proportion: one device down out of
two hundred is still an outage for whoever depends on it, and a pin that faded to amber
because the other hundred and ninety-nine were fine would be hiding it. The size of the
problem is in the numbers on the card.

### The OUI table reads all four IEEE registries

A 24-bit prefix is what everybody means by "the OUI", and it is 39 815 of 53 487
assignments. The other 13 672 are MA-M (28-bit) and MA-S (36-bit) blocks, issued to
organisations that do not need sixteen million addresses — in practice, most companies
that are not household names.

The first draft of that module's documentation said a 24-bit-only lookup would return the
*wrong* vendor, the one holding the parent block. The data disagrees: IEEE reserves the
parent prefixes above the MA-M and MA-S ranges and does not list them in the MA-L
registry, so the answer is nothing rather than somebody else. Checked before it was
written down, and worth recording because the wrong version was the intuitive one.

A locally administered address — every VM, veth, bond and VLAN interface — belongs to
nobody, and returning a manufacturer for one would be inventing it. A CID is returned
with a flag saying it is not a uniqueness guarantee, because an inventory wants it and
identity resolution must not treat it as proof.

### A serial number is what identity resolution was missing

SPEC §M0.2 ranks identifiers by how much a match proves: a serial is tier 1, confidence
1.00, proof on its own; a management address is 0.80 and a hostname 0.65. Nothing in this
product produced a tier-1 identifier for an SNMP device, so resolution had been running
entirely on the weak tiers — two collectors seeing one switch resolved to one resource
only if they agreed about its address or its name, and a re-addressed device looked new.

The profile's `identity` block is what fixes that, and it is a block rather than code
because every vendor puts its model number at a different OID and a customer discovers
that before we do.

### The simulator modelled a GET as a GETNEXT

Found by a profile reading `entPhysicalSoftwareRev`, the first column of ENTITY-MIB's
chassis row. Every other fact came back and that one never did — against the simulator.
The real transport returned all five.

`Transport::get_scalars`' default implementation is a `GETNEXT` per OID, which is what a
transport with no batching can do, and it cannot return the lowest OID of a contiguous
block because nothing precedes it to ask after. `sim::Fleet` now implements a `GET` as a
`GET`. A simulator that models a request as a different request hides exactly the bugs it
exists to catch.

### Six tests that passed on Windows and failed on Linux

`KekRing::from_file` refuses a group- or world-readable key on Unix — rightly, since a
KEK other local accounts can read is not a root of trust — and `std::fs::write` leaves
`0644`. Two test fixtures wrote a KEK that way. On Windows the check does not apply and
they passed; on Linux, which is where CI runs, all six sealed-credential tests failed.

They had been failing since they were written and nothing said so, because the suite had
only ever been run on Windows. Found by running the whole workspace in a Linux container
while verifying ICMP — which is worth doing before any commit touching anything
platform-sensitive, not just that one.

### ICMP needs no capability

`SOCK_DGRAM` + `IPPROTO_ICMP`, not a raw socket. `CAP_NET_RAW` would also permit forging
arbitrary packets and putting an interface into promiscuous mode, granted to the whole
process for the life of the container — a large grant for a ping.

The unprivileged socket is gated by `net.ipv4.ping_group_range`, and Docker sets that to
`0 2147483647` on its default bridge. Verified in a container, as root and as uid 1000,
rather than assumed. A container run with `--network host` inherits the host namespace
instead, where the range is usually closed; CI runs on a VM and sets the sysctl in a
named step, which is also where the deployment requirement is written down so it cannot
drift from the code.

Where the sysctl is narrowed, the check reports which sysctl to widen rather than
reporting every device as down — the distinction `CheckError` exists to make, because a
poller that cannot open a socket looks exactly like a catastrophic outage.

### Rates are computed in `ClickHouse`, not in Rust

`uops_poll::counter` states the wrap rule and is thoroughly tested, and nothing executes
it on a query path — which is why the criterion sat unmet while looking done. SPEC says
rates are computed at query time from the raw series, and doing that in Rust would mean
shipping every raw point to the API: a 30-day dashboard panel is millions of rows to
compute a few hundred. So `Field::Rate` compiles to a window function.

Reading `counter.rs` closely gives the simplification the SQL is built on: **width and
the timing window decide only what a backwards step is called** — `Wrapped` or `Reset` —
and neither ever produces a number. So the condition a query needs is just "forwards, or
nothing", and nothing in the SQL has to know whether a counter is 32-bit.

Three things that are easy to get wrong and are each asserted:

* A series is one resource's one metric with **one set of labels**. Partition by less and
  two interfaces' counters are differenced against each other, which produces a
  plausible number rather than an error.
* The predicates go **inside** the window subquery. Outside, the window would run over
  the whole table before filtering — and would compute one customer's frames over
  another's rows.
* The **first row of a series** has no rate. `lagInFrame` returns the column default when
  there is no previous row — zero for a `Float64`, the epoch for a `DateTime64` — and the
  arithmetic accepts both, giving a plausible rate over fifty-six years.

A rate over a rollup is refused. A rollup stores the *average* of a counter per bucket,
and the difference between two averages of a monotonic counter is a number that is not a
rate of anything. That one was found by a test written expecting the refusal: rollup
aggregations are chosen by function, so `avg` became `avgMerge(avg_v)` and the field was
never consulted.

### Two found by the test suite racing itself

**A KEK rotation could destroy a credential.** `rotate_kek` read a row, unwrapped its
DEK, re-wrapped it under the new key and wrote the wrapping back — unconditionally. If
the credential was rotated in between (`put` reuses the id, so a rotation replaces the
row's DEK and ciphertext), the row ended up wrapping the *old* DEK over the *new*
ciphertext. Unwrapping yields a key that decrypts nothing, and the credential is
permanently unopenable with no error anywhere until somebody tries to use it.

Found as an intermittent `Open` in a test that had nothing to do with KEK rotation: the
rotation test re-wraps every row in the database, deliberately, and raced the credential
rotation in a neighbouring test. `replace_wrapping` is a compare-and-set now — one
statement, `WHERE id = $1 AND wrapped_dek = $5`, so there is no window between the check
and the write — and returns `Rewrapped::Superseded` rather than pretending it wrote.
Counted separately from `failed` in the report, because nothing went wrong: the row is
newer than the rotation, and the next rotation picks it up.

**Fixtures outside the retention TTL.** `the_explorer_histogram_is_served_from_the_pre_aggregate`
failed about one run in fifteen with an empty result. The ClickHouse fixtures were dated
`1_700_000_000` — 2023-11-14 — and every table they write to has a retention TTL
(`metrics` 30 days, the rest 365). The rows were inserted and then removed by a
background TTL merge, so whether a test passed depended on whether that merge had run
against its part yet. A `SELECT` at the time found 335 rows still in `logs` for that
window and zero in `logs_counts_5m`.

Both fixtures are anchored to now and truncated to a five-minute boundary, and each file
carries `the_fixtures_are_inside_every_retention_window` — a plain assertion with no
timing in it, so the day somebody writes a fixed timestamp again it fails on the first
run rather than once a fortnight.

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
3. ~~**M2 · the real transport.**~~ Done: `UdpTransport`, `snmp2`-backed, one pooled
   session per device. Two request shapes — a `GETBULK` run for walks and a `GET` for a
   scalar set — which is what `Work::Scalars`' "one request" actually needs.
4. **M2 · the poller binary.** Every part exists and is tested; what
   is missing is the process that joins them. In order:
   1. ~~Load profiles.~~ Done: `seed_builtin_profiles`, `profiles_for`, `put_profile`.
   2. ~~The loop.~~ Done: `uops_poll::poller::{Schedule, tasks, run_tick}`.
   3. ~~Samples.~~ Done: `uops_poll::sample::{scalars, interface_columns}`.
   3a. ~~Credentials that survive a restart.~~ Done: `PgSealedStore`. Found while
      starting the binary — the `credential` table and the `SealedStore` trait both
      existed, but nothing joined them, so the poller could not have read a real
      credential.
   3b. ~~The binary.~~ Done: `uops-poller`. Connects, seeds profiles, loads every
      tenant's devices on an interval, ticks once a second, and wires a task to
      `UdpTransport` + `walk` + `sample` + ClickHouse. `tests/live.rs` asserts the whole
      path against real infrastructure and CI runs it; a mutation guard breaks the
      `mgmt_ip` join and requires it to fail.
      **Not done inside it:** the availability check. `generic-snmp` asks for ICMP,
      which needs a raw socket and therefore a privilege this process should not hold
      by default; TCP needs a port no built-in profile sets. The job is scheduled and
      counted as unsupported rather than silently succeeding — a check that always
      "passed" would report every device permanently up, which is worse than no
      availability at all.
      **Also not done:** a lease. Two pollers against one database would both schedule
      every device, doubling the load on the fleet and writing each sample twice. One
      process for now, said out loud in `main.rs`.
   4. ~~Discovery.~~ Done. Each named row of the interface walk becomes a child
      resource plus a `member_of` edge, in one transaction — a child with no edge is
      unreachable from the device it belongs to, which is what the **and** in the
      acceptance criterion is about. Matched across runs on the *name*, not `ifIndex`:
      the MIB promises an index is stable only "between re-initializations", so an
      index-keyed child would be re-created on every reboot. Migration 0008 is the
      partial unique index that makes it repeatable.
      **Not done:** an interface that stops appearing is left alone. Deleting it would
      orphan the telemetry that references it, and one missed walk is not proof a port
      was removed. `last_seen` stops advancing, which is the signal; acting on it is a
      product decision.

## M2 acceptance criteria, where they actually stand

| SPEC §M2 | State |
|---|---|
| 1 000 simulated agents at 60s, p95 < 5 s, no missed cycles | Met in `uops-poll`'s fleet test, against the simulator. **Not** re-measured through the binary |
| SNMPv3 authPriv SHA-256/AES-256 against a real device, credential through `SecretStore` with an access-log entry | Met. `tests/agent.rs` for the wire, `tests/live.rs` for the credential path. The access-log entry is written but the log is in-memory — the `credential_access` table is M3 |
| Interface discovery creates child resources **and** `member_of` relationships | Met. Asserted against the real agent in `tests/live.rs` — the container's `eth0` and `lo` become resources with edges — and two CI mutations require the suite to fail: one writes the wrong edge kind, one breaks the rediscovery key |
| A 32-bit counter wrap produces no negative rate in any query | Met. `Field::Rate` compiles to a window over each series in `ClickHouse`; a backwards step yields `NULL`, which the aggregates skip. Asserted against real `ClickHouse` with a real wrap, and CI breaks the guard and requires the suite to fail — unguarded, the fixture reports −71 582 754 B/s |
| An unknown-vendor device gets interfaces and availability via `generic-snmp` | Met. Interfaces become resources; the ICMP check runs over an **unprivileged datagram socket**, so no `CAP_NET_RAW`. Verified on Linux against a real container — a state transition lands in `states` and in `resource.status` |
| Dead device does not delay healthy devices (measured, not assumed) | Met at both levels. `uops-poll`'s fleet test measures the executor; the scale test measures the loop above it, which is a different claim — a lock held across an await in the loop would serialise the fleet with the executor entirely innocent. Healthy p95 270 ms; with 100 silent devices, 258 ms. Guarded in CI by a mutation that serialises the executor |

## How to pick this up

```bash
docker compose -f deploy/docker-compose.yml up -d          # postgres + clickhouse
bash scripts/db.sh migrate && bash scripts/db.sh test      # 22 schema invariants
bash scripts/ch.sh apply && bash scripts/ch.sh verify      # 6 migrations, 12 golden
DATABASE_URL=postgres://uops:uops@localhost:5432/uops   CLICKHOUSE_USER=uops CLICKHOUSE_PASSWORD=uops   bash scripts/serve.sh                                      # http://127.0.0.1:8080
DATABASE_URL=postgres://uops:uops@localhost:5432/uops   CLICKHOUSE_USER=uops CLICKHOUSE_PASSWORD=uops   cargo test --workspace --all-targets                     # 527, all green
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
| **Shared-database contamination** | intermittent local failures | **Recurred, larger.** The development database had accumulated **2 608 tenants** from every integration test that ever panicked before its clean-up. Harmless until the poller existed; now a reload reads *every* tenant and issues two queries each, so an unswept database turned one reload into five thousand round trips and the live poller test from 2.6 s into 29 s. `db.sh sweep` now removes every tenant but `default` and everything under it, and the poller's live test takes its own scratch database rather than sharing. Earlier instance: the scale test seeded 10 000 resources and did not remove them; four runs left 40 400 rows in the database every other suite shares, which changes what the planner chooses for all of them. It cleans up after itself now, and `db.sh sweep` removes what an interrupted run leaves. This is the likely cause of the "one unreproduced failure" recorded earlier — both occurrences followed scale-test runs. Not proven, because it has not recurred since the purge |
| **Row-level security** | M1 API | Tenant isolation currently rests on `TenantScope`, composite foreign keys and sqlx. RLS would be a fourth layer and is worth having, but it needs an app role and a per-transaction `SET LOCAL` — a decision about connection pooling and the request lifecycle, so it belongs with the API |
| **Credential rollback vs. the primary key** | rotation being undoable | Migration 0005 says "rotation writes a new row rather than overwriting one … a rotation that turns out to be wrong is undone by revoking a row". Neither implementation does that: `LocalVault::put` reuses the credential's id, so both `PgSealedStore` (upsert on id) and `MemorySealedStore` (a map keyed by id) *replace* the previous version. The previous material is gone and revoking leaves nothing to fall back to. Reconciling them is a choice — keep the stable id so `resource.credential_ref` survives a rotation and drop the rollback claim, or key on `(id, version)` and make every reference resolve a version — so it is recorded rather than patched over in one implementation |
| **The poller is not in `docker compose`** | a stack that polls | The image builds every workspace binary but copies only `uops-server`, `uops-ch-migrate` and `uops-pg-migrate`, and compose has no poller service. So `docker compose up` gives an API and a UI over a database nothing is filling. Adding it needs a KEK in the compose environment, which is a decision about what a development stack may ship with |
| **The bundled IEEE data's terms** | a commercial release | `crates/uops-oui/data/assignments.tsv` is derived from the four public IEEE registries. They are redistributed widely — Wireshark, nmap and Debian's `ieee-data` all ship them — which is the basis for bundling. It is **not** a licence review: IEEE attaches no SPDX identifier, and `cargo deny` checks crate licences rather than the terms of embedded data, so nothing in CI is looking at this |
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
