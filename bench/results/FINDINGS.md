# W1 storage benchmark — findings

**Status: interim (10M rows). 100M run in progress.**
ClickHouse 26.8.2.7, single node, Docker Desktop on Windows, warm cache, 5 iterations.

---

## Headline

**The ClickHouse decision holds. The sort-key decision does not — it needs a second
ordering, and that is a day-one change to SPEC §M0.6, not a later optimisation.**

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

## 4. The text index is not free: 67% overhead

| | |
|---|---|
| compressed data | 320 MiB |
| **text index** | **214 MiB** |
| uncompressed | 1.86 GiB |
| compression ratio | 5.93x |

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

## Actions for SPEC

- [ ] §M0.6 — add the `p_by_time` projection and state the ~2x storage cost
- [ ] §M0.6 — add materialized columns for grouped semconv attributes (§6 above)
- [ ] §M0.6 — record the text index at ~67% of compressed data; make it opt-out per source
- [ ] §M0.5 — confirmed: `TextMode::Phrase`/`Substring` must emit `QueryWarning`
- [ ] §M0.6 — close the tenant-wide-search [OPEN] item with the finding in §3
- [ ] §M0.2 — unaffected. §M0.6 sort key for resource-scoped reads: **confirmed**

## Still to measure

- 100M and 1B rows (does Q01 scale as linearly as projected?)
- `logs_by_time` comparison to quantify the projection's benefit directly
- Metrics table + the `metrics_5m` rollup MV, and rollup cost on ingest
- Cold-cache numbers (`run.sh --cold`)
- Concurrency ceiling
