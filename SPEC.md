# Implementation Specification — M0 through M4

Codename `uops`. Companion to [PLAN.md](./PLAN.md).
Scope: everything needed to build v0.1. M5+ is deliberately absent.
Revised: 2026-09-13

> **How to read this.** M0 is design that must be finished before collectors exist; it is mostly
> types, DDL and rules. M1–M4 are buildable milestones, each with acceptance criteria that are
> testable rather than aspirational. Where a decision is still open it is marked **[OPEN]** and must
> be closed before the milestone starts — not during.

---

## Conventions

### Workspace layout

```
uops/
├── Cargo.toml                    # workspace
├── crates/
│   ├── uops-core/                # M0: envelope, resource, identity types, errors
│   ├── uops-query/               # M0: Query AST + ClickHouse codegen
│   ├── uops-store/               # M0: storage traits
│   │   ├── uops-store-pg/        # PostgreSQL impl (control plane)
│   │   └── uops-store-ch/        # ClickHouse impl (telemetry)
│   ├── uops-secrets/             # M0: SecretStore trait + LocalVault
│   ├── uops-bus/                 # M0: TelemetryBus trait + InProcess impl
│   ├── uops-identity/            # M1: identity resolution service
│   ├── uops-api/                 # M1: Axum HTTP/WS surface
│   ├── uops-collector-snmp/      # M2
│   ├── uops-collector-check/     # M2: ICMP/TCP
│   ├── uops-profiles/            # M2: monitoring profile engine
│   ├── uops-collector-syslog/    # M3
│   ├── uops-collector-otlp/      # M3
│   ├── uops-pipeline/            # M3: normalize → resolve → enrich → batch
│   ├── uops-alerts/              # M4
│   └── uops-server/              # binary: wires everything
├── web/                          # React + Vite
├── migrations/                   # sqlx (PostgreSQL)
├── ch-migrations/                # ClickHouse DDL, hand-applied + versioned
├── profiles/                     # built-in monitoring profiles (YAML)
└── deploy/docker-compose.yml
```

### Rules

- **Every** table in PostgreSQL and ClickHouse carries `tenant_id`. No exceptions, including
  lookup tables. A table without it is a bug.
- No SQL string interpolation anywhere. PostgreSQL goes through `sqlx` compile-time-checked
  queries; ClickHouse goes through the Query AST codegen (§M0.5) which binds parameters.
- All timestamps are `DateTime64(3, 'UTC')` / `timestamptz`. No local time anywhere below the UI.
- IDs are UUIDv7 (time-ordered — matters for ClickHouse sort keys and PG index locality).
- Errors: one `uops_core::Error` enum per crate boundary, `thiserror` internally,
  `anyhow` only in binaries. API maps to RFC 7807 problem+json.
- `#![forbid(unsafe_code)]` in every crate. `clippy::pedantic` warn-level in CI.

---

# M0 — Architecture primitives

No collectors, no UI. Output is types, DDL, and written rules. This milestone is "done" when
someone else could implement M1–M4 from it without asking you a question.

## M0.1 Resource model

The hierarchy, from day one (PLAN §0 change #9):

```
Organization ──┬── Tenant ──┬── Site ──┬── Resource ──┬── Resource (child)
               │            │          │              └── Resource (child)
               └── Tenant   └── Site   └── Resource
```

`Organization` exists so an MSP can own many tenants without cross-tenant data access.
A single-company deployment has exactly one organization and one tenant; the cost is one join.

```sql
CREATE TYPE resource_kind AS ENUM (
  'device', 'interface', 'host', 'vm', 'container', 'service',
  'application', 'database', 'cloud_resource', 'site'
);

CREATE TYPE resource_status AS ENUM ('up','down','degraded','unknown','maintenance','decommissioned');

CREATE TABLE organization (
  id           uuid PRIMARY KEY,
  name         text NOT NULL,
  created_at   timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE tenant (
  id           uuid PRIMARY KEY,
  org_id       uuid NOT NULL REFERENCES organization(id),
  name         text NOT NULL,
  slug         text NOT NULL,
  created_at   timestamptz NOT NULL DEFAULT now(),
  UNIQUE (org_id, slug)
);

CREATE TABLE site (
  id           uuid PRIMARY KEY,
  tenant_id    uuid NOT NULL REFERENCES tenant(id),
  name         text NOT NULL,
  timezone     text NOT NULL DEFAULT 'UTC',
  UNIQUE (tenant_id, name)
);

CREATE TABLE resource (
  id             uuid PRIMARY KEY,
  tenant_id      uuid NOT NULL REFERENCES tenant(id),
  site_id        uuid REFERENCES site(id),
  parent_id      uuid REFERENCES resource(id),      -- interface → device, container → host
  kind           resource_kind NOT NULL,
  name           text NOT NULL,                     -- canonical, system-chosen
  display_name   text,                              -- user override, never auto-written
  vendor         text,
  model          text,
  os             text,
  os_version     text,
  status         resource_status NOT NULL DEFAULT 'unknown',
  profile_id     uuid,                              -- → monitoring_profile, M2
  credential_ref uuid,                              -- → secret, M0.4. Never the material
  attributes     jsonb NOT NULL DEFAULT '{}',       -- OTel semconv keys
  first_seen     timestamptz NOT NULL DEFAULT now(),
  last_seen      timestamptz NOT NULL DEFAULT now(),
  created_at     timestamptz NOT NULL DEFAULT now(),
  updated_at     timestamptz NOT NULL DEFAULT now()
);

CREATE INDEX ON resource (tenant_id, kind);
CREATE INDEX ON resource (tenant_id, parent_id);
CREATE INDEX ON resource USING gin (attributes jsonb_path_ops);
```

### Relationships — in v0.1 (PLAN §0 change #3)

The topology *UI* waits for M6. The *edges* do not, because interface discovery (M2) produces them
for free and the correlation engine (M9) cannot be designed against a table that doesn't exist.

```sql
CREATE TYPE relationship_kind AS ENUM (
  'connected_to',   -- L2/L3 adjacency (M5/M6)
  'depends_on',     -- service → database
  'hosts',          -- hypervisor → vm, host → container
  'runs',           -- host → service
  'routes_to',      -- L3 next-hop
  'member_of'       -- interface → device, node → cluster
);

CREATE TABLE resource_relationship (
  id            uuid PRIMARY KEY,
  tenant_id     uuid NOT NULL REFERENCES tenant(id),
  source_id     uuid NOT NULL REFERENCES resource(id) ON DELETE CASCADE,
  target_id     uuid NOT NULL REFERENCES resource(id) ON DELETE CASCADE,
  kind          relationship_kind NOT NULL,
  confidence    real NOT NULL DEFAULT 1.0 CHECK (confidence BETWEEN 0 AND 1),
  discovered_by text NOT NULL,                      -- 'lldp' | 'manual' | 'snmp-iftable' | 'otel'
  attributes    jsonb NOT NULL DEFAULT '{}',
  first_seen    timestamptz NOT NULL DEFAULT now(),
  last_seen     timestamptz NOT NULL DEFAULT now(),
  UNIQUE (tenant_id, source_id, target_id, kind)
);

CREATE INDEX ON resource_relationship (tenant_id, source_id, kind);
CREATE INDEX ON resource_relationship (tenant_id, target_id, kind);
```

Traversal is a recursive CTE. Write it once in M0 as a reusable function so M6 and M9 don't each
invent their own:

```sql
-- blast radius: everything that depends on $1, up to $2 hops
CREATE FUNCTION resource_dependents(root uuid, max_depth int)
RETURNS TABLE (resource_id uuid, depth int, path uuid[]) AS $$
  WITH RECURSIVE walk AS (
    SELECT r.source_id, 1 AS depth, ARRAY[r.target_id, r.source_id] AS path
      FROM resource_relationship r
     WHERE r.target_id = root AND r.kind IN ('depends_on','member_of','hosts','runs')
    UNION ALL
    SELECT r.source_id, w.depth + 1, w.path || r.source_id
      FROM resource_relationship r
      JOIN walk w ON r.target_id = w.source_id
     WHERE w.depth < max_depth
       AND NOT r.source_id = ANY(w.path)          -- cycle guard, mandatory
  )
  SELECT source_id, depth, path FROM walk;
$$ LANGUAGE sql STABLE;
```

The cycle guard is not optional. Real networks contain relationship cycles and the first one will
hang the API worker.

## M0.2 Identity resolution

The highest-leverage component in the system. Everything downstream is worthless if this is wrong.

### Storage

```sql
CREATE TYPE identifier_kind AS ENUM (
  'chassis_id',        -- LLDP chassis ID
  'serial',            -- entPhysicalSerialNum
  'snmp_engine_id',    -- SNMPv3 engine ID
  'otel_host_id',      -- OTel host.id
  'mgmt_ip',
  'mac',
  'hostname',          -- sysName, syslog HOSTNAME, host.name
  'flow_exporter',     -- exporter IP for NetFlow/IPFIX
  'service_name'       -- OTel service.name
);

CREATE TABLE resource_identifier (
  id           uuid PRIMARY KEY,
  tenant_id    uuid NOT NULL REFERENCES tenant(id),
  resource_id  uuid NOT NULL REFERENCES resource(id) ON DELETE CASCADE,
  kind         identifier_kind NOT NULL,
  value        text NOT NULL,
  confidence   real NOT NULL CHECK (confidence BETWEEN 0 AND 1),
  source       text NOT NULL,                       -- 'snmp' | 'syslog' | 'otlp' | 'manual'
  first_seen   timestamptz NOT NULL DEFAULT now(),
  last_seen    timestamptz NOT NULL DEFAULT now(),
  UNIQUE (tenant_id, kind, value)                   -- ← the resolution index
);

CREATE INDEX ON resource_identifier (tenant_id, resource_id);
```

That `UNIQUE (tenant_id, kind, value)` is the whole mechanism: resolution is a lookup, not a scan.
A violation is not an error — it is a genuine identity conflict and belongs in the review queue.

### Confidence rules

Ordered. First match wins; additional matches *raise* confidence via the combination rule.

| Tier | Identifier | Base confidence | Note |
|---|---|---|---|
| 1 | `serial` | 1.00 | Globally unique by definition |
| 1 | `chassis_id` | 1.00 | LLDP, MAC-derived |
| 1 | `snmp_engine_id` | 1.00 | Unique per SNMPv3 agent |
| 1 | `otel_host_id` | 1.00 | Machine ID |
| 2 | `mac` | 0.90 | Unique, but NICs move between chassis |
| 3 | `mgmt_ip` | 0.80 | DHCP and re-IP break this |
| 3 | `flow_exporter` | 0.80 | Same failure mode as mgmt_ip |
| 4 | `hostname` | 0.65 | Truncated, duplicated, and lied about constantly |
| 4 | `service_name` | 0.60 | Not a host at all — often maps to many resources |

**Combination rule.** Independent matches compound as noisy-OR:

```
combined = 1 − Π(1 − cᵢ)
```

So `mgmt_ip` (0.80) + `hostname` (0.65) → `1 − (0.20 × 0.35)` = **0.93** → review, not auto-merge.
Add `mac` (0.90) → **0.993** → auto-merge. This is deliberately conservative: a wrong auto-merge
silently corrupts every downstream correlation, while a review-queue item costs someone ten seconds.

**Contradiction rule.** If two tier-1 identifiers disagree (same `mgmt_ip`, different `serial`),
confidence is forced to `0.0` and a **new resource** is created. Tier-1 disagreement means the box
was physically replaced; it is not the same resource and must not inherit its history.

### Outcomes

```
confidence ≥ 0.95   →  auto-merge, record decision
0.60 – 0.95         →  create provisional resource + review queue entry
< 0.60              →  create new resource
tier-1 contradiction →  create new resource, flag predecessor as 'decommissioned' candidate
```

### Decision audit

Every decision is recorded. This is what makes identity mistakes debuggable instead of mystifying.

```sql
CREATE TABLE identity_decision (
  id             uuid PRIMARY KEY,
  tenant_id      uuid NOT NULL,
  resource_id    uuid,                              -- null if it created a new one
  outcome        text NOT NULL,                     -- 'auto_merge'|'review'|'new'|'manual_merge'|'manual_split'
  confidence     real NOT NULL,
  matched_by     jsonb NOT NULL,                    -- [{"kind":"serial","value":"FTX…","confidence":1.0}]
  observed       jsonb NOT NULL,                    -- full identifier set presented
  source         text NOT NULL,
  actor_id       uuid,                              -- set only for manual decisions
  decided_at     timestamptz NOT NULL DEFAULT now()
);
```

### Merge and split — in v0.1

```
POST /api/v1/resources/{id}/merge    { into: uuid, reason: string }
POST /api/v1/resources/{id}/split    { identifiers: [uuid], reason: string }
GET  /api/v1/identity/review         → pending items, newest first
POST /api/v1/identity/review/{id}    { action: 'merge'|'new', target?: uuid }
```

**Merge is reversible.** A merge writes an `identity_decision` capturing the pre-merge identifier
partition, so a split can restore it. Telemetry already written to ClickHouse under the old
`resource_id` is *not* rewritten — instead, `resource_alias` maps historical IDs:

```sql
CREATE TABLE resource_alias (
  tenant_id        uuid NOT NULL,
  historical_id    uuid NOT NULL,
  current_id       uuid NOT NULL REFERENCES resource(id),
  merged_at        timestamptz NOT NULL DEFAULT now(),
  decision_id      uuid REFERENCES identity_decision(id),
  PRIMARY KEY (tenant_id, historical_id)
);
```

Every telemetry query expands `resource_id` through `resource_alias` before hitting ClickHouse.
This makes merges O(1) instead of a re-ingest, and it is the reason the query layer must own
resource-ID expansion rather than each caller doing it.

**[OPEN]** Alias chain depth. A merged into B merged into C. Either collapse on write (rewrite
A→C when B→C is created) or resolve transitively at read. **Decide in W2**; collapse-on-write is
simpler and merges are rare. Recommend collapse-on-write.

### Resolution algorithm

```rust
pub struct ObservedIdentity {
    pub identifiers: Vec<(IdentifierKind, String)>,
    pub source: &'static str,
    pub hints: ResolutionHints,          // site_id, expected kind
}

pub enum Resolution {
    Matched   { resource_id: Uuid, confidence: f32, matched_by: Vec<Match> },
    Review    { provisional_id: Uuid, candidates: Vec<Candidate> },
    Created   { resource_id: Uuid },
}

#[async_trait]
pub trait IdentityResolver: Send + Sync {
    async fn resolve(&self, tenant: TenantId, obs: &ObservedIdentity) -> Result<Resolution>;
}
```

Implementation notes that matter:

- **Cache aggressively.** A syslog receiver at 50k msg/s cannot hit PostgreSQL per message.
  In-memory `(tenant_id, kind, value) → resource_id` LRU, invalidated on merge/split via a
  broadcast channel. Target: >99% hit rate steady-state.
- **Resolution happens once, in the pipeline** (§M3.3), not in each collector. Collectors emit
  envelopes with `resource_id: None` and a populated `identity` block.
- **Never block ingestion on resolution.** An unresolvable envelope gets a provisional resource
  rather than being dropped. Dropped telemetry during an incident is the worst possible failure.

## M0.3 Telemetry envelope

```rust
pub struct TelemetryEnvelope {
    pub tenant_id:   TenantId,
    pub site_id:     Option<SiteId>,
    pub resource_id: Option<ResourceId>,   // None until the pipeline resolves it
    pub identity:    Option<ObservedIdentity>,

    pub observed_at: DateTime<Utc>,        // when the source saw it
    pub ingested_at: DateTime<Utc>,        // when we got it — both, always

    pub source:      Source,
    pub severity:    Option<Severity>,
    pub attributes:  AttrMap,              // OTel semconv keys
    pub signal:      Signal,
}

pub struct Source {
    pub kind:         SourceKind,          // Snmp|Syslog|Otlp|Icmp|Flow|Trap|Internal
    pub vendor:       Option<String>,
    pub collector_id: String,
}

pub enum Signal {
    Metric(MetricPoint),
    Log(LogRecord),
    Trace(SpanRecord),
    Flow(FlowRecord),
    Event(EventRecord),
    State(StateRecord),    // availability / status transitions
}

pub struct MetricPoint { pub name: String, pub value: f64,
                         pub unit: Option<String>, pub kind: MetricKind,   // Gauge|Counter|Histogram
                         pub labels: AttrMap }

pub struct LogRecord   { pub body: String, pub facility: Option<u8>,
                         pub trace_id: Option<[u8;16]>, pub span_id: Option<[u8;8]> }

pub struct EventRecord { pub category: String, pub event_type: String,
                         pub summary: String, pub details: AttrMap }

pub struct StateRecord { pub previous: ResourceStatus, pub current: ResourceStatus,
                         pub reason: String }
```

Attribute keys follow **OpenTelemetry semantic conventions** — `server.address`,
`network.protocol.name`, `source.address`, `host.name`. Not `src_ip`/`dst_ip`. This costs nothing
now and buys interop with the entire OTel ecosystem later.

`AttrMap` is `BTreeMap<Cow<'static, str>, AttrValue>` — ordered for deterministic serialization,
`Cow` because semconv keys are static strings 95% of the time.

**Rule: a secret can never appear in an envelope.** Enforce with a `#[serde(skip)]`-free design —
the envelope has no field that can hold credential material, and the `Secret<T>` newtype (§M0.4)
does not implement `Serialize`.

## M0.4 Secrets subsystem

```rust
/// Wrapper that cannot be logged, serialized, or printed.
pub struct Secret<T>(T);
impl<T> fmt::Debug   for Secret<T> { fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result { f.write_str("Secret(<redacted>)") } }
impl<T> fmt::Display for Secret<T> { fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result { f.write_str("<redacted>") } }
// Deliberately NOT: Serialize, Clone (use .expose() explicitly)
impl<T> Secret<T> { pub fn expose(&self) -> &T { &self.0 } }
impl<T> Drop for Secret<T> { /* zeroize */ }

#[async_trait]
pub trait SecretStore: Send + Sync {
    async fn get(&self, tenant: TenantId, r: CredentialRef, ctx: AccessContext)
        -> Result<Secret<CredentialMaterial>>;
    async fn put(&self, tenant: TenantId, m: Secret<CredentialMaterial>, meta: CredentialMeta)
        -> Result<CredentialRef>;
    async fn rotate_kek(&self, new: KeyId) -> Result<RotationReport>;
    async fn versions(&self, tenant: TenantId, r: CredentialRef) -> Result<Vec<CredentialVersion>>;
    async fn revoke(&self, tenant: TenantId, r: CredentialRef) -> Result<()>;
}

pub struct AccessContext {
    pub actor:       Actor,          // User(uuid) | Collector(&'static str) | System
    pub resource_id: Option<Uuid>,   // what it is being used for
    pub purpose:     &'static str,   // "snmp-poll" | "ssh-runbook"
}
```

### Crypto backend is a BUILD-TIME feature

Target buyers are unrestricted — any country's government, military or enterprise
(PLAN §0b). Different jurisdictions mandate different validated cryptography, and
**validation attaches to a binary, not to an algorithm or a code path**: an AES-256-GCM
implementation is not "FIPS compliant" as source, only a specific validated module is.
A runtime toggle therefore cannot deliver it.

So the crypto primitive is selected at compile time, and one codebase ships as multiple
artifacts:

```toml
[features]
default     = ["crypto-rustcrypto"]
crypto-rustcrypto = ["aes-gcm", "argon2"]   # portable, no C toolchain, default build
crypto-awslc      = ["aws-lc-rs"]           # FIPS 140-3 build: cargo build --no-default-features --features crypto-awslc
```

```rust
/// The only primitive the rest of the codebase may use. Backends are selected by
/// feature, never at runtime. Adding a jurisdiction-specific backend later means a
/// new feature and a new build artifact, not a change to SecretStore or its callers.
pub trait AeadProvider: Send + Sync {
    fn seal(&self, key: &Key, nonce: &Nonce, aad: &[u8], pt: &[u8]) -> Result<Vec<u8>>;
    fn open(&self, key: &Key, nonce: &Nonce, aad: &[u8], ct: &[u8]) -> Result<Secret<Vec<u8>>>;
    /// Reported in the UI, in `/api/v1/health`, and stamped into the audit log, so an
    /// auditor can confirm which build is actually deployed.
    fn backend_id(&self) -> &'static str;   // "rustcrypto" | "aws-lc-fips"
}
```

**M0 obligations:** no crate outside `uops-secrets` may call an AEAD or KDF directly
(enforced by a CI grep, the same way `Secret<T>` misuse is caught); `backend_id` is
recorded on every credential row so a re-key after a backend change is detectable; and
the release pipeline produces both artifacts from the first tagged build, because adding
a second build target to a mature CI pipeline is materially harder than starting with two.

> **Do not pursue FIPS validation now.** This is the seam that keeps it *possible*, and
> that is all it is. See the line-to-hold note in PLAN §0b.

The same reasoning applies to SNMPv3 in M2: `async-snmp` offers a pluggable crypto backend
with a FIPS 140-3 option, `snmp2` does not. That is the tiebreaker between them.

### LocalVault (the v0.1 implementation)

Envelope encryption. Per-secret data encryption key (DEK), wrapped by a key encryption key (KEK)
that **never enters the database**.

```
CredentialMaterial ──AES-256-GCM(DEK)──→ ciphertext ──→ PostgreSQL
                DEK ──AES-256-GCM(KEK)──→ wrapped_dek ──→ PostgreSQL
                KEK ← env var | file (0600) | OS keyring | (M12) KMS/Vault
```

```sql
CREATE TABLE credential (
  id           uuid PRIMARY KEY,
  tenant_id    uuid NOT NULL REFERENCES tenant(id),
  name         text NOT NULL,
  kind         text NOT NULL,          -- 'snmpv3'|'snmp_community'|'ssh_password'|'ssh_key'|'api_token'
  version      int  NOT NULL DEFAULT 1,
  kek_id       text NOT NULL,          -- which KEK wrapped this DEK
  wrapped_dek  bytea NOT NULL,
  nonce        bytea NOT NULL,
  ciphertext   bytea NOT NULL,
  aad          bytea NOT NULL,         -- tenant_id||credential_id||version, binds ciphertext to identity
  created_at   timestamptz NOT NULL DEFAULT now(),
  revoked_at   timestamptz,
  UNIQUE (tenant_id, name, version)
);

CREATE TABLE credential_access_log (
  id            bigserial PRIMARY KEY,
  tenant_id     uuid NOT NULL,
  credential_id uuid NOT NULL,
  actor         text NOT NULL,
  resource_id   uuid,
  purpose       text NOT NULL,
  succeeded     bool NOT NULL,
  at            timestamptz NOT NULL DEFAULT now()
);
```

The `aad` field matters: AES-GCM additional authenticated data binds the ciphertext to its
tenant/credential/version, so swapping ciphertext rows between tenants fails decryption rather than
silently succeeding.

**KEK rotation is cheap by construction** — re-wrap DEKs, never touch ciphertext. Credential
rotation is a new `version` row; collectors read the highest non-revoked version.

### Non-negotiables for M0

- `tracing` subscriber layer that redacts on a key denylist (`password`, `community`, `auth_key`,
  `priv_key`, `token`, `secret`, `private_key`) regardless of nesting depth.
- Every `get()` writes `credential_access_log` — including failures.
- API never returns credential material. Not to admins. There is no "reveal" endpoint.
- CI check: `grep` the codebase for `Secret` used in a `format!`/`info!`/`json!` position. Cheap,
  catches the realistic mistake.

## M0.5 Query AST

One AST. The UI builds it, the API accepts it, alerts are saved instances of it, and a text query
language (M6+) will be a *parser onto it* — never a second path to the database.

```rust
pub struct Query {
    pub signal:       SignalType,
    pub time:         TimeRange,
    pub resources:    ResourceSelector,      // expands through resource_alias
    pub filter:       Option<Expr>,
    pub aggregations: Vec<Aggregation>,
    pub group_by:     Vec<Field>,
    pub order_by:     Vec<Sort>,
    pub limit:        u32,                   // hard cap, always set
    pub offset:       u32,
}

pub enum Expr {
    And(Vec<Expr>),
    Or(Vec<Expr>),
    Not(Box<Expr>),
    Compare { field: Field, op: CompareOp, value: Value },   // = != < <= > >= in not-in
    Text    { field: Field, mode: TextMode, terms: Vec<String> },
    Exists  { field: Field },
}

pub enum TextMode {
    AnyToken,      // index-accelerated  → searchAny()
    AllToken,      // index-accelerated  → searchAll()
    Substring,     // n-gram index if declared, else scan
    Phrase,        // NOT index-accelerated — narrows granules then scans. Warn in UI.
}

pub enum ResourceSelector {
    All,
    Ids(Vec<ResourceId>),
    Kind(ResourceKind),
    Site(SiteId),
    Descendants { root: ResourceId, max_depth: u8 },   // uses resource_dependents()
}
```

### Codegen contract

`uops-query` compiles `Query` → parameterized ClickHouse SQL. Requirements:

1. **`tenant_id` is injected by the compiler, not by the caller.** The compile function signature is
   `fn compile(q: &Query, scope: &TenantScope) -> Sql`. There is no way to produce SQL without a
   scope. This is the single most important line of defence for tenant isolation.
2. `ResourceSelector` resolves through `resource_alias` *before* SQL generation.
3. Every `limit` has a server-side ceiling (default 10 000; `tail` uses a separate streaming path).
4. `TextMode::Phrase` and `Substring`-without-n-gram-index emit a `QueryWarning` returned alongside
   results, so the UI can show "this query cannot use the index."
5. Golden tests: a fixture directory of `Query` JSON → expected SQL. Every codegen change diffs
   against it. This is how the AST stays trustworthy while ClickHouse syntax evolves.

## M0.6 Storage traits and ClickHouse DDL

```rust
#[async_trait]
pub trait TelemetryStore: Send + Sync {
    async fn query(&self, q: &Query, scope: &TenantScope) -> Result<ResultSet>;
    async fn health(&self) -> Result<StoreHealth>;
}

#[async_trait] pub trait MetricStore: TelemetryStore { async fn insert(&self, b: Vec<MetricRow>) -> Result<()>; }
#[async_trait] pub trait LogStore:    TelemetryStore { async fn insert(&self, b: Vec<LogRow>)    -> Result<()>;
                                                       async fn tail(&self, q: &Query, scope: &TenantScope)
                                                           -> Result<BoxStream<'_, LogRow>>; }
#[async_trait] pub trait TraceStore:  TelemetryStore { async fn insert(&self, b: Vec<SpanRow>)   -> Result<()>; }  // M8
#[async_trait] pub trait FlowStore:   TelemetryStore { async fn insert(&self, b: Vec<FlowRow>)   -> Result<()>; }  // M7
```

Trace and flow traits are **declared in M0 and unimplemented until M7/M8**. Declaring them now costs
an hour and forces the envelope and AST to be general enough that adding them later is additive.

### ClickHouse schemas

> Verify index and search-function syntax against your installed ClickHouse version before applying —
> the text index reached GA in March 2026 and syntax moved during beta. The W1 benchmark is where
> this gets confirmed.

**Sort key is the same on every telemetry table: `(tenant_id, resource_id, observed_at)`.**
This is the Investigation Workspace decision from PLAN §6 — "all signals for resource R in window W"
must be a contiguous range read on every table. It is nearly free now and a full re-ingest later.

```sql
CREATE TABLE logs
(
    tenant_id     UUID,
    resource_id   UUID,
    site_id       UUID,
    observed_at   DateTime64(3, 'UTC'),
    ingested_at   DateTime64(3, 'UTC'),
    source_kind   LowCardinality(String),
    source_vendor LowCardinality(String),
    severity      Enum8('trace'=1,'debug'=2,'info'=3,'notice'=4,'warn'=5,
                        'error'=6,'critical'=7,'alert'=8,'emergency'=9),
    facility      UInt8,
    body          String,
    attributes    Map(LowCardinality(String), String),
    trace_id      String,
    span_id       String,

    INDEX idx_body body TYPE text(tokenizer = 'splitByNonAlpha') GRANULARITY 1,
    INDEX idx_sev  severity TYPE set(16) GRANULARITY 4
)
ENGINE = MergeTree
PARTITION BY toYYYYMMDD(observed_at)
ORDER BY (tenant_id, resource_id, observed_at)
TTL toDateTime(observed_at) + INTERVAL 7   DAY TO VOLUME 'warm',
    toDateTime(observed_at) + INTERVAL 30  DAY TO VOLUME 'cold',
    toDateTime(observed_at) + INTERVAL 365 DAY DELETE
SETTINGS storage_policy = 'tiered', index_granularity = 8192;
```

**W1 CLOSED THIS ITEM — measured, not predicted.** See `bench/results/FINDINGS.md`.

The sort key is **confirmed and must not change**: Q05 ("all signals for one resource in a window")
ran at **9 ms reading 16 380 rows at both 10M and 100M** — completely independent of table size.

The open question was framed as "tenant-wide *text search* may be slow." That framing was wrong.
Text search is fine — a selective token read 8 190 rows at both scales. **It is time-ordered queries
that cannot prune**, and they need two separate fixes:

```sql
-- FIX 1 — the tail. Measured 32x: 2303ms/33.8M rows → 72ms/254K rows.
-- MUST NOT carry the text index: search resolves against the base table, and
-- projections do not support secondary indexes in any case.
ALTER TABLE logs ADD PROJECTION p_by_time (
    SELECT * ORDER BY (tenant_id, observed_at)
);
```

Cost: **~1.9x total storage** for the logs table. Note that time-ordering compresses ~58% worse than
resource-ordering (4.79 GiB vs 3.03 GiB for identical data), because sorting by resource groups rows
sharing `source_vendor`, `source_kind` and `host.name`. Budget for it.

```sql
-- FIX 2 — the Explorer histogram. The projection does NOT solve this: rows read
-- barely moved (33.47M → 33.39M) because a histogram over the full retention
-- window touches every row whatever the sort order. Only pre-aggregation helps.
-- This query re-renders on EVERY search and filter change.
CREATE TABLE logs_counts_5m (
    tenant_id UUID, resource_id UUID, severity LowCardinality(String),
    bucket DateTime('UTC'), cnt AggregateFunction(count, UInt64)
) ENGINE = AggregatingMergeTree
PARTITION BY toYYYYMM(bucket)
ORDER BY (tenant_id, bucket, severity, resource_id);

CREATE MATERIALIZED VIEW logs_counts_5m_mv TO logs_counts_5m AS
SELECT tenant_id, resource_id, severity,
       toStartOfFiveMinute(observed_at) AS bucket, countState() AS cnt
FROM logs GROUP BY tenant_id, resource_id, severity, bucket;
```

**Two fixes, two different root causes.** Neither substitutes for the other.

### Also measured in W1, and required

**Materialize the attributes that get grouped on.** The slowest query in the suite was not a text
search — it was `GROUP BY attributes['host.name']` at **2 252 ms** at 100M, worse than phrase
proximity. Map columns decompress in full per row.

```sql
host_name    String                  MATERIALIZED attributes['host.name'],
service_name LowCardinality(String)  MATERIALIZED attributes['service.name'],
```

**The text index must be opt-out per source.** It cost **71% of compressed data at 100M** (2.15 GiB
against 3.03 GiB) and the overhead *grew* with scale (67% → 71%) rather than amortising. Combined
with the projection, a naive build stores roughly 3x the raw compressed size — unacceptable on an
on-prem customer's finite disk array (PLAN §0b). Per-source opt-out belongs in the M2 monitoring
profile schema.

**Do not plan on index-accelerated substring or phrase search.** Measured at 100M: `LIKE` 1 928 ms
and phrase proximity 2 378 ms, both reading the entire tenant with no pruning. ClickHouse 26.8
exposes `use_text_index_like_evaluation_by_dictionary_scan`, which suggested `LIKE` might be
accelerated beyond what the March 2026 GA post describes; it was not. This confirms the boundary in
PLAN §3 and the `QueryWarning` requirement in §M0.5.

```sql
CREATE TABLE metrics
(
    tenant_id   UUID,
    resource_id UUID,
    site_id     UUID,
    metric      LowCardinality(String),
    observed_at DateTime64(3, 'UTC'),
    ingested_at DateTime64(3, 'UTC'),
    value       Float64,
    unit        LowCardinality(String),
    labels      Map(LowCardinality(String), String)
)
ENGINE = MergeTree
PARTITION BY toYYYYMM(observed_at)
ORDER BY (tenant_id, resource_id, metric, observed_at)
TTL toDateTime(observed_at) + INTERVAL 30 DAY DELETE
SETTINGS storage_policy = 'tiered';

-- rollups: raw 30d → 5m for 1y → 1h for 3y
CREATE TABLE metrics_5m
(
    tenant_id UUID, resource_id UUID, metric LowCardinality(String),
    bucket DateTime('UTC'),
    min_v AggregateFunction(min, Float64), max_v AggregateFunction(max, Float64),
    avg_v AggregateFunction(avg, Float64), cnt   AggregateFunction(count, UInt64)
)
ENGINE = AggregatingMergeTree
PARTITION BY toYYYYMM(bucket)
ORDER BY (tenant_id, resource_id, metric, bucket)
TTL bucket + INTERVAL 365 DAY DELETE;

CREATE MATERIALIZED VIEW metrics_5m_mv TO metrics_5m AS
SELECT tenant_id, resource_id, metric,
       toStartOfFiveMinute(observed_at) AS bucket,
       minState(value), maxState(value), avgState(value), countState()
FROM metrics GROUP BY tenant_id, resource_id, metric, bucket;
```

Query planning rule: the AST compiler picks raw / 5m / 1h based on `TimeRange` span vs. retention,
transparently. The caller never names a table.

`events` and `states` tables follow the same shape. `traces` and `flows` are declared in
`ch-migrations/` but not created until M7/M8.

## M0.7 Telemetry bus

PLAN §0 change #4: keep the boundary, defer the daemon.

```rust
#[async_trait]
pub trait TelemetryBus: Send + Sync {
    async fn publish(&self, subject: Subject, env: TelemetryEnvelope) -> Result<()>;
    async fn subscribe(&self, subject: Subject, group: Option<&str>)
        -> Result<BoxStream<'static, Delivery>>;
}

pub struct Delivery { pub envelope: TelemetryEnvelope, ack: AckHandle }
```

- **v0.1 `InProcessBus`** — bounded `tokio::sync::mpsc` per subject. `ack` is a no-op.
  Backpressure is the channel bound; a full channel blocks the collector, which is correct
  (better than dropping telemetry silently).
- **v0.2 `NatsBus`** — JetStream, same trait, real acks and redelivery.

Because `ack` exists in the trait from day one, the pipeline is written ack-aware and migrating to
NATS is a wiring change rather than a rewrite. Subjects are hierarchical from the start:
`telemetry.{tenant}.{signal}.{source_kind}`.

## M0.8 Security baseline

Security *engineering* from M0; security *analytics* stays at M11 (PLAN §0 change #6). Different
things.

| Control | Requirement |
|---|---|
| Passwords | argon2id, `m=19456,t=2,p=1` minimum |
| Sessions | Opaque token, httpOnly + Secure + SameSite=Lax cookie, server-side store, 12h idle / 7d absolute |
| CSRF | Double-submit token on all cookie-authenticated mutations |
| Tenant isolation | `TenantScope` required to construct any query. **Enforced by the type system, not by review** |
| Transport | TLS on the API; mTLS between collectors and gateway (M2 when collectors become remote) |
| Device credentials | §M0.4. SNMPv3 with authPriv preferred; v2c community strings marked `insecure` in the UI |
| API auth | Session cookie (UI) or bearer token (automation). Tokens are scoped, expiring, revocable |
| Rate limits | Per-IP on auth endpoints, per-tenant on query endpoints |
| Input validation | All API input through typed extractors. `limit` ceilings. Query depth cap on `Expr` (default 32) to stop pathological nesting |
| Audit (write) | Every mutating call → `audit_log` (actor, tenant, action, target, before/after, IP, at) |
| **Audit (read)** | Every *read* of a credential, a resource detail, or a telemetry query → `access_log` (actor, tenant, target, query fingerprint, row count, at). Defence and law-enforcement buyers audit who **saw** what, not only who changed it (PLAN §0b). Implemented once as Axum middleware over the query and resource routes — trivial now, invasive to retrofit. Sampled for high-volume dashboard polling; never sampled for credential reads |
| Egress | **None in the `onprem` profile.** No auto-update check, no crash reporting, no license callback, no telemetry. Enforced by a test that asserts no outbound connection is attempted during a full integration run |
| Supply chain | SBOM (CycloneDX) generated in CI and attached to every release; artifacts signed. Procurement asks for this |
| Secure defaults | No default credentials. First-run generates an admin password and prints it once. Telemetry ports bind localhost unless configured |
| Dependencies | `cargo-deny` + `cargo-audit` in CI, failing the build |

The `TenantScope` point deserves emphasis. Do not rely on "remember to add `WHERE tenant_id = $1`."
Make the repository layer take `&TenantScope` and make `TenantScope` only constructible from an
authenticated request context. Then a missing tenant filter is a compile error.

## M0 acceptance criteria

- [ ] W1 benchmark complete; results committed to `docs/benchmarks/w1.md` with a written go/no-go
- [ ] All PostgreSQL DDL applied via `sqlx migrate`; all ClickHouse DDL in `ch-migrations/` and applied
- [ ] `uops-core` compiles with the full envelope, resource, and identity types
- [ ] `uops-query` compiles a `Query` to ClickHouse SQL; golden test fixtures pass
- [ ] `uops-secrets` round-trips a credential; KEK rotation re-wraps without touching ciphertext;
      access log written; `Secret<T>` provably not serializable (a compile-fail test)
- [ ] Identity resolution rules implemented with unit tests covering: tier-1 match, noisy-OR
      combination, tier-1 contradiction, review threshold, cycle in relationship traversal
- [ ] `InProcessBus` passes the `TelemetryBus` conformance test suite (written once, run against
      both impls)

---

# M1 — Core platform

## Scope

PostgreSQL + ClickHouse wired, Axum API, React shell, authentication, RBAC, resource inventory,
identity resolution service running.

## API surface

```
POST   /api/v1/auth/login                 POST   /api/v1/auth/logout
GET    /api/v1/me

GET    /api/v1/organizations              GET    /api/v1/tenants
GET    /api/v1/sites                      POST   /api/v1/sites

GET    /api/v1/resources                  ?kind=&site=&status=&q=&cursor=
POST   /api/v1/resources                  (manual creation)
GET    /api/v1/resources/{id}
PATCH  /api/v1/resources/{id}
DELETE /api/v1/resources/{id}             (soft → 'decommissioned')
GET    /api/v1/resources/{id}/identifiers
GET    /api/v1/resources/{id}/relationships
POST   /api/v1/resources/{id}/merge
POST   /api/v1/resources/{id}/split

GET    /api/v1/identity/review
POST   /api/v1/identity/review/{id}

POST   /api/v1/credentials                (write-only; returns a ref)
GET    /api/v1/credentials                (metadata only — never material)
DELETE /api/v1/credentials/{id}

POST   /api/v1/query                      (the Query AST, §M0.5)
GET    /api/v1/audit                      ?actor=&action=&from=&to=
```

Pagination is cursor-based (`(observed_at, id)` keyset), never `OFFSET`. Offset pagination on a
resource list of 50 000 devices degrades, and on telemetry it is unusable.

## RBAC

Three roles in v0.1. Resist adding more until a customer asks.

| | viewer | operator | admin |
|---|---|---|---|
| Read resources / telemetry / dashboards | ✅ | ✅ | ✅ |
| Acknowledge alerts, resolve identity review | — | ✅ | ✅ |
| Create/edit resources, profiles, alert rules | — | ✅ | ✅ |
| Manage credentials | — | — | ✅ |
| Manage users, roles, tenants | — | — | ✅ |
| Read audit log | — | — | ✅ |

Role is per `(user, tenant)`, so one MSP operator can be admin on one tenant and viewer on another.

## Frontend shell

React + Vite + TanStack Query + TanStack Router. Dark and light from the start (NOC screens are dark;
management screenshots are light). One layout: left nav, content, a global time-range picker that is
**shared state across every view** — the single most-used control in the product.

## M1 acceptance criteria

- [ ] `docker compose up` gives a working stack from a clean checkout on a clean machine
- [ ] First-run generates an admin credential, printed once, not stored in plaintext
- [ ] A user in tenant A cannot read *anything* from tenant B — verified by an integration test that
      attempts it on every endpoint, not by inspection
- [ ] Resource CRUD + list with cursor pagination over 10 000 seeded resources, p95 < 200 ms
- [ ] Identity resolution resolves two sources to one resource; the review queue receives a
      0.60–0.95 case; a merge is performed and then reverted by a split
- [ ] Audit log contains an entry for every mutating call made during the integration suite

---

# M2 — NMS

## Scope

ICMP, TCP, SNMP v2c/v3 polling, OID metrics, **monitoring profiles**, interface discovery.

## Monitoring profiles (PLAN §0 change #5)

The alternative is `if vendor == "Cisco"` spreading through the poller until it is unmaintainable.
Profiles are declarative, versioned, shipped as built-ins in `profiles/`, and user-overridable.

```yaml
id: mikrotik-routeros
version: 1
name: MikroTik RouterOS
match:
  sysobjectid_prefix: "1.3.6.1.4.1.14988"

discovery:
  - kind: interface
    walk: "1.3.6.1.2.1.2.2.1"          # IF-MIB::ifTable
    key: ifIndex
    creates:
      resource_kind: interface
      name_from: "1.3.6.1.2.1.31.1.1.1.1"    # ifName
      relationship: member_of                 # → parent device. Exercises M0.1 edges
      identifiers:
        - kind: mac
          oid: "1.3.6.1.2.1.2.2.1.6"          # ifPhysAddress

metrics:
  - name: system.cpu.utilization             # OTel semconv naming
    oid: "1.3.6.1.2.1.25.3.3.1.2"
    kind: gauge
    unit: "%"
    interval: 60s
  - name: network.io.receive
    oid: "1.3.6.1.2.1.31.1.1.1.6"            # ifHCInOctets
    kind: counter
    unit: By
    scope: interface
    interval: 60s

availability:
  - kind: icmp
    interval: 30s
    timeout: 2s
    retries: 3
```

Profile resolution order: explicit `resource.profile_id` → `sysObjectID` match → generic fallback
(`SNMPv2-MIB` system group + `IF-MIB` only). The generic fallback matters: an unknown vendor should
still get interfaces and availability, not nothing.

```sql
CREATE TABLE monitoring_profile (
  id          uuid PRIMARY KEY,
  tenant_id   uuid REFERENCES tenant(id),    -- NULL = built-in, visible to all tenants
  profile_key text NOT NULL,                 -- 'mikrotik-routeros'
  version     int  NOT NULL,
  definition  jsonb NOT NULL,
  enabled     bool NOT NULL DEFAULT true,
  UNIQUE (tenant_id, profile_key, version)
);
```

Built-ins for v0.1 — resist adding more: **generic-snmp**, **linux-snmp**, **mikrotik-routeros**,
**cisco-ios**, **windows-snmp**. Five is enough to prove the model; a profile library is a
community contribution surface later, not a solo-developer task.

## Poller design

The part that determines whether this scales, and the part most likely to be got wrong:

- **Scheduler is a time wheel, not a `tokio::spawn` per device per interval.** 10 000 devices × 20
  metrics × 60s means 200 k tasks/minute; spawning per-poll will thrash.
- **Per-device concurrency cap and a global semaphore.** Devices have small SNMP agent queues and
  will drop requests before they refuse them — a flooded switch silently returns nothing, which
  looks like an outage.
- **Jitter every schedule** by ±10% of interval. Without it, everything polls at `:00` and the box
  has a 1-second CPU spike per minute.
- **GETBULK with `max-repetitions` tuning**, and handle `tooBig` by halving and retrying
  (`async-snmp` does this automatically; `snmp2` needs it written).
- **Counter wrap handling is mandatory.** 32-bit `ifInOctets` wraps in ~34 seconds on a 1 Gbps link.
  Prefer 64-bit `ifHC*` counters; when only 32-bit exists, detect wrap by
  `current < previous && delta_t < interval * 2`, and discard rather than emit a negative rate.
  Store the raw counter; compute rates at query time from the raw series. Storing pre-computed
  rates makes re-interpretation impossible.
- **Timeout budget per device** so one dead device cannot delay the wheel.

## M2 acceptance criteria

- [ ] 1 000 simulated SNMP agents polled at 60s with p95 poll latency < 5 s and no missed cycles
- [ ] SNMPv3 authPriv (SHA-256 / AES-256) against a real device; credential fetched through
      `SecretStore` with an access-log entry
- [ ] Interface discovery creates child resources **and** `member_of` relationships
- [ ] A 32-bit counter wrap produces no negative rate in any query
- [ ] An unknown-vendor device gets interfaces and availability via `generic-snmp`
- [ ] Dead device does not delay polling of healthy devices (measured, not assumed)

---

# M3 — Logs

## Scope

Syslog UDP/TCP/TLS, OTel Collector distribution, OTLP receiver, the normalization pipeline,
ClickHouse text search, Log Explorer with saved searches.

## Syslog receiver

Support both wire formats — real networks emit both, often from the same vendor:

- **RFC 5424** (structured, preferred) and **RFC 3164** (BSD, legacy, ambiguous).
- **TCP framing:** octet-counting (RFC 6587 §3.4.1) *and* non-transparent framing (LF-delimited).
  Detect per-connection on the first byte: a digit means octet-counting.
- **UDP:** single datagram per message, 64 KB buffer, `SO_RCVBUF` raised explicitly. Track and
  expose drop counts — a silently dropping syslog receiver is worse than none.
- **TLS:** rustls, optional client certificates.

Parse failures are **never dropped**. An unparseable message is stored with
`severity=unknown`, the raw bytes as `body`, and `attributes['parse.error']` set. Dropping malformed
input loses exactly the messages that matter during an incident.

## Pipeline

```
receiver → decode → TelemetryEnvelope (resource_id: None)
                        ↓
                  identity resolution   (cached, §M0.2)
                        ↓
                    normalize           (severity mapping, semconv keys)
                        ↓
                     enrich             (site, profile, vendor; GeoIP at M7)
                        ↓
                  batch accumulator     (N rows or T ms, whichever first)
                        ↓
                     ClickHouse
```

Batching parameters that matter: ClickHouse wants **large, infrequent inserts** — target 10 000–100 000
rows or 1 second, whichever comes first. Per-row inserts will destroy it. On insert failure, retry
with backoff and spill to a local WAL after N failures; never drop in-memory batches on a transient
ClickHouse restart.

## OTLP receiver

`opentelemetry-proto` with `gen-tonic` gives the gRPC server stubs. Implement
`LogsService`, `MetricsService`, `TraceService` (trace accepts and stores nothing until M8 — accept
and drop with a counter, so instrumented apps don't error).

Resource attributes (`host.name`, `host.id`, `service.name`) map directly onto
`ObservedIdentity`, which is the whole reason for choosing semconv naming in M0.3.

## OTel Collector distribution

Not an agent. A Collector build with a preset config:

```yaml
receivers:  [hostmetrics, filelog, journald, windowseventlog, syslog]
processors: [resourcedetection, batch, memory_limiter]
exporters:  [otlp]     # → uops gateway
```

Ship it as a binary + config template + install script per platform. The differentiator is the
preset and the enrollment, not the code.

## Log Explorer

```
┌──────────────────────────────────────────────────────────────┐
│ [resource ▾] [severity ▾]  body contains: timeout            │
│                                        [Last 15m ▾] [Search] │
├──────────────────────────────────────────────────────────────┤
│ ▁▂▅█▅▂▁  histogram — click-drag to zoom the time range       │
├───────────────┬──────────────────────────────────────────────┤
│ Fields        │ 14:32:01  ERROR  rtr-01  Database timeout    │
│ ─────────     │ 14:32:01  WARN   rtr-01  Pool exhausted      │
│ resource  12  │ 14:32:02  ERROR  api-03  HTTP 500            │
│ severity   5  │ ← click a row → full document + "show all    │
│ source     3  │    signals for this resource at this time"   │
│ vendor     4  │                                              │
└───────────────┴──────────────────────────────────────────────┘
```

Required in v0.1: field sidebar with value counts, histogram with drag-to-zoom, live tail, saved
searches, and **"show all signals for this resource around this timestamp"** — the seed of the
Investigation Workspace, and the one interaction that demonstrates the product thesis in ten seconds.

Saved searches are stored `Query` ASTs. They become alert rules in M4 with no translation.

## M3 acceptance criteria

- [ ] 50 000 msg/s sustained syslog ingest on a single node, with drop counter at zero
- [ ] RFC 3164 and RFC 5424, over UDP, TCP octet-counted, TCP LF-delimited, and TLS
- [ ] Malformed messages stored, not dropped, with `parse.error` set
- [ ] Identity cache hit rate > 99% at steady state
- [ ] ClickHouse restart mid-ingest loses zero messages (WAL spill verified)
- [ ] OTel Collector on Linux and Windows sends host metrics + logs end to end
- [ ] Log Explorer: token search over 100M rows returns p95 < 2 s
- [ ] "Show all signals for this resource" works across logs and metrics

---

# M4 — Metrics, dashboards, alerting

## Alert engine

Two rule types in v0.1. Rate, change, anomaly, correlation and topology suppression are M9.

**Threshold** — `avg(system.cpu.utilization) > 90 for 5m`
**Absence** — no telemetry from resource R for 5m (covers device-down without a separate mechanism)

```sql
CREATE TABLE alert_rule (
  id             uuid PRIMARY KEY,
  tenant_id      uuid NOT NULL REFERENCES tenant(id),
  name           text NOT NULL,
  kind           text NOT NULL,            -- 'threshold' | 'absence'
  query          jsonb NOT NULL,           -- a Query AST (§M0.5)
  condition      jsonb NOT NULL,           -- { op: '>', value: 90, for: '5m' }
  severity       text NOT NULL,
  enabled        bool NOT NULL DEFAULT true,
  eval_interval  interval NOT NULL DEFAULT '60 seconds',
  notify         jsonb NOT NULL,           -- channel refs
  created_by     uuid NOT NULL,
  UNIQUE (tenant_id, name)
);

CREATE TABLE alert_state (
  id           uuid PRIMARY KEY,
  tenant_id    uuid NOT NULL,
  rule_id      uuid NOT NULL REFERENCES alert_rule(id) ON DELETE CASCADE,
  resource_id  uuid NOT NULL,
  dedup_key    text NOT NULL,              -- rule_id + resource_id + label fingerprint
  state        text NOT NULL,              -- 'ok'|'pending'|'firing'|'resolved'
  since        timestamptz NOT NULL,
  last_eval    timestamptz NOT NULL,
  last_value   double precision,
  acked_by     uuid,
  acked_at     timestamptz,
  UNIQUE (tenant_id, dedup_key)
);
```

State machine: `ok → pending` (condition true, `for` window not elapsed) `→ firing` (window elapsed,
notify) `→ resolved` (condition false, notify once). **`pending` is what stops flapping from
generating notification storms**, and it is the single most important piece of the engine.

Evaluation: one tokio task per rule, jittered. A rule is a saved `Query` plus a condition — so
"alert from a saved search" needs no new machinery, which is exactly why the AST was built first.

**Notification** in v0.1: email (SMTP) and webhook. Per-channel rate limiting and a per-tenant
notification budget, because the first misconfigured rule *will* try to send 10 000 emails.

## Dashboards

Panels are `{ query: Query, viz: VizSpec }`. Layout is a grid. Time range comes from the global
picker unless a panel overrides it.

v0.1 panel types: time series, single stat, table, gauge, alert list. Not: heatmap, geo map,
topology, pie. Five types built well beats twelve built badly, and geo/topology depend on M6/M7.

## M4 acceptance criteria

- [ ] Threshold rule fires, notifies once, resolves, notifies once — verified against a flapping
      signal that would produce ≥20 notifications without `pending`
- [ ] Absence rule detects a device stopping telemetry within `interval + eval_interval`
- [ ] 1 000 rules evaluate within one 60 s cycle
- [ ] Notification rate limit prevents a storm from a rule matching 5 000 resources
- [ ] A saved search from the Log Explorer converts to an alert rule with no edits
- [ ] Dashboard with 20 panels loads p95 < 3 s over 30 days of data (rollups exercised)

---

# Cross-milestone: what must not be deferred

Re-stated because these are the decisions that are cheap now and structurally impossible later:

1. `tenant_id` on every row, `TenantScope` required to build any query
2. `resource_id` on every telemetry row, with the ClickHouse sort key `(tenant_id, resource_id, observed_at)`
3. Both `observed_at` and `ingested_at`
4. The telemetry envelope, before the first collector
5. The Query AST, before the first UI view
6. Secrets through `SecretStore`, never a plaintext column
7. Relationship edges, even with no topology UI
8. Audit log on every mutation
9. `Organization → Tenant → Site → Resource`, even for a single-company deployment

## Deliberately deferred

NetFlow/IPFIX/sFlow · topology UI · traces/APM · correlation engine · incidents · Investigation
Workspace UI · automation/runbooks · config backup · security analytics · AI · MSP billing and
white-label · SSO/SAML/OIDC · HA · distributed collectors · own agent · text query language · GeoIP

---

## Open items blocking work

| Item | Blocks | Decide by |
|---|---|---|
| **[OPEN]** Alias chain: collapse-on-write vs transitive read (recommend collapse-on-write) | M0.2 | W2 |
| ~~Tenant-wide log search performance~~ **CLOSED by W1** → `p_by_time` projection + `logs_counts_5m` both required; ~1.9x storage | M0.6 | done |
| **[OPEN]** License: AGPL / Apache 2.0 / BSL | first public commit | before M1 |
| **[OPEN]** Target buyer: MSP-first vs self-hoster-first | credential scoping and isolation depth in M1 | before M1 |
| **[OPEN]** Product name | crate naming, GitHub org | before public release |
