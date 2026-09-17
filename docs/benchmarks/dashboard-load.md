# M4 — the dashboard load: measured

**SPEC §M4 acceptance criterion: *"dashboard with 20 panels loads p95 < 3 s over 30 days
of data (rollups exercised)."* Met, at 0.42 s, from the pre-aggregate.**

| | |
|---|---|
| **Ran** | September 2026 |
| **Subject** | `uops-api` through its own router, against PostgreSQL 17 and ClickHouse 26.8, single-node in Docker Desktop on Windows |
| **Scale** | 20 panels, 30 days, 172 800 metric rows from 20 devices at one sample every five minutes |
| **Harness** | [`crates/uops-api/tests/dashboard_load.rs`](../../crates/uops-api/tests/dashboard_load.rs) — `cargo test -p uops-api --test dashboard_load -- --ignored --nocapture` |

```text
20 panels over 30 days · 172800 rows from 20 devices
  seeded in     5.5s
  page load     p50 0.27s · p95 0.42s · worst 0.42s  (budget 3s)
  answered by   metrics_5m
```

Run twice: p95 0.42 s and 0.44 s, both from the pre-aggregate.

---

## What "loads" means

Twenty `POST /api/v1/query` requests through the real router — session, CSRF, tenant
resolution, the compiler, ClickHouse — issued together the way a browser issues them, and
timed until the last one answers. That is what a person waits for. Timing one panel and
multiplying by twenty would measure a page nobody loads, and timing them sequentially
would measure a browser nobody has.

Each panel asks about a different device, so twenty panels are twenty different questions.
A dashboard of twenty identical panels would have measured a cache.

## "Rollups exercised" is half the criterion

It is not automatic, and it is the half that could have passed by accident. A thirty-day
window only reaches `metrics_5m` because the panel asks for **wide buckets** — and it only
asks for wide buckets because `forWindow` rewrites the stored bucket to suit the window
when the header's range changes. A panel saved over an hour keeps five-minute buckets; over
thirty days that is 8 640 of them, past the panel's own limit, and the chart would have
shown the first few hundred as though they were the whole month.

So the test asserts the answering table as well as the time. A thirty-day scan of raw
points that happened to be fast enough would have passed the timing and proved nothing.

## What the number says

**Seven times inside the budget**, and the shape of the work explains why: the
pre-aggregate holds one row per five-minute bucket per series, so a month is about 8 600
rows per device rather than 8 600 samples — and the twelve-hour buckets a month-wide chart
asks for are `AggregatingMergeTree` states being merged, not points being scanned.

**The twenty run concurrently and do not contend.** p50 0.27 s and p95 0.42 s over ten
loads means the slowest page was not much worse than the median, which is what "twenty
independent requests" is supposed to buy — one slow panel spins on its own rather than
holding the other nineteen. That is the design decision this measurement confirms: the
panels are not batched into one request, deliberately.

## What this does not measure

* **One machine.** The same caveat as the alert cycle: a production ClickHouse has more
  than this laptop's cores, and a production PostgreSQL is not sharing them with the thing
  querying it.
* **Twenty devices, not twenty thousand.** The rollup's row count grows with series, and
  a panel scoped to a whole tenant of ten thousand devices reads ten thousand series'
  worth of states rather than one's. W1 measured that dimension for raw reads; this one
  holds the estate small on purpose so the thirty-day *window* is what is under test.
* **Log panels.** Every panel here is a metric aggregate. A table panel over logs with a
  substring filter costs what that filter costs — which the Explorer warns about before
  anybody saves it as a search, and therefore before it becomes a panel.
* **The browser.** This is the server's half of the load. Drawing twenty SVG charts is the
  other half and is not measured here.
