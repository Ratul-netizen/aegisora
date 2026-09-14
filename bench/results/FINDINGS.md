# W1 storage benchmark — findings

**Status: complete.** 10M and 100M both measured.
ClickHouse 26.8.2.7, single node, Docker Desktop on Windows, warm cache, 5 iterations.

> This file is the **raw evidence** — every query at both scales, with the reasoning
> that produced each amendment. The **decision** it supports is
> [`docs/benchmarks/w1.md`](../../docs/benchmarks/w1.md), which is what to read first.

---

## Headline

**The ClickHouse decision holds, and the core architectural bet is confirmed at scale.
The sort key needs a second ordering — a day-one change to SPEC §M0.6, not a later
optimisation.**

---

## 0. Scaling 10M → 100M

| id | query | 10M p50 | 100M p50 | 10M rows read | 100M rows read |
|---|---|---:|---:|---:|---:|
| **Q05** | **all signals for one resource** | **9 ms** | **9 ms** | **16.38 K** | **16.38 K** |
| Q02a | token: 1 hit | 13 ms | 39 ms | 8.19 K | 8.19 K |
| Q02b | token: 100 hits | 12 ms | 30 ms | 32.8 K | 278 K |
| Q02c | token: 10 K hits | 32 ms | 141 ms | 1.94 M | 18.9 M |
| Q02d | token: 1 M hits | 46 ms | 181 ms | 3.39 M | 33.5 M |
| Q03 | filter + time + severity | 22 ms | 59 ms | 262 K | 2.38 M |
| Q04 | filter + GROUP BY | 19 ms | 54 ms | 262 K | 2.38 M |
| Q06 | top-N talkers | 19 ms | 56 ms | 262 K | 2.38 M |
| Q11 | multi-token AND | 34 ms | 88 ms | 1.94 M | 18.9 M |
| Q12 | token + GROUP BY | 57 ms | 230 ms | 1.94 M | 18.9 M |
| Q01 | **tail recent logs** | 93 ms | **403 ms** | 3.70 M | **33.8 M** |
| Q09 | full-range histogram | 179 ms | **1 066 ms** | 3.39 M | 33.5 M |
| Q07 | substring LIKE | 314 ms | **1 928 ms** | 3.39 M | 33.5 M |
| Q08 | GROUP BY map key | 584 ms | **2 252 ms** | 3.39 M | 33.5 M |
| Q10 | phrase proximity | 469 ms | **2 378 ms** | 3.39 M | 33.5 M |

**Two clean groups.** Queries that can use the sort key or the text index stay
comfortably under 250 ms at 100M. Queries that read the whole tenant land between
0.4 s and 2.4 s. There is nothing in between, and the dividing line is exactly
"can this query prune?"

### Q05 is size-independent — the central bet, confirmed

**9 ms and 16 380 rows read at 10M. 9 ms and 16 380 rows read at 100M.** Ten times the
data; identical latency, identical work. `ORDER BY (tenant_id, resource_id, observed_at)`
makes resource-scoped investigation a function of *how much that resource produced*, not
of how large the table is.

This is the query the Investigation Workspace (PLAN §6) is built on, and it is the single
result that most justifies the architecture. **Do not change this sort key.**

### Selective token search is also size-independent

Q02a read **8 190 rows at both scales** to find its single hit. The text index prunes to
a constant absolute cost regardless of table size. Latency rose 13 → 39 ms because the
dictionary itself grew, but the data read did not move at all.

### Correction to the 10M projection

The 10M findings projected Q01 at **~0.9 s** for 100M by linear extrapolation. The
measured value is **403 ms** — the estimate was 2x pessimistic, because ClickHouse
parallelises the scan across cores until they saturate. The 1B projection of ~9 s should
likewise be treated as an upper bound; ~1.5–4 s is more plausible.

**This does not change the conclusion.** 403 ms for the default view is already poor, it
reads the entire tenant, and it grows without bound as retention extends. The projection
in §3 is still required — the urgency was overstated, the need was not.

---

## 1. Q05 — the Investigation Workspace query — is excellent

This is the query the whole sort key exists for (PLAN §6): *all signals for one
resource in a window*.

| | |
|---|---|
| p50 | **9 ms** |
| p95 | 36 ms |
| rows read | **16.4 thousand** out of 10M |

ClickHouse read 0.16% of the table. `ORDER BY (tenant_id, resource_id, observed_at)`
does exactly what SPEC §M0.6 claims: resource-scoped investigation is a contiguous
range read. **This decision is confirmed and should not change.**

## 2. Token search is excellent, and degrades sensibly

| query | selectivity | p50 | rows read |
|---|---|---:|---:|
| Q02a ultrarare | 1 hit | **13 ms** | 8.2 K |
| Q02b rare | 100 hits | **12 ms** | 32.8 K |
| Q02c mid | 10 K hits | 32 ms | 1.94 M |
| Q02d common | 1 M hits | 46 ms | 3.39 M |

The text index prunes hard when the term is selective — 8 000 rows read to find 1 hit
in 10 million. As selectivity drops the index correctly stops helping and the engine
falls back to scanning; 46 ms for a 1M-hit aggregate is a fine outcome.

Q11 (multi-token AND) = 34 ms and Q12 (token + GROUP BY) = 57 ms confirm the pattern
that matters for ops: **search, then aggregate, in one engine.**

## 3. Q01 — "tail the most recent logs" — is the problem

| | |
|---|---|
| p50 | 93 ms |
| rows read | **3.70 million — the entire tenant** |

`ORDER BY observed_at DESC LIMIT 1000` cannot use the sort order. Within a tenant the
data is ordered by `resource_id` first, so the globally-latest rows are scattered across
all 5 000 resources' ranges. ClickHouse must read every row in the tenant to find the
newest 1 000.

**This is the single most common query in a log explorer** — the default view, the thing
a user sees before they have filtered anything. And it scales linearly with retention:

| rows | projected Q01 |
|---|---|
| 10 M | 93 ms (measured) |
| 100 M | ~0.9 s |
| 1 B | ~9 s |

Nine seconds to open the default view is not shippable.

### Resolution of the SPEC §M0.6 [OPEN] item

The open question was phrased as *"tenant-wide **text search** may be slow; decide from
the W1 numbers."* The measurement shows the framing was too narrow. Text search is fine —
the index handles it. **It is the unfiltered time-ordered tail that breaks**, and no
text index helps because there is no text predicate.

So the answer is yes, a second ordering is required — but it should be a **ClickHouse
projection on the same table**, not the separate `logs_by_time` table this benchmark
uses. A projection is maintained transactionally by ClickHouse and selected automatically
by the query planner, so the Query AST compiler (§M0.5) does not need to know it exists.
The separate table here exists only to measure the two orderings independently.

```sql
ALTER TABLE logs ADD PROJECTION p_by_time (
    SELECT * ORDER BY (tenant_id, observed_at)
);
```

**Cost: roughly 2x storage for the logs table.** That must go into SPEC §M0.6 as a stated
cost, not discovered during M3.

### Q09 — the Explorer histogram — has the same root cause and is worse

Only visible at 100M: **Q09 (`GROUP BY toStartOfHour`) costs 1 066 ms**, up from 179 ms
at 10M, reading the entire tenant.

That query is the **histogram at the top of the Log Explorer** (SPEC §M3, the
drag-to-zoom time selector). Unlike Q01, which a user triggers once on arrival, the
histogram re-renders on **every search and every filter change**. A second of latency on
every interaction is the difference between a tool that feels alive and one that feels
broken — and it is the first thing any evaluator touches.

Same root cause as Q01: a time-bucketed aggregate cannot use a resource-first sort key.
The `p_by_time` projection helps, but a histogram over 180 days of retention will still
scan a lot. The durable answer is a **pre-aggregated counts table** — an
`AggregatingMergeTree` MV keyed `(tenant_id, bucket, severity)` — which turns the
histogram into a read of a few thousand rows regardless of retention, exactly as
`metrics_5m` does for metrics.

That is cheap to add now (it is the same pattern already specified for metric rollups)
and awkward later, because it changes what the Explorer queries.

**Action for SPEC §M0.6:** add `logs_counts_5m` alongside `metrics_5m`.

### Measured: the second ordering fixes the tail, and does NOT fix the histogram

Both tables loaded with the same 100M rows, queried back to back under identical
conditions:

| query | `logs` (resource-ordered) | `logs_by_time` (time-ordered) | |
|---|---:|---:|---|
| Q01 tail | 2 303 ms · 33.78 M rows | **72 ms · 254 K rows** | **32x faster, 133x less read** |
| Q09 histogram | 3 227 ms · 33.47 M rows | **1 357 ms · 33.39 M rows** | 2.4x faster, **same rows read** |

> The `logs` figures here are slower than the earlier suite run (Q01 403 ms) because both
> 100M tables now share one machine's cache and I/O. The **comparison within this run is
> valid** — both were measured under identical contention — but these absolute numbers
> should not be compared against the single-table run above.

**Q01: settled.** The time ordering turns a full-tenant scan into a 254 K-row read. The
projection is mandatory.

**Q09: the projection is not the answer.** Rows read barely moved — 33.47 M → 33.39 M.
The 2.4x gain is better locality, not pruning, because a histogram over the *full
retention window* has to touch every row whatever the sort order. Only pre-aggregation
removes the work. This confirms §3 above: **`logs_counts_5m` is required in addition to
the projection, not instead of it.** Two fixes, two different root causes.

### Storage cost: ~1.9x, not 2x — and the index must not be duplicated

| table | data | text index | total |
|---|---:|---:|---:|
| `logs` | 3.03 GiB | 2.15 GiB | 5.18 GiB |
| `logs_by_time` | **4.79 GiB** | 2.14 GiB | 6.94 GiB |

Two things worth noting.

**Time-ordering compresses ~58% worse** (4.79 vs 3.03 GiB for identical data). Sorting by
resource groups rows that share `source_vendor`, `source_kind` and `host.name`, so those
columns compress far better. Sorting by time interleaves all 5 000 resources. This is a
real cost of the second ordering and was not anticipated.

**But the text index should not be duplicated.** In this benchmark `logs_by_time` is a
separate table and so carries its own 2.14 GiB index; as a real ClickHouse *projection*
it would not — text search resolves against the main table, and projections do not carry
secondary indexes anyway. Realistic cost:

```
3.03 (data) + 2.15 (index) + 4.79 (projection data) = 9.97 GiB
versus 5.18 GiB for the main table alone  →  1.9x
```

**Action for SPEC §M0.6:** state the projection cost as **~1.9x**, and note explicitly
that the projection must not replicate the text index.

## 4. The text index is not free: 67% overhead

| | 10M | 100M |
|---|---|---|
| compressed data | 320 MiB | **3.03 GiB** |
| **text index** | **214 MiB (67%)** | **2.15 GiB (71%)** |
| uncompressed | 1.86 GiB | 18.56 GiB |
| compression ratio | 5.93x | 6.12x |
| active parts | 16 | 35 |

The index overhead **grew** with scale, 67% → 71%. It is not amortising.

The index costs two-thirds of the compressed data size. Combined with the projection
above, a naive implementation stores roughly **3x** what the raw compressed logs need.

Levers, in order of preference: index only `body` (already the case), consider a coarser
tokenizer, and make the text index **opt-out per tenant or per source** for high-volume
low-search-value streams. That last one belongs in the monitoring-profile model (M2).

> The 5.93x compression ratio is **pessimistic and should not be quoted as a product
> number.** The generator embeds random IPs, ports and counters in every line to defeat
> dictionary compression deliberately. Real syslog is far more repetitive; published
> ClickHouse log workloads reach 10–30x.

## 5. Confirmed slow, exactly as the GA documentation says

| query | p50 | rows read |
|---|---:|---:|
| Q10 phrase proximity | 469 ms | 3.39 M (full tenant scan) |
| Q07 substring `LIKE` | 314 ms | 3.39 M (full tenant scan) |

Both read the entire tenant — no pruning at all.

**Correction to an earlier assumption.** ClickHouse 26.8 exposes
`use_text_index_like_evaluation_by_dictionary_scan=1` and
`text_index_like_min_pattern_length=4`, which suggested `LIKE` might be index-accelerated
beyond what the March 2026 GA post describes. **It was not, on this query.** Q07 read
every row. Do not plan on index-accelerated substring search.

This is not a blocker — PLAN §3 already places phrase and fuzzy outside the requirement
set, and ops search sorts by time, not relevance. But the UI must set expectations: the
Query AST's `TextMode::Phrase` and `Substring` should surface the `QueryWarning` that
SPEC §M0.5 already specifies.

## 6. Slowest query is a Map lookup, not a text search

Q08 (`GROUP BY attributes['host.name']`) at **584 ms p50** is the slowest in the suite —
worse than phrase search.

High-cardinality `GROUP BY` over a `Map` column requires decompressing the whole map for
every row. Any attribute that is queried or grouped on routinely should be a real column,
not a map entry. `host.name` is precisely such an attribute.

**Implication for SPEC §M0.3:** the telemetry envelope's `attributes` map is right for the
open-ended tail, but the handful of semconv keys that drive grouping deserve promotion to
materialized columns:

```sql
host_name    String MATERIALIZED attributes['host.name'],
service_name LowCardinality(String) MATERIALIZED attributes['service.name'],
```

## 7. Harness artifacts — not ClickHouse numbers

Recorded so nobody later mistakes them for storage limits:

| | |
|---|---|
| `docker exec -i` stdin load | 182 s / 1M rows |
| HTTP interface load | 9.9 s / 1M rows (**18x faster**) |
| `curl --data-binary @-` | buffers entire stream; 6.7 GB RSS climbing toward ~32 GB |
| batched HTTP load | 57 000 rows/s sustained, curl RSS flat at 658 MB |

The 57 k rows/s ingest figure is **not** a ClickHouse ceiling — it is a single-threaded
generator piped through curl on Docker Desktop for Windows. Real ingest uses batched
native-protocol inserts from the pipeline (SPEC §M3.3). Treat it as "fast enough to load
the benchmark", nothing more.

---

## Actions for SPEC — all folded in

- [x] §M0.6 — `p_by_time` projection added, with the measured ~1.9x storage cost
- [x] §M0.6 — materialized columns for grouped semconv attributes (§6 above)
- [x] §M0.6 — text index recorded at 71% of compressed data at 100M; opt-out per source
- [x] §M0.5 — confirmed: `TextMode::Phrase`/`Substring` emit `QueryWarning`
- [x] §M0.6 — tenant-wide-search [OPEN] item closed with the finding in §3
- [x] §M0.2 — unaffected. §M0.6 sort key for resource-scoped reads: **confirmed**

All six are implemented, not merely specified: see `ch-migrations/` and
`crates/uops-query/`.

## Still to measure

Scoped out of the W1 verdict deliberately — see "What this verdict does not cover" in
[`docs/benchmarks/w1.md`](../../docs/benchmarks/w1.md).

- 1B rows (does Q01 scale as linearly as projected?)
- Metrics ingest throughput, and the write cost of maintaining the rollup MVs (M4)
- Cold-cache numbers (`run.sh --cold`)
- Concurrency ceiling
- Replicated / sharded behaviour
