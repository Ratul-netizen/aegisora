# M4 — the alert cycle: measured

**SPEC §M4 acceptance criterion: *"1 000 rules evaluate within one 60 s cycle."* Met, with
about five times the budget to spare.**

| | |
|---|---|
| **Ran** | September 2026 |
| **Subject** | `uops-alert` against PostgreSQL 17 and ClickHouse 26.8, both single-node in Docker Desktop on Windows |
| **Scale** | 1 000 threshold rules over 50 devices, 500 metric samples |
| **Harness** | [`crates/uops-alert/tests/cycle.rs`](../../crates/uops-alert/tests/cycle.rs) — `cargo test -p uops-alert --test cycle -- --ignored --nocapture` |

```text
1000 rules over 50 devices
  seeded in     5.3s
  cycle         12.09s  (budget 60s, 16 in flight)
  per rule      p50 180 ms · p95 331 ms · worst 543 ms
  throughput    83 rules/s
```

Run twice, a few minutes apart: 12.09 s and 12.52 s, p95 331 ms and 306 ms. Quoted as
"about twelve seconds" everywhere below, because the difference between those two runs is
the laptop and not the engine.

---

## What was being measured

Not `Engine::cycle`, which evaluates rules one after another — that would measure a design
nothing runs. The measurement drives `evaluate_and_deliver` per rule with the same
`IN_FLIGHT` bound the run loop uses, which is the path an installation actually takes:
read the rule, resolve its selector against PostgreSQL, run one ClickHouse query, read the
rule's current state, decide each series' phase, write the ones that changed.

Fifty devices rather than one, so every evaluation reads a result set with fifty series in
it. A thousand rules over a single device would have measured a thousand nearly-empty
queries, which is a number nobody needs.

## What the number says

**The budget is not close.** Twelve seconds of sixty, with five times the headroom — and
the headroom is bigger than it looks, because this is close to a worst case: *every* rule
fires on *every* hot device, so the cycle pays for 5 000 state writes on top of 1 000
reads. A real installation has most rules quiet most of the time, and a quiet rule writes
nothing at all (see the `recovering` condition in `engine.rs`, which is there because the
first version rewrote every healthy row every cycle).

**The bound is per-rule latency against concurrency, not either database.** 16 in flight
at a p50 of 180 ms is 89 rules a second, and 83 is what was measured — so the cycle is
doing exactly what the semaphore allows and nothing is queueing behind a lock. An
installation that needed more would raise `IN_FLIGHT`, and the next thing to bind would be
the ClickHouse connection count rather than anything in this crate.

**Where the 180 ms goes.** Four round trips and up to five writes per rule, across two
databases on the same laptop as the test. It is not one slow thing; it is the sum of a
handful of small ones, which is why the answer to "make it faster" is concurrency rather
than optimisation.

## What this does not measure

* **One machine, one node each.** A production ClickHouse serves this from more than one
  core and a production PostgreSQL is not sharing a laptop with the thing querying it.
* **Rules whose queries are expensive.** Every rule here is `avg(value) > 90` over five
  minutes, which is the shape SPEC uses as its example. A rule with a substring search
  over a wide window costs what that search costs; the Explorer's warnings say so before
  somebody saves one as a rule, and `must_compile` refuses the ones that cannot run at
  all.
* **A thousand rules across a thousand tenants.** They are one tenant's here. The
  suppression cache is per tenant, so a thousand tenants would read a thousand maintenance
  maps per cycle rather than one — the cache would still hold each for ten seconds, but
  the constant is different and unmeasured.
* **Delivery.** `notify` is empty on every rule: this measures evaluation. A thousand rules
  firing at one webhook would measure the rate limiter refusing them, which
  `uops-store-pg`'s own tests already do at 5 000.

## What it changes

Nothing, which is the useful outcome. The design holds at the scale SPEC names, so the
next thing to measure is the one it does not cover: many tenants rather than many rules.
That is recorded in STATUS rather than guessed at here.
