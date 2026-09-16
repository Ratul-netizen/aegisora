# Unified Infrastructure Observability Platform — Plan v2

Working codename: **uops** (`unified-observability`). Not the brand. See §1.
Status: pre-v0, architecture frozen for M0–M4. Implementation spec: [SPEC.md](./SPEC.md)
Revised: 2026-09-13

---

## 0. Decisions (frozen)

### Confirmed from v1

| Decision | Rationale |
|---|---|
| **Resource identity + correlation is the product** | Storage, polling and ingestion are commodities. This is the only part nobody hands you |
| **PostgreSQL (control) + ClickHouse (telemetry)** | Two engines, not four. See §3 |
| **OpenTelemetry first; own agent later** | OTel Collector already does host metrics, files, journald, Windows Event Log, syslog |
| **Rust + Axum / React + TypeScript** | |
| **Normalized telemetry envelope before the first collector** | §5 |
| **Query AST first, text query language later as a parser onto it** | Never a parallel code path |
| **SNMP / ICMP / Syslog / OTLP in v0.1; flow, topology UI, APM later** | §7 |
| **Hexora stays a separate repo** | Integration point is a findings API, not shared code |
| **Week 1 is a storage benchmark, not product code** | §9 |

### Changed in v2

| # | v1 said | v2 says | Why |
|---|---|---|---|
| 1 | ClickHouse text index is "new, validate it" | GA **10 Mar 2026**. Validate the *boundary*, not the feature | See §3 — the limits are now documented, not unknown |
| 2 | "credentials (sealed)" — one line | **Full secrets subsystem in M0** | §4. This system will hold SNMPv3 priv keys and SSH keys for network devices. It is not an enterprise feature |
| 3 | Topology deferred to v0.2 | **Relationship primitives in v0.1; topology *UI* in v0.2** | The correlation engine needs `DEPENDS_ON` edges to exist before it can use them, and interface discovery produces them for free |
| 4 | NATS JetStream as the bus | **`TelemetryBus` trait: in-process channels in v0.1, NATS at v0.2** | 10–100 devices on one box does not need a durable broker. Keep the boundary, defer the daemon |
| 5 | Device support unspecified | **Declarative monitoring profiles in M2** | The alternative is `if vendor == "Cisco"` metastasizing through the poller |
| 6 | Security analytics at v0.5 | **Security *engineering* from M0**; security *analytics* still v0.5 | Different things. Tenant isolation, mTLS, redaction, audit are M0 |
| 7 | "Log Explorer" as one line | **Investigation Workspace as a named first-class concept** with M0 consequences | §6 — it constrains ClickHouse sort keys *today* |
| 8 | ~2 years solo to v1.0 | Prototype 3–6mo · useful v0.1 6–12mo · strong v0.x 12–24mo · mature commercial 2–4+ yr | The v1 estimate was optimistic |
| 9 | Multi-tenancy "deferred" | **`Organization → Tenant → Site → Resource` hierarchy from day one**; MSP *features* deferred | Cheap now, structurally impossible later |

### Research baseline (unchanged, still valid)

- **SNMP in Rust is solved.** `snmp2` (v1/v2c/v3, traps, MIB, ~218K recent downloads) or `async-snmp`
  (async-first, SNMPv3 SHA-2/AES-256, FIPS 140-3 backend option, tooBig recovery, zero-copy).
- **NetFlow v9 / IPFIX solved.** `netgauze-flow-pkt` + `netgauze-flow-service` + `netgauze-ipfix-code-generator`.
- **sFlow is the gap.** No mature crate. Simple fixed binary format, ~1–2 weeks. M7.
- **OTLP ingestion nearly free.** `opentelemetry-proto` with `gen-tonic` emits tonic gRPC **server** stubs.
- **Rejected:** OpenSearch (search-first, JVM, ~10x nodes for the same retention at log volume);
  Quickwit (Datadog-owned since Jan 2025, team focused on Datadog integration);
  TimescaleDB for metrics (557K → 159K rows/s going to 10M hosts).

---

## 0b. Deployment model — both, as a customer choice

**Decision: on-premise and hosted/SaaS are both first-class targets. The customer picks.
Target buyers are unrestricted — any country's government, military, enterprise or MSP.**

The on-premise side is not a downgrade of the hosted product; for defence, law
enforcement and regulated buyers it is the *only* acceptable form, and it is often the
deal. The hosted side is what makes the product reachable for everyone else.

This reinforces the storage choice: single-node ClickHouse against an Elasticsearch
cluster is a large operability win exactly where on-prem hurts — no JVM heap tuning, no
shard rebalancing, no split-brain, no dedicated master nodes. The customer's ops team
runs two engines and a Compose file.

### Deployment profiles, not flags

Two **named profiles**, each selecting a coherent set of implementations. Arbitrary
mix-and-match config flags produce combinations nobody tests.

| | `onprem` | `hosted` |
|---|---|---|
| Secrets | `LocalVault` (KEK from file/keyring) | KMS / HashiCorp Vault |
| Auth | Local users, optional LDAP/AD | SSO / OIDC / SAML |
| Bus | In-process → NATS at scale | NATS JetStream |
| Storage tiering | **Opt-in**, single volume by default | On by default, S3 |
| Tenancy | Usually one tenant | Many tenants |
| Egress | **None. Ever.** | Normal |

### What this requires

| Requirement | Change |
|---|---|
| Validation attaches to a **binary**, not a code path | **Crypto backend is a build-time feature.** Ship a standard build and a FIPS build from one codebase. See SPEC §M0.4 |
| Defence/LE buyers audit *access*, not just change | **Read auditing**, not only mutation auditing. Schema + middleware change: trivial now, invasive later |
| Air-gapped and classified networks | **No phone-home, ever.** No auto-update check, no crash reporting, no license callback. Offline path for GeoIP data, OTel Collector distribution, container images |
| Supply-chain review is standard in procurement | **SBOM (CycloneDX/SPDX) + signed artifacts** in CI from the first release |
| Most on-prem sites have no object storage | **`storage_policy = 'tiered'` is opt-in.** Single volume must be the default that works |
| Disk is finite and someone else's | Text index (~67% of compressed) + `p_by_time` projection ≈ **3x raw compressed size**. Per-source text-index opt-out is **required**, not optional |
| Customers upgrade unattended, across multiple versions | ClickHouse migrations need a **real versioned runner** — idempotent, ordered, resumable |
| Buyers ask during evaluation, not after | **Backup/restore in the M1–M4 window**, out of "enterprise" |
| No server-side metering is possible on-prem | **Entitlement hooks** (resource counting) exist before they are enforced |
| Destructive automation in defence contexts | **Two-person integrity** on runbooks — note for M10, do not build now |

### The line to hold

Pursuing actual certification — FedRAMP, Common Criteria, STIG accreditation — is a
multi-year, six-to-seven-figure programme requiring dedicated compliance staff. **It is
not available to a solo developer and must not be attempted now.**

There is a large gap between *pursuing certification* and *not architecting yourself out
of it*. Everything in the table above is the second thing: cheap if done now, expensive
or impossible later. Build the seams; pursue no certification until a real buyer funds it.

**The failure mode to avoid** is letting hypothetical defence requirements distort v0.1.
That is how a monitoring platform becomes a compliance project that never ships a working
NMS.

**Not affected:** the resource model, identity resolution, telemetry envelope, Query AST,
and the M0–M4 scope.

---

## 1. Naming

The product name is **Veyronis** — *Unified Infrastructure Observability & Operations
Platform*.

`aegisora-ai/aegisora` is an active GitHub org doing AI runtime security and governance — adjacent
market, same buyer. **Do not use Aegisora.** That rejection is kept here rather than deleted,
because without it somebody proposes the name again.

No variants: not `Veyron`, `VeyronOS`, `Veyronis AI` or `Veyronis NMS`. The product is
`Veyronis` and the CLI is `veyronis`.

**Clearance has not been done.** The search behind `Veyronis` was preliminary. Clearance =
GitHub org + crates.io + npm + `.com`/`.io` + USPTO TESS classes 9 and 42 + Bangladesh RJSC if
incorporating locally.

**Code keeps the codename `uops` until clearance passes.** Decided 2026-09-16. Crates, binaries,
the 18 `UOPS_*` variables, the PostgreSQL role and database, the ClickHouse database, the Docker
images and the `uops.*` NATS subjects all stay as they are. This is what the codename was *for* —
`Aegisora` → `Veyronis` cost eight lines of documentation and no code, because the product name was
never baked into an identifier. Renaming them now would re-couple the tree to a name that has had a
search rather than a clearance, and pay the cost twice if clearance fails. It would also touch
persistent state — the database, the Docker volumes and `UOPS_KEK_FILE`, which points at the key
that decrypts every stored credential.

The identifier rename happens in one commit at clearance, alongside the volume and database
migration, which is also the first moment the crates can be published. Renaming a local workspace
is a `sed`; renaming a published crate is not. The inventory is in
[RENAME_AUDIT.md](./RENAME_AUDIT.md).

---

## 2. The moat

```
                    RESOURCE
                       │
          ┌────────────┼────────────┐
       METRICS        LOGS        TRACES
        FLOWS        EVENTS       CONFIG
          └────────────┼────────────┘
                    IDENTITY
                       │
                    TOPOLOGY
                       │
                  CORRELATION
                       │
                    INCIDENT
                       │
                 INVESTIGATION
                       │
                  AUTOMATION
```

Get identity and correlation right and ClickHouse/OTel/NATS are implementation details. Get them
wrong and a technically excellent NMS is still just another collection of dashboards.

Identity resolution carries **confidence + evidence** on every decision (`matched_by: [serial,
chassis_id]`, `confidence: 0.98`), with three outcomes: auto-merge, review queue, new resource.
Full rules in [SPEC.md §M0.2](./SPEC.md).

---

## 3. Storage

```
        CONTROL PLANE                          DATA PLANE
        ─────────────                          ──────────
         PostgreSQL                            ClickHouse
             │                                      │
  orgs, tenants, sites, users, RBAC   ┌─────┬───────┼───────┬──────┐
  resources + identifiers + aliases   │     │       │       │      │
  relationships (graph edges)      metrics logs  traces  flows  events
  sealed credentials                  │     │       │       │      │
  monitoring profiles                 └─────┴───────┼───────┴──────┘
  alert / automation rules                          │
  incidents, runbook history                 S3 / MinIO tiering
  dashboards, saved queries                (ClickHouse storage policy)
```

The topology graph lives in **PostgreSQL**, not a graph database. At NMS scale (<1M edges) recursive
CTEs are fast enough, and a third engine is not worth the operational cost to you or to every customer
who has to run it.

### The ClickHouse boundary — now precise

Full-text search is **GA as of 10 March 2026**: native inverted index, no false positives, built for
observability. But the GA announcement is explicit about what it deliberately is *not*:

| Capability | ClickHouse text index | Consequence for us |
|---|---|---|
| Token filtering (`hasToken`, any/all) | ✅ index-accelerated | Covers ~95% of ops log search |
| Filter + time range + `GROUP BY` + top-N | ✅ this is its home turf | Covers dashboards and analytics |
| Substring / n-gram | ✅ via n-gram tokenizer (costs index size) | Opt-in per field |
| **Phrase proximity** ("a" immediately before "b") | ❌ not index-accelerated — narrows granules, then linear scan | Acceptable; document the latency |
| **Relevance ranking (TF-IDF / BM25)** | ❌ explicitly not implemented | We don't need it. Ops search sorts by *time*, not relevance |
| **Fuzzy / edit-distance matching** | ❌ | Real gap if we ever want "did you mean" |

**Verdict: correct choice, known edges.** Ops log search is `filter + time range + aggregate + tail`,
sorted by timestamp. Relevance ranking is a search-product feature, not an ops feature.

**But keep the escape hatch.** `trait LogStore / MetricStore / TraceStore / FlowStore` are not
ceremony — they exist so that if BM25 or fuzzy matching ever becomes a real customer requirement, a
Tantivy-backed secondary index can be added beside ClickHouse without touching correlation, the API,
or the UI. The Query AST already has a `TextMatch` node with a `mode` field precisely so a second
backend has somewhere to plug in.

```
                Query AST
                    │
         ┌──────────┴──────────┐
   ClickHouse SQL       (future) Tantivy
   token · filter ·      fuzzy · BM25 ·
   aggregate · tail       proximity
```

---

## 4. Secrets (new in v2)

This platform will hold SNMPv3 auth/priv keys, SSH private keys, API tokens and cloud credentials for
a customer's entire network. A breach is a full network compromise, not a data leak.

```
              SecretStore trait
                      │
        ┌─────────┬───┴────┬──────────┐
   LocalVault    env/    HashiCorp   Cloud
   (v0.1)        KMS      Vault       KMS
   AES-256-GCM            (v0.4+)    (v0.4+)
   envelope enc
```

**M0 requirements, not enterprise ones:** envelope encryption (per-secret DEK wrapped by a KEK held
outside the database), secret versioning, KEK rotation without re-encrypting payloads, RBAC on
access, an audit record for *every* decrypt naming who/when/which resource/what for, and a tracing
layer that makes it structurally hard to log a secret. Resources reference a `credential_ref` UUID;
the material never enters the telemetry envelope, an API response, or a log line. Details in
[SPEC.md §M0.4](./SPEC.md).

---

## 5. Telemetry envelope

Formal, single, and shared by every signal. Aligned to **OpenTelemetry semantic conventions** rather
than a bespoke schema — interop for free, and it's what the trace/metric side already speaks.

```
TelemetryEnvelope
├── tenant_id · site_id · resource_id        (resource_id may be unresolved on arrival)
├── observed_at · ingested_at                (both — you need the delta to measure lag)
├── source        { kind, vendor, collector_id }
├── signal_type   metric | log | trace | flow | event | state
├── severity
├── attributes    (OTel semconv keys)
├── body          (signal-specific payload)
└── identity      (observed identifiers, pre-resolution)
```

`signal_type` as an enum with a per-signal body gives every source a common envelope without forcing
six different shapes into one table.

---

## 6. Investigation Workspace

Named as a product concept now, built at M9 — because **it constrains M0 decisions today.**

```
Incident INC-1001 ─→ Investigation
                     ├── Timeline (all signals, one axis)
                     ├── Metrics · Logs · Flows · Traces
                     ├── Topology (blast radius)
                     ├── Config changes
                     └── Related alerts · resources
```

The whole pitch is that a user stops jumping between Zabbix, Kibana, Grafana, a NetFlow analyzer and
a terminal. The M0 consequence: **every store must answer "all signals for resource R in window W"
cheaply.** That dictates ClickHouse `ORDER BY (tenant_id, resource_id, observed_at)` on every
telemetry table — a decision that is nearly free now and a full re-ingest later.

---

## 7. v0.1 scope

```
Control plane            Collectors           Storage       UI
─────────────            ──────────           ───────       ──
Axum API                 SNMP v2c/v3 poll     PostgreSQL    Resource list / detail
Auth (argon2id)          ICMP / TCP checks    ClickHouse    Metric graphs
RBAC (viewer/op/admin)   Syslog UDP/TCP/TLS                 Log explorer + saved searches
Org→Tenant→Site→Resource OTLP receiver                      Identity review / merge UI
Identity resolution      OTel Collector distro              Alert rules CRUD
  + confidence + review    (host metrics)                   One dashboard
Relationship model
Secrets subsystem
Monitoring profiles
Alert engine (threshold, absence)
Notify: email + webhook
```

**Deferred:** NetFlow, topology UI, APM/traces, correlation engine, incidents, automation, config
backup, security analytics, AI, MSP features, HA, distributed collectors, own agent, text query language.

The temptation to add NetFlow or topology visuals to v0.1 will be enormous. Don't.

---

## 8. Stack

```
Backend        Rust · Axum · tokio · sqlx
Bus            TelemetryBus trait → tokio mpsc (v0.1) → NATS JetStream (v0.2)
Control DB     PostgreSQL 16+
Telemetry DB   ClickHouse 25.x+ (Apache 2.0) — text index GA
Object store   MinIO / S3 (ClickHouse cold tier)
Agent          OpenTelemetry Collector, custom distribution
Frontend       React · TypeScript · Vite · TanStack Query
Charts         uPlot (perf) — not Chart.js at this volume
Deploy         Docker Compose → Helm at v0.3
```

Crates: `snmp2` or `async-snmp`, `netgauze-flow-pkt`/`-service`, `opentelemetry-proto` (gen-tonic,
server side), `clickhouse`, `sqlx`, `argon2`, `aes-gcm`, `tokio`, `tracing`.

---

## 9. First four weeks

| Week | Work | Gate |
|---|---|---|
| **W1** | ClickHouse benchmark. 100M + 1B logs, 10M + 100M metric points. 10 query shapes × p50/p95/p99, ingest rate, compression, concurrency | Go/no-go on §3 |
| **W2** | Resource identity: `resource`, `resource_identifier`, `resource_alias`, `resource_relationship`, confidence scoring, merge/split. Highest-leverage week in the project | Two sources resolve to one resource; a bad merge can be undone |
| **W3** | Telemetry envelope + Query AST as Rust types + DDL. Metric, Log, Event. Before any UI | AST → ClickHouse SQL round-trips under test |
| **W4** | Vertical slice: SNMP → identity resolution → normalize → ClickHouse → Query AST → API → React chart | One real device, one real metric, ugly UI |

If W4 works, every subsequent collector is an incremental addition to a proven path.

### W1 benchmark matrix

Volumes `100M logs · 1B logs · 10M metrics · 100M metrics` × queries:

```
1. Recent logs (tail 1000)        6. Top-N by field
2. Token search                   7. Substring / n-gram
3. Filter + time range            8. High-cardinality GROUP BY
4. Filter + GROUP BY              9. 30-day range scan
5. All signals for one resource  10. 180-day range scan
```

Record p50/p95/p99, CPU, RAM, disk, compression ratio, sustained ingest rate, concurrency ceiling.
Query 5 is the Investigation Workspace query — it is the one that must be fast.

Two queries are **expected to be slow by design** (phrase proximity, fuzzy). Run them anyway and
record the numbers, so the §3 boundary is measured rather than quoted.

---

## 10. Roadmap

| | Milestone | Content |
|---|---|---|
| **M0** | Architecture | Resource + identity model · telemetry envelope · security model · secrets · query AST · storage traits |
| **M1** | Core | PG · ClickHouse · Axum · React · auth · RBAC · inventory · identity resolution |
| **M2** | NMS | ICMP · TCP · SNMP v2c/v3 · OID metrics · **monitoring profiles** · interface monitoring |
| **M3** | Logs | Syslog UDP/TCP/TLS · OTel Collector · OTLP · normalization · text search · Log Explorer |
| **M4** | Metrics + dashboards | Metric storage · rollups · graphs · dashboard builder · alert rules · email/webhook |
| M5 | Discovery | CIDR · SNMP · LLDP/CDP · ARP · device classification |
| M6 | Topology | Graph · dependency model · topology UI · impact analysis |
| M7 | Flow | NetFlow v9 · IPFIX · sFlow (hand-written) · traffic analytics · GeoIP |
| M8 | Observability | OTLP traces · APM · service map · log↔metric↔trace correlation |
| M9 | Incident | Alert grouping · root cause · timeline · **Investigation Workspace** |
| M10 | Automation | Runbooks · SSH · APIs · approval · rollback · audit |
| M11 | Security analytics | Security events · detections · auth/firewall/VPN/DNS analytics |
| M12 | Enterprise | Multi-tenancy features · SSO · distributed collectors · HA · MSP |
| M13 | AI | Only after the data model and correlation actually work |

**M0–M4 are specified in [SPEC.md](./SPEC.md). M5+ are direction, not commitments.**

Effort: prototype 3–6 months · useful v0.1 6–12 months · strong v0.x 12–24 months · mature commercial
platform 2–4+ years. The point is not to wait four years — it is that something valuable exists at
month 9.

---

## 11. Open questions — blocking M1

1. **Name.** Blocks crate naming and the GitHub org. Codename `uops` unblocks everything else.
2. **License — recommendation: AGPL-3.0 + CLA, enabling commercial dual-licensing.**

   The straight AGPL recommendation was made *before* the on-premise decision (§0b) and is
   wrong on its own. On-prem enterprise procurement is exactly where AGPL gets blocked —
   many corporate legal teams maintain AGPL blocklists that apply even to purely internal
   use, and you would discover that mid-deal.

   Apache 2.0 is not the fix; it gives away the only asset. The fix is **AGPL-3.0 plus a
   Contributor License Agreement**, so a proprietary license can be sold to anyone whose
   lawyers object while AGPL covers everyone else (the GitLab / MongoDB play).

   **The time-critical part is the CLA, not the license text.** Without a CLA in place
   before the first outside contribution, dual-licensing becomes permanently impossible —
   it would require every contributor's consent. Put the CLA in place before publicising
   the repository.
3. **Target buyer.** MSPs in Bangladesh/SEA vs global self-hosters. The `Organization → Tenant`
   hierarchy is in from day one either way, but MSP-first pulls credential scoping, per-tenant data
   isolation guarantees and cross-tenant admin into v0.1. **The only open question that can still
   move v0.1 scope.**
4. Hexora boundary — findings API. No decision needed until M11.

---

## Sources

- ClickHouse full-text search GA (10 Mar 2026) and its stated limits — no TF-IDF/BM25, no positional
  data, phrase proximity not index-accelerated: https://clickhouse.com/blog/full-text-search-ga-release
- ClickHouse text index internals: https://clickhouse.com/blog/clickhouse-full-text-search · https://clickhouse.com/docs/engines/table-engines/mergetree-family/textindexes
- BM25 tracking issue: https://github.com/ClickHouse/ClickHouse/issues/92097
- ClickHouse vs Elasticsearch for logs: https://clickhouse.com/blog/elasticsearch-log-analytics-clickhouse
- ES/OpenSearch/Loki/Quickwit/ClickHouse comparison: https://blog.none.at/blog/2026/2026-05-14-es-os-loki-quickwit-clickhouse-guide/
- Datadog acquires Quickwit: https://www.datadoghq.com/blog/datadog-acquires-quickwit/
- TimescaleDB cardinality: https://sanj.dev/post/clickhouse-timescaledb-influxdb-time-series-comparison/
- SigNoz single-store architecture: https://signoz.io/blog/building-a-high-performance-log-store/
- Rust SNMP: https://lib.rs/crates/snmp2 · https://github.com/lukeod/async-snmp
- Rust NetFlow/IPFIX: https://crates.io/crates/netgauze-flow-pkt · https://crates.io/crates/netgauze-flow-service
- OTLP server stubs: https://docs.rs/opentelemetry-proto/
- OTel Collector: https://opentelemetry.io/docs/collector/ · https://opentelemetry.io/docs/specs/otel/logs/
