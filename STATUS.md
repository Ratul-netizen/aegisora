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
CI, `uops-core`, `uops-secrets` and `uops-query` are done and pushed.

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
| M0 · PostgreSQL migrations | ⬜ |
| M0 · ClickHouse migration runner | ⬜ |
| M0 · `uops-bus` | ⬜ |
| M1–M4 | ⬜ |

## Resume in three commands

```bash
git clone https://github.com/Ratul-netizen/aegisora && cd aegisora
cargo test --workspace --all-targets && cargo test --workspace --doc   # 115 tests, green
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

ClickHouse 26.8.2.7, single node, warm cache. Full detail:
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

1. **PostgreSQL migrations** — the DDL in SPEC §M0.1/M0.2/M0.4.
2. **ClickHouse migration runner** — versioned, idempotent, resumable. On-prem customers
   upgrade unattended across multiple versions; a folder of hand-applied SQL will not survive.
3. **`uops-bus`** — `TelemetryBus` trait + `InProcess` impl + a conformance suite that both
   implementations run.

Then M1. Three of six M0 crates are done; realistic M0 completion is 2–4 weeks from here.

**Do not start yet:** the frontend, or any collector. Both are more fun than schema work
and both need rewriting if the resource model shifts.

---

## Open items

| Item | Blocks | Note |
|---|---|---|
| **CLA reviewed by a lawyer** | accepting outside contributions | Draft is in `CLA.md`, modelled on Apache ICLA. **The only irreversible item** — an unsigned contribution permanently forecloses dual-licensing |
| Product name | crate publishing only | `uops` codename unblocks everything else. Repo is still named `aegisora`, which was rejected (`aegisora-ai` is an active org in an adjacent market) |
| Buyer focus: MSP-first? | credential scoping depth in M1 | My recommendation was MSP-first; your read on Bangladesh/SEA overrides mine |
| Alias chain depth | M0.2 | Collapse-on-write recommended |
| Metrics + rollup ingest cost | M4, not M0 | The one W1 measurement not run |
| `metrics_1h` is emitted but not in the DDL | ClickHouse migration runner | `uops-query` plans onto it for windows past 30 days, per the raw→5m→1h rollup rule in SPEC §M0.6. The table itself still has to be created — a golden fixture already names it |

## Housekeeping

The benchmark ClickHouse container holds ~200M rows in a Docker volume (~12 GiB). It is
stopped, not deleted — `docker compose -f bench/docker-compose.yml up -d` brings it back
with data intact. To reclaim the space:
`docker compose -f bench/docker-compose.yml down -v`. Everything is regenerable from
seed 42.
