# uops — Unified Infrastructure Observability Platform

> **Status: pre-v0.** No product code yet. This repository currently holds the
> architecture decisions and the W1 storage benchmark that validates them.

Network monitoring, infrastructure monitoring, logs, metrics, traces, flows, topology,
events and automation on **one resource identity and one correlation model** — rather than
an NMS bolted to a log stack.

The gap this targets is real and measured: traditional NMS tools (LibreNMS, Zabbix,
Observium) do not do APM, traces or serious log analytics; observability stacks
(Elastic, Grafana, SigNoz) do not do SNMP, flow or network topology. Nothing sits in
the middle with a shared identity model.

## The thesis, in one sentence

> Storage, polling and ingestion are commodities. **Resource identity and cross-signal
> correlation are the product.**

When `router-01` appears in SNMP, syslog, NetFlow, LLDP, a config backup and an alert,
all six resolve to one `resource_id`. Everything downstream — correlation, blast radius,
"tell me what is wrong and why" — depends on getting that right.

## Documents

| | |
|---|---|
| **[PLAN.md](./PLAN.md)** | Strategy, frozen architecture decisions, roadmap M0–M13, open questions |
| **[SPEC.md](./SPEC.md)** | Implementation specification for M0–M4: DDL, Rust types, API surface, acceptance criteria |
| **[bench/](./bench/)** | W1 storage benchmark — the go/no-go on the ClickHouse decision |

## Architecture, briefly

```
        CONTROL PLANE                          DATA PLANE
         PostgreSQL                            ClickHouse
             │                                      │
  orgs, tenants, sites, users, RBAC   ┌─────┬───────┼───────┬──────┐
  resources + identifiers + aliases   │     │       │       │      │
  relationships (graph edges)      metrics logs  traces  flows  events
  sealed credentials                  │     │       │       │      │
  monitoring profiles                 └─────┴───────┼───────┴──────┘
  alert rules, dashboards                    S3 / MinIO tiering
```

**Two storage engines, not four.** ClickHouse full-text search reached GA in March 2026,
so one columnar engine covers logs, metrics, traces and flows — with documented limits
(no BM25, no fuzzy, phrase proximity unindexed) that ops log search does not need.

**Stack:** Rust · Axum · tokio · sqlx · PostgreSQL · ClickHouse · OpenTelemetry Collector ·
React · TypeScript · Vite

## Current state

- [x] Architecture decisions frozen for M0–M4
- [x] M0–M4 implementation specification
- [x] W1 benchmark harness (generator + DDL + query suite)
- [ ] **W1 benchmark executed** ← the next gate
- [ ] M0 primitives
- [ ] M1 core platform

Nothing proceeds past the W1 result. If ClickHouse does not hold up under the measured
workload, the storage architecture in SPEC §M0.6 changes before any code is written
against it.

## Name

`uops` is a **working codename**, not the product name. `Aegisora` was rejected —
`aegisora-ai/aegisora` is an active org in an adjacent market (AI runtime security).
Crates stay unpublished under the codename until trademark, domain and org clearance.

## License

**Undecided.** AGPL / Apache 2.0 / BSL — see PLAN.md §11. This blocks the first public
release, since relicensing after outside contributions requires their consent.
