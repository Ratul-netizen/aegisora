# ClickHouse migrations — telemetry

SPEC §M0.6, amended by W1. Applied by [`uops-ch-migrate`](../crates/uops-ch-migrate).

| file | what | why it is its own file |
|---|---|---|
| `0001_logs.sql` | the logs table, text index, materialised semconv columns | |
| `0002_logs_by_time_projection.sql` | `p_by_time` — W1 FIX 1, the tail | the expensive one: ~1.9x storage, and a data rewrite on a populated table |
| `0003_logs_counts_5m.sql` | pre-aggregate — W1 FIX 2, the Explorer histogram | a different root cause from 0002; neither fix substitutes for the other |
| `0004_metrics.sql` | metrics + the 5-minute rollup | |
| `0005_metrics_1h.sql` | the hourly rollup | closes the open item `uops-query` left: it already plans onto this table |
| `0006_events_states.sql` | events and state transitions | |
| `deferred/` | traces and flows | declared, created in M7/M8. The runner ignores this directory |

```bash
docker compose -f deploy/docker-compose.yml up -d
bash scripts/ch.sh apply      # or: cargo run -p uops-ch-migrate -- apply
bash scripts/ch.sh verify     # uops-query's golden SQL against the live schema
bash scripts/ch.sh smoke      # insert rows, assert the views behave
```

## The sort key is the product

`ORDER BY (tenant_id, resource_id, observed_at)` on every telemetry table. W1 measured
Q05 — "all signals for one resource in a window" — at **9 ms reading 16 380 rows at both
10M and 100M rows**: resource-scoped investigation is independent of table size. That
number is the Investigation Workspace, and changing this key is a full re-ingest rather
than a migration.

## Rules for writing one

**Every statement must be individually idempotent.** `CREATE TABLE IF NOT EXISTS`,
`ADD PROJECTION IF NOT EXISTS`, and so on. ClickHouse has no transactional DDL, so a
migration that fails at its third statement has applied two — and the runner's resume
re-runs the statement it was in the middle of.

**Never edit an applied migration.** The runner checksums every file and refuses to run
when one it has already applied has changed:

```
error: migration 3 (0003_logs_counts_5m) was edited after it was applied
       (recorded 3ad44b1a0c53, on disk ae653413a9f0); write a new migration instead
```

That includes retention: changing a TTL is `ALTER TABLE … MODIFY TTL` in a new file,
not a smaller number in an old one.

**Never number below what is already released.** Two branches each adding a migration
produce an order nobody tested; the runner refuses that too, and the fix is a rename.

**Do not name a database.** Every statement is unqualified and the runner supplies the
database, so an on-premise deployment that uses a different name works untouched.

## What the SPEC DDL says and this does not

**No tiered storage.** SPEC §M0.6 shows `TTL … TO VOLUME 'warm' / 'cold'` against a
`tiered` storage policy. That policy does not exist on a default install, so those
migrations would fail outright on a deployment that has not configured one. Retention
here is a plain `DELETE` TTL; tiering arrives with the deployment profiles that
configure the policy, as a later migration. Tracked in STATUS.md.

## Two agreements that nothing else checks

**`uops-query` and this schema must name the same columns and functions.**
`scripts/ch.sh verify` runs every golden SQL file from `uops-query` against the live
schema. Both sides' unit tests pass while they drift, and it has already caught one:
the compiler emitted `searchAll()` / `searchAny()` — the names from the beta
announcements — which do not exist in 26.8. The real functions are `hasAllTokens()` and
`hasAnyTokens()`.

**The materialised columns here and `semconv::MATERIALIZED` in `uops-core` are one
list.** `uops_query::plan` rewrites `Attr` fields onto `host_name` and `service_name`
for logs and events. Dropping a `MATERIALIZED` clause here makes the compiler emit SQL
naming a column that does not exist; adding a key there without adding it here does the
same. `scripts/ch.sh smoke` asserts both are populated from real rows.

## Version

Pinned to **ClickHouse 26.8** in `deploy/docker-compose.yml` and in CI, matching what W1
was measured on. The text index reached GA in 26.x and its syntax moved during the beta,
so "whatever is latest" is not a safe default for a schema that depends on it.
