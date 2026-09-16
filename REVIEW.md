# Architecture review gate — 2026-09-16

Requested as a gate before M1 is frozen: does the architecture support the roadmap, what
must change now, what can wait, what is still blocking, and are SPEC and PLAN consistent.

**No code was changed to produce this document.** Where it says something is implemented,
that was verified against the tree, not against SPEC.

---

## 0. What is actually implemented

This was the one thing the review request could not determine from SPEC and PLAN alone,
so it goes first. Verified against the repository, not the documents.

| | State |
|---|---|
| W1 storage benchmark | ✅ executed; `docs/benchmarks/w1.md` |
| M0 primitives | ✅ envelope, identity rules, Query AST, secrets, migrations, bus |
| M1 core platform | ✅ API, auth, inventory, telemetry, web shell, 10 000-resource p95 75 ms |
| M2 NMS | ✅ all six acceptance criteria met **and measured**, each with a CI mutation guard |
| M5 device identity, M6 map, OUI | ✅ taken out of order because they were asked for |
| M3 logs | 🟡 both wire formats, RFC 6587 framing, UDP + TCP receivers, normalization, the shared pipeline and batching. The daemon, OTLP and the Log Explorer are not written |
| M4 dashboards and alerting | ⬜ |

704 tests on Windows, against real PostgreSQL, real ClickHouse and a real net-snmp agent.
No mocked infrastructure in any integration suite. 16 crates, all named `uops-*`.

So: the architecture is not a document. M0–M2 are built and measured, and M3 is half
built.

---

## 1. Does the current architecture support the complete roadmap?

**Yes, with two exceptions, both in this document.**

The parts that carry the roadmap are the parts that were built first, which was the right
order:

- **`resource_id` as the join key for every signal.** Logs, metrics, events, states,
  traces and flows all sort by `(tenant_id, resource_id, observed_at)` in ClickHouse, so
  "everything about resource R in window W" is one contiguous range read per table rather
  than six different access patterns. The Investigation Workspace is reachable because of
  that decision and would not be otherwise.
- **The Query AST.** The compiler injects tenant scope; no caller can forget it. Adding a
  text query language later means adding a parser, not a second engine — and a secondary
  search index, if the log workload ever needs one, is a new backend behind the same AST.
- **`resource_alias`.** A merge is O(1) and history is never rewritten. Without it,
  identity corrections would mean re-ingesting ClickHouse.
- **`TelemetryEnvelope` on OTel semantic conventions.** `host.name`, `service.name`,
  `source.address` rather than `src_ip`. This is what makes the OTLP receiver a new
  input rather than a new data model — and it is already proven by `normalize`, which maps
  syslog onto the same keys the metrics path writes.
- **Declarative monitoring profiles.** No `if vendor == "Cisco"` anywhere.
- **`SecretStore` / `Secret<T>`, envelope encryption, no reveal endpoint.** CI greps for
  `.expose()` inside a logging macro and for crypto primitives outside `uops-secrets`.

The two gaps are **resource groups** and **operator tags**, below. Both are cheap now and
expensive after M4, because alerting, dashboards, notification routing and maintenance
windows all want to select a set of resources that is neither a site nor a topology
subtree.

---

## 2. What must change before M1 is frozen?

M1 is already met, so read this as "before M4 builds on it".

### FOUNDATIONAL — do these now

#### 2.1 Resource groups

An operator-defined set: *Dhaka Core Routers*, *Branch Routers*, *Critical Servers*,
*Internet Edge*, *Production*. Neither a site (geography) nor a topology subtree
(physical), and every one of alerting, dashboards, notification routing, maintenance
windows and RBAC wants one.

```
resource_group        (tenant_id, id, name, description)
resource_group_member (tenant_id, group_id, resource_id)
```

with `ResourceSelector::Group(id)` in the Query AST alongside the existing selectors.

Doing this after M4 means every alert rule, dashboard and notification policy written
against the old selector has to be migrated. Doing it now is two tables and one AST
variant.

#### 2.2 Operator tags, separate from system attributes

`resource.attributes` is currently one `jsonb` column holding both what a collector
discovered and what a human decided. It has a GIN index and works, but the two are not
the same thing and conflating them has a specific cost: **discovery overwrites**. A
profile that writes `attributes` on every walk will eventually clobber
`criticality=critical` that somebody set by hand, and nothing will report it.

The precedent is already in the schema and was already right: `resource.name` is
system-chosen and `resource.display_name` is the human override, *"never written
automatically — if a human named it, discovery must not silently rename it underneath
them."* Tags are the same argument applied to attributes.

```
resource.attributes   -- system / OTel semconv. Collectors write. Humans do not.
resource.tags         -- operator-managed. Humans write. Collectors never do.
```

Tags must be queryable and usable by alerts, dashboards, maintenance windows,
notification routing, automation, and RBAC where appropriate.

### M1–M4 — design the model now, implement inside the v0.1 window

#### 2.3 Maintenance windows

`resource_status` already has a `maintenance` value and nothing populates it. That is the
gap: when somebody reboots 500 switches on a Saturday night, the alert engine has no way
to be told.

The model, which should exist before the alert engine is written, because the alert
engine has to consult it:

```
maintenance_window
├── target     resource | group | site
├── start, end
├── timezone
├── recurrence
├── reason
├── suppress_alerts
└── suppress_notifications
```

Not a general scheduling system. The model and the alert-engine boundary; the recurrence
evaluator can be crude in v0.1.

Note the dependency: **targeting a group needs 2.1.** Another reason groups come first.

#### 2.4 Event / state / alert / incident semantics, written down

The tables exist — `events` and `states` are both in ClickHouse with the same sort key —
but the distinction is not documented anywhere, and the correlation engine in M9 will be
built by somebody reading whatever they assume. Put this in SPEC before M4:

| | |
|---|---|
| **EVENT** | Something happened. *BGP neighbour changed state* |
| **STATE** | Something is currently true. *BGP neighbour = DOWN* |
| **ALERT** | A rule decided a condition is worth notifying about. *BGP neighbour down > 5 min* |
| **INCIDENT** | Correlated alerts and events describing one operational problem. *WAN connectivity degraded* |

#### 2.5 Notification abstraction

v0.1 implements SMTP and webhook and nothing else. But the *shape* must be right now,
because retrofitting routing onto a direct alert→email call is a rewrite:

```
NotificationChannel · NotificationPolicy · Routing · Deduplication
RateLimit · QuietPeriod · MaintenanceSuppression
```

Two implementations behind a correct abstraction. Not fifteen integrations.

#### 2.6 Backup and restore

PLAN already moves this into the M1–M4 window; the review asks for acceptance criteria,
and it is right to. **An NMS without a tested restore is a liability**, and this one
holds envelope-encrypted credentials — a restore that recovers the database and not the
KEK recovers nothing, and finding that out during an incident is the worst possible time.

Acceptance criteria, to be added to SPEC §M4:

- [ ] PostgreSQL backup and restore, documented and executed in CI
- [ ] ClickHouse backup and restore
- [ ] Configuration backup
- [ ] **Secret metadata and KEK backup, stated separately**, with the explicit warning that
      `docker compose down -v` destroys the KEK and with it every stored credential
- [ ] A restore test that starts from empty and ends with a working install
- [ ] Version-compatibility rules: which backup versions restore into which binary

### M5+ — correctly deferred

Everything in the Motadata feature inventory that is not in M0–M4. VLAN and routing
monitoring, NetFlow/IPFIX/sFlow, config backup and compliance, runbooks, security
analytics, RUM, cloud and Kubernetes collectors, AI. PLAN's M5+ is *direction*, not
commitment, and that distinction is the single most valuable piece of scope control in
the document. Keep it.

### NOT NEEDED

- **A second search index.** The escape hatch behind the Query AST is the right answer.
  Building it now would be paying for a problem the benchmark has not demonstrated.
- **Two codebases for on-prem and hosted.** One core, two deployment profiles. The
  review flagged the risk and it is worth restating: the moment there is a *Veyronis
  Enterprise* and a *Veyronis Cloud* with separate trees, every feature costs twice.
- **Renaming crates to `veyronis-*` before clearance.** Settled; see `RENAME_AUDIT.md`.

---

## 3. What can safely wait until M5+?

All of §M5+ above, plus three things that look foundational and are not:

- **Row-level security.** A fourth isolation layer behind `TenantScope`, composite foreign
  keys and sqlx. Worth having; needs an app role and a per-transaction `SET LOCAL`, which
  is a connection-pooling decision. It does not change the schema, so it can land later.
- **Tiered storage.** SPEC §M0.6 shows `TTL … TO VOLUME 'warm'` against a policy that does
  not exist on a default install — those migrations would fail outright. Retention is a
  plain `DELETE` TTL now; tiering is a later migration written alongside the deployment
  profile that configures the policy.
- **NATS.** The in-process `TelemetryBus` is correct for one process. The conformance
  suite already exists so the second implementation has something to pass.

---

## 4. What decisions are still genuinely blocking?

| Decision | Blocks | Status |
|---|---|---|
| ~~Licence~~ | — | **Closed 2026-09-16: AGPL-3.0-only plus a CLA, enabling commercial dual-licensing.** This review raised it because the recommendation was being enacted by default; it is now a decision. What remains is a *lawyer's review of `CLA.md`* before the repository is publicised — see the row below. The licence choice is reversible; accepting one unsigned contribution is not |
| **The review queue has no way to say "no"** | the review-queue UI | A case leaves the queue only by being merged. An operator who decides two resources are genuinely different has no action; the question returns every day. Needs a dismissal outcome, a store method and a route — and a product decision about what "not the same" means for a provisional that already has telemetry attached |
| **Tenant attribution for syslog** | the syslog daemon | A syslog message carries no tenant. Recommendation: a listener per tenant, address or port identifying it, with `create_provisional` for unknown senders. The alternative is an explicit sender→tenant allow-list that refuses unknown senders — a different security posture, so it is a decision rather than a default |
| **Brand clearance for Veyronis** | crate publishing, not development | GitHub org, crates.io, npm, `.com`/`.io`, USPTO TESS classes 9 and 42, Bangladesh RJSC. Until then the code stays `uops-*` |
| **CLA reviewed by a lawyer** | accepting outside contributions | The only other irreversible item |

---

## 5. Are there contradictions between SPEC and PLAN?

Four, none fatal, all worth fixing.

1. **SPEC §M0.6 shows tiered-storage TTLs that cannot be applied** to a default install.
   PLAN does not mention tiering at all. SPEC should mark those as illustrative.
2. **SPEC asks for syslog over TLS**, which is now decided against — terminated at a
   proxy, for the licence reason that has already shaped every other client here. SPEC
   should carry the decision rather than the requirement.
3. **Migration 0005 promises credential rollback** — *"rotation writes a new row rather
   than overwriting one … a rotation that turns out to be wrong is undone by revoking a
   row"* — and neither implementation does it. `LocalVault::put` reuses the credential's
   id, so both the PostgreSQL and in-memory stores *replace* the previous version.
   Either drop the claim or key on `(id, version)`. Currently the schema comment is a
   promise the code does not keep, which is worse than either.
4. **PLAN positions the product against Kibana more strongly than the benchmark
   supports.** The W1 work is honest about it — relevance ranking, fuzzy matching and
   phrase proximity are a different problem, and the text index costs real storage. The
   positioning should be *"unified observability platform with an operations-focused log
   explorer"*, not *"Elasticsearch replacement"*. The architecture already leaves the
   escape hatch; the marketing language should match the architecture.

---

## 6. Is anything over-engineered?

Very little, which is unusual. Two candidates, and I would keep both:

- **The identity confidence model** looks heavy for v0.1 — noisy-OR, tier-1
  contradictions, three outcome bands, an evidence trail. It is not. It found a real
  defect in this very review cycle: the model was being asked the wrong question in the
  steady state, and a device would have acquired a duplicate resource by its second
  message. That is only findable because the rules are explicit and separately tested.
- **The mutation-guard CI pattern** — break something deliberately, require the suite to
  fail — roughly doubles the work per acceptance criterion. It has caught enough
  ineffective tests to pay for itself. Keep it.

One thing that is genuinely too much for v0.1 and is already scoped out: the FIPS crypto
build matrix. It is one CI job and a feature flag, so it costs almost nothing, but it
should not grow.

---

## 7. Missing foundational abstractions that become expensive later

Ranked by what they cost if deferred:

1. **Resource groups** — §2.1. Every M4 feature selects a set of resources.
2. **Operator tags distinct from system attributes** — §2.2. Discovery will silently
   overwrite human intent, and the resulting bug reports will be unreproducible.
3. **The maintenance-window model** — §2.3. The alert engine has to consult it, so it must
   exist before the alert engine does.
4. **The notification policy shape** — §2.5. Retrofitting routing onto a direct
   alert→email call is a rewrite.
5. **Event/state/alert/incident semantics, written down** — §2.4. The correlation engine
   will otherwise be built on somebody's assumption.

---

## Branding rule — to be added to the top of SPEC

> **Veyronis** is a provisional product brand. **`uops`** is the implementation codename.
>
> No product-facing identifier may be used as a persistence, crate, database,
> environment-variable, NATS subject, Docker image or encryption identifier until brand
> clearance is complete.
>
> Product-facing surfaces — README, documentation, the UI, marketing — say *Veyronis*.
> Everything in the tree says `uops`.

This is what stops a future session coupling the brand to the architecture by accident.
The cost of having done it correctly so far was measured this week: renaming *Aegisora* to
*Veyronis* touched eight lines.
