# Status — pick up from here

Last updated: 2026-09-16 · repo: `github.com/Ratul-netizen/veyronis`

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
| **`docker compose up` polls** | ✅ credentials, identifiers and the poller service |
| `uops-profile` | ✅ 40 — 5 built-ins, schema, resolution |
| `uops-poll` | ✅ 56 — wheel, jitter, counters, executor, planner, samples |
| `uops-snmp` | ✅ 42 — walk, simulator, `snmp2` over UDP, real net-snmp |
| `uops-poller` | ✅ 32 — the binary, end to end against real everything |
| interface discovery | ✅ children + `member_of`, matched on name |
| counter wrap → no negative rate | ✅ computed in `ClickHouse` at query time |
| ICMP availability | ✅ unprivileged datagram socket, no capability needed |
| p95 through the binary | ⬜ measured in the library only |
| **M3 · syslog parsing** | ✅ RFC 5424, RFC 3164, RFC 6587 framing |
| **M3 · syslog receivers** | ✅ UDP with drop counting, TCP with backpressure — 46 tests |
| **M3 · normalize + batch** | ✅ syslog → `LogRow` on semconv keys, batched inserts — 59 tests |
| M3 · syslog over TLS | ✅ **decided: terminated at a proxy**, not in-process |
| M3 · the syslog daemon, OTLP, Log Explorer | ⬜ |
| M4 | ⬜ |

## Resume in three commands

```bash
git clone https://github.com/Ratul-netizen/veyronis && cd veyronis
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

### UDP and TCP fail in opposite directions

They are written separately rather than behind one transport abstraction, because the
right behaviour under load is the opposite in each.

**UDP cannot push back.** A datagram that arrives with nowhere to go is gone, and the
sender will never know or retry. So the receiver drops it and *counts* it — SPEC: *"a
silently dropping syslog receiver is worse than none"*. It uses `try_send` rather than
`send` for exactly this: awaiting a full channel would stop reading the socket, and the
kernel would then drop the rest of the burst invisibly, which is the outcome SPEC is
warning about. Dropping in userspace is visible; dropping in the kernel is not.

**TCP can push back.** Not reading makes the receive window shrink, which makes the
sender slow down. So it uses `send` and waits. A TCP receiver that dropped under load
would be discarding something it could simply have taken more slowly.

What the drop counter does *not* include is datagrams the kernel discarded before this
process saw them. Those need `SO_RXQ_OVFL` and a `recvmsg` with control messages, which
is Linux-only and is not wired up. What is done instead is the half SPEC names: `SO_RCVBUF`
is raised explicitly, and **what the kernel actually granted is reported** — Linux caps
the request at `net.core.rmem_max`, which on a stock install is 208 KiB against the 8 MiB
asked for. A receiver that asked for 8 MB, silently got 208 KB and reported success would
be precisely the silent dropping the requirement exists to prevent.

### Putting the poller in compose was three pieces, not one

The plan was to copy a binary into the image and add a service. What that would have
produced is a poller logging "no credential is assigned" for every device for ever,
because **nothing in the product could create a credential**. The `credential` table had
existed since migration 0005 and had only ever been written by a test.

Then, with credentials possible, a device still could not be polled: `pollable_devices`
reads `resource_identifier` where `kind = 'mgmt_ip'`, deliberately — an address is what
identity resolution matches on rather than a column on the device — and no route could
write one. So the chain needed a third piece.

The result is that the path from `docker compose up` to a device being polled is now
four API calls, and it is in the README because nothing else would make it discoverable.

Two decisions inside that:

**Material goes in and never comes out.** No route returns a credential's material — not
for an administrator, not for an export, not for a "reveal" button. The value of envelope
encryption is that the material has one reader, and every additional path to it is one a
compromised session or a screenshot can take. `NewMaterial` and `CreateCredential` carry
hand-written `Debug` impls that redact, because a derived one puts a community string in
the first `{:?}` anybody reaches for.

**Manual identifiers are replaced wholesale and discovered ones are untouched.** One verb
gives add, change and remove — a route that only added would make a mistyped `mgmt_ip`
permanent, and a mistyped management address is a device polling somebody else's
equipment. The `source` column separates what an operator asserted from what a collector
observed, and an operator editing the inventory has no business deleting the evidence
identity resolution merged two resources on.

### The KEK is generated, not committed

`docker compose up` generates one into a named volume on first run. A key in a repository
is a key in every clone, every fork and every CI log, and the one certain thing about a
convenient development default is that somebody ships it.

The API and the poller read the same file. A credential the API sealed that the poller
cannot open is a device that silently never gets polled, which is the worst way for this
to be misconfigured — so there is one file and both mount it read-only.

`down` keeps the volume; `down -v` destroys it and with it the ability to decrypt every
stored credential. That is the only way to say it on purpose.

### Two inconsistencies the isolation harness found

Both were routes answering something other than 404 for another tenant's object, and
neither leaked anything — which is why they had survived.

`PgSealedStore::revoke` returned `Ok` however many rows it touched, so revoking another
tenant's credential answered **204**. `MemorySealedStore` had always reported `NotFound`;
the PostgreSQL implementation was the outlier, and an API built on it told an operator
"done" about something it had not touched.

`identifiers_for` returned **200 `[]`** for another tenant's resource — the same answer
as a resource with no identifiers, so no information crossed, but a 200 says "this exists
and is empty" where every other resource route says "not found".

The harness caught both only because it now builds its `AppState` with a real vault.
Without one the credential routes answer 503 before the scope check, and the test would
have been checking that a disabled feature leaks nothing.

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

0. **M3 · the syslog daemon.** Every piece from the wire to a batched insert now
   exists and is tested; what is missing is the process that joins them, and the one
   decision it needs is **how a message is attributed to a tenant**. A syslog message
   carries no tenant and cannot be made to. My proposal is a listener per tenant —
   address or port identifies the tenant, which is how every multi-tenant collector
   does it — with `create_provisional` for a sender that resolves to nothing, so the
   logs are kept and land in the review queue rather than being dropped. That is a
   product decision about what an unknown sender means, so it is named here rather
   than chosen quietly.
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
| **The bundled IEEE data's terms** | a commercial release | `crates/uops-oui/data/assignments.tsv` is derived from the four public IEEE registries. They are redistributed widely — Wireshark, nmap and Debian's `ieee-data` all ship them — which is the basis for bundling. It is **not** a licence review: IEEE attaches no SPDX identifier, and `cargo deny` checks crate licences rather than the terms of embedded data, so nothing in CI is looking at this |
| **CLA reviewed by a lawyer** | accepting outside contributions | Draft is in `CLA.md`, modelled on Apache ICLA. **The only irreversible item** — an unsigned contribution permanently forecloses dual-licensing |
| Buyer focus: MSP-first? | credential scoping depth in M1 | My recommendation was MSP-first; your read on Bangladesh/SEA overrides mine |
| Metrics + rollup ingest cost | M4, not M0 | The one W1 measurement not run |
| **Tiered storage policy** | deployment profiles | SPEC §M0.6 shows `TTL … TO VOLUME 'warm'/'cold'` against a `tiered` policy that does not exist on a default install — those migrations would fail outright. Retention is a plain `DELETE` TTL for now; tiering is a later migration, written alongside the profile that configures the policy |

## Decided since the last update

**The product is Veyronis; the code keeps the codename `uops` until clearance.**
`Aegisora` had already been rejected in PLAN.md §1 — `aegisora-ai/aegisora` is an active
org in an adjacent market — and `Veyronis` is the replacement.

The audit is in [RENAME_AUDIT.md](./RENAME_AUDIT.md), and its headline is that the
rename was **eight lines of documentation across four files**. Nothing else in the tree
ever contained the product name: not the 16 crates, not the binaries, not the 18
`UOPS_*` variables, not the PostgreSQL role or database, not the ClickHouse database,
not the Docker images, not the `uops.*` NATS subjects, not the npm package, not a
migration identifier, not a test fixture. That is exactly what the codename was for, and
it is the first time the bet has been tested.

So the identifiers stay `uops-*`. The rename to `veyronis-*` happens in one commit **at
clearance** — GitHub org, crates.io, npm, `.com`/`.io`, USPTO TESS classes 9 and 42,
Bangladesh RJSC — which is also the first moment the crates can be published. Doing it
now would re-couple the tree to a name that has had a preliminary search rather than a
clearance, and would pay the cost twice if clearance fails. It also touches persistent
state in a way the documentation rename does not: the database and role, the Docker
volumes, and `UOPS_KEK_FILE`, which points at the key that decrypts every stored
credential. That migration gets written when it is worth writing.

**The repository is now `github.com/Ratul-netizen/veyronis`.** Renamed by hand — `gh` is
not installed here and it needs repo-admin credentials. The four URLs in `Cargo.toml` and
this file followed in the same commit, `origin` was re-pointed, and a `git fetch` against
the new URL confirms it. GitHub keeps a redirect from the old name, so an existing clone
keeps working.

**Syslog over TLS is terminated at a proxy.** The open item asked for a decision and
this is it: no TLS in this process, for syslog or anything else. `rustls`' two
production crypto providers both carry OpenSSL-licensed code — `aws-lc-rs` is
`ISC AND MIT AND OpenSSL`, `ring` includes BoringSSL-derived sources under the same
terms — and neither is on `deny.toml`'s allow-list, which is already why the ClickHouse
and PostgreSQL clients were built without it.

The three ways out were: allow the OpenSSL licence, adopt the unaudited
`rustls-rustcrypto`, or terminate at a proxy. The proxy wins because it is what every
other transport here already does, it adds no dependency, and it keeps the licence
posture — no OpenSSL-derived crypto anywhere in the tree — that `deny.toml` exists to
hold. The cost is honest and belongs in the deployment docs rather than in a crate: an
on-premise install that needs syslog-over-TLS runs stunnel, HAProxy or rsyslog in front
of the TCP receiver and forwards plaintext over the loopback. That is a real
requirement on the operator, and the alternative was an unaudited TLS stack handling
untrusted input from the network, which is worse.

**The pipeline is where syslog stops being syslog.** `normalize` and `batch` are the
two halves of it, and both are deliberately about *not* being syslog-shaped.

`normalize::to_row` maps `hostname` → `host.name`, `app_name` → `service.name` and
`proc_id` → `process.pid` — OpenTelemetry semantic conventions, the same keys the
metrics path already writes and the Query AST already knows. This is the whole product
in one function: a log from a switch and a metric from the same switch are only
correlatable if they agree what the host is called, and the moment one of them stores
`syslog.hostname` instead, the join silently stops existing while both tables still
look fine on their own. The fields with no semconv equivalent keep a `syslog.` prefix
so they cannot be mistaken for a convention that exists.

Which clock wins is decided here too. `observed_at` is the device's timestamp when
there is one and the receipt time when there is not; `ingested_at` is always receipt.
A device with an unreadable clock would otherwise land at the Unix epoch and sort to
the top of every search, which is worse than being a few seconds out — and when the
substitution happens it is recorded in `syslog.timestamp.missing`, so a timeline nobody
can trust is at least one somebody can question.

`batch` turns a stream of rows into the few large inserts ClickHouse wants: 10 000 rows
or one second, whichever comes first. A failed insert retries with doubling backoff to
30 s rather than dropping, because a ClickHouse restart is a routine event and losing
the logs written during one is exactly the failure syslog receivers are notorious for.
The buffer is bounded at 500 000 rows and past that it drops the **oldest** — the
newest rows are the ones an operator is looking at during the incident that caused the
backlog.

`normalize::identifiers` offers the resolver the sender's address as `mgmt_ip` (0.80)
before the claimed hostname (0.65). The address is the strong one in practice because
it is the identifier the poller already wrote, so a switch that is both polled and
logging resolves to one resource with nothing configured; the hostname is whatever
somebody typed into the device, and a relay forwards messages whose hostname is not its
own. Both are offered and the resolver weighs them.

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

## The ledger — progress, and what went wrong getting here

Asked for explicitly. Progress is easy to find above; the failures are the part worth
keeping, because each one changed how something is built.

### Progress

| Milestone | State | Evidence |
|---|---|---|
| W1 storage benchmark | ✅ | `docs/benchmarks/w1.md` — the architecture bet, measured |
| M0 primitives | ✅ | envelope, identity, Query AST, secrets, migrations, bus |
| M1 core platform | ✅ | API, auth, inventory, telemetry, web shell, 10 000-resource p95 75 ms |
| M2 NMS | ✅ | all six acceptance criteria met **and measured**, each with a CI mutation guard |
| M5 device identity, M6 map, OUI | ✅ | taken out of order because they were asked for |
| M3 logs | 🟡 | wire → row is done; the daemon, OTLP and the Explorer are not |
| M4 dashboards & alerting | ⬜ | |

Roughly 690 tests on Windows, one more on Linux, against real PostgreSQL, real
ClickHouse and a real net-snmp agent. No mocked infrastructure anywhere in the
integration suites.

### Failures, and what each one cost

| What broke | How it was found | What changed because of it |
|---|---|---|
| **KEK rotation destroyed credentials** | reading the code while writing `PgSealedStore` | `rotate_kek` read a row, unwrapped its DEK and wrote the wrapping back *unconditionally*. A concurrent `put` on the same id left the row wrapping the old DEK over new ciphertext — silently undecryptable. Now a compare-and-set on `wrapped_dek`, returning `Rewrapped::Superseded`. This is the worst bug found so far: it destroys data and nothing reports it until someone needs the credential |
| **CI was red and nothing said so** | five commits later | the `check` job ran the workspace suite with no databases. The suite now runs once, in the job that has both engines |
| **2 608 leaked test tenants** | the live poller test went from 2.6 s to 29 s | every integration test that panicked before its cleanup left a tenant. Harmless until the poller existed, then a reload read all of them. `db.sh sweep` rewritten; the poller's live test takes its own scratch database |
| **ClickHouse fixtures outside the retention TTL** | intermittent, then reproducible | fixtures dated `1_700_000_000` against 30- and 365-day TTLs; background merges deleted them mid-run. Anchored to now and bucket-aligned. My first fix recomputed per call and broke three tests whenever a run crossed a 5-minute boundary — memoised with `OnceLock` |
| **Six tests passed on Windows and failed on Linux** | the Linux container | `fs::write` leaves 0644 and `KekRing::from_file` refuses a group-readable key. The fixtures now `set_permissions(0o600)` under `#[cfg(unix)]`. The refusal was correct; the tests were wrong |
| **A pre-aggregated query floored the wrong way** | a real histogram against a real rollup | a window starting partway through a bucket dropped the leftmost bar of every histogram. An Explorer opened at 14:37 silently omitted 14:35 |
| **The simulator modelled a GET as a GETNEXT** | a scalar that was invisible in tests but present on the real agent | `entPhysicalSoftwareRev` could never have been read. The simulator now has a real `get_scalars`. A simulator that is wrong in the same direction as the code under test proves nothing |
| **`Runner::load` had a trap** | the scale test was measuring nothing | discovery rules lived in a side map populated only inside `run::reload`, so the 1 000-device scale test measured 1 000 devices whose every discovery job failed. `load()` now does both and is the only way in |
| **Two routes leaked tenant existence** | the isolation harness, once it was given a real vault | `revoke` returned 204 for another tenant's credential and `identifiers_for` returned `200 []`. Both now `NotFound` — 404-never-403 |
| **My own documentation was false** | checking the claim against the data | I wrote that a 24-bit-only OUI lookup returns the *wrong* vendor. The data shows zero MA-M/MA-S nesting inside listed MA-L blocks, so it returns *nothing*. The wrong version was the intuitive one, which is why it survived review |
| **A runaway Python process** | 86 000 s of CPU over 25 hours | an orphan from a malformed heredoc. Heredocs through this shell are now written with a file tool instead |
| **Two test fixtures had arithmetic errors** | writing the assertions | a framed length off by one, and `<189>` asserted as `error` when it is `notice`. Both mine, both in the tests rather than the code |

The pattern that catches most of these: implement → test against real infrastructure →
**mutate the code and require the suite to fail** → add that mutation as a CI guard.
Every M2 acceptance criterion has one. A test that has never been seen to fail is not
evidence.

---

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
