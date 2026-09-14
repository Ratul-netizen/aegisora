# PostgreSQL migrations — the control plane

SPEC §M0.1 (resource model), §M0.2 (identity), §M0.4 (secrets). Telemetry does not live
here; it lives in ClickHouse and arrives with the migration runner.

| file | what |
|---|---|
| `0001_foundation.sql` | organization → tenant → site, and the `updated_at` trigger function |
| `0002_resource.sql` | the resource model — the centre of the data model |
| `0003_relationships.sql` | relationship edges and `resource_dependents()` |
| `0004_identity.sql` | identifiers, decisions, and aliases that collapse on write |
| `0005_credentials.sql` | sealed credentials and the access log |

```bash
docker compose -f deploy/docker-compose.yml up -d
bash scripts/db.sh migrate    # sqlx-cli if present, psql otherwise
bash scripts/db.sh test       # assert the invariants
```

## Two rules that shaped every table

**Every table below the isolation boundary carries `tenant_id`.** `organization` is the
only one without, and it sits *above* the boundary rather than being an exception to it.

**Intra-tenant references are composite.** The obvious spelling —

```sql
parent_id uuid REFERENCES resource (id)
```

— lets a resource in one tenant be the child of a resource in another. No application
code intends that, which is exactly why nobody notices when it happens. So every such
reference carries `tenant_id` and targets a `UNIQUE (id, tenant_id)`:

```sql
FOREIGN KEY (parent_id, tenant_id) REFERENCES resource (id, tenant_id)
```

This is the SQL counterpart of `TenantScope` in `uops-core`. The same invariant, held at
the other end of the wire: a missing tenant filter is a compile error in Rust and a
constraint violation in PostgreSQL.

## Three deliberate departures from the SPEC DDL

**1. `resource_dependents()` takes the tenant.** The sketch in SPEC §M0.1 took
`(root, max_depth)`. Knowing a UUID is not authorization — without the filter, a caller
who learns one resource ID from another tenant can walk that tenant's graph.
`ResourceCatalog::descendants` in `uops-query` already takes the tenant, so this
signature is what the Rust side was written against. The function also returns the root
at depth 0, because a blast radius that omits the thing that broke makes every caller
remember to add it back.

**2. The `credential.aad` column is not here.** The AAD is
`tenant_id || credential_id || version`, and `uops-secrets` *derives* it from the row's
own identity at decrypt time. Storing it would turn a binding into an input: an attacker
with database write access could transplant a ciphertext into another tenant and store
the matching AAD beside it, and decryption would succeed. Deriving it means that same
attacker gets an authentication failure. `uops-secrets` has a test for exactly this
attack; a stored `aad` column would make the test pass and the property false.

**3. `dek_nonce` and `backend_id` are added.** Both are in `SealedCredential` and
neither was in the sketch. `backend_id` is required by the M0.4 obligations — a re-key
after a crypto-backend change has to be detectable, and an auditor needs to see what
produced the bytes rather than what the deployment claims to be running.

## The alias chain question is now decided

SPEC §M0.2 left it open: A merged into B merged into C, resolve transitively at read or
collapse on write? **Collapse on write**, as recommended there, implemented as a trigger
in `0004`.

Merges are rare; reads are constant. Transitive resolution would put a recursive lookup
on the hot path of every telemetry query. The trigger buys one flat invariant —
*no `historical_id` is ever also a `current_id`* — which is what lets alias expansion in
the query layer be a single lookup. It lives in the database rather than in application
code because that invariant is only true if every writer collapses, including a DBA
fixing something by hand at 3am.

## Tests

`tests/invariants.sql` asserts the properties the schema exists for, not that the tables
exist: cross-tenant parents, sites, edges and credential references are all refused;
the resolution index rejects a duplicate hostname within a tenant but allows the same
hostname in two tenants; graph traversal terminates on a cycle, honours `max_depth`, and
returns nothing for another tenant's root; alias chains collapse; access-log rows
survive deletion of the credential they describe.

It runs in one transaction and rolls back, so it is safe against a development database.

CI runs it, and then runs it a second time with `resource_parent_id_tenant_id_fkey`
dropped — the suite is required to *fail* that run. A test suite that passes whether or
not the property holds is worse than no suite, because it reads as covered. That trap
has already been hit once in this repo, with the `compile_fail` doctests in
`uops-core`.

## What is not here, and why

**Row-level security.** Tenant isolation is currently enforced by the type system
(`TenantScope`), by these composite foreign keys, and by `sqlx` compile-time-checked
queries. RLS would add a fourth layer, and it is worth having — but it needs an
application role and a per-transaction `SET LOCAL`, which means it is a decision about
connection pooling and the request lifecycle. That belongs in M1 with the API, not in a
DDL file. Tracked as an open item in STATUS.md.
