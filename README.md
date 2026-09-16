# uops — Unified Infrastructure Observability Platform

> **Status: pre-v0**, and no longer only documents. M0, M1 and M2 are implemented and
> tested; `docker compose up` gives an API, a web UI and an SNMP poller that fills them.
> See [Try it](#try-it).

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

**→ [STATUS.md](./STATUS.md) — start here on a new machine.**

- [x] Architecture decisions frozen for M0–M4
- [x] M0–M4 implementation specification
- [x] W1 storage benchmark **executed — architecture validated**
- [x] **M0** — primitives: envelope, identity, query AST, secrets, migrations, bus
- [x] **M1** — core platform: API, auth, inventory, telemetry, web shell, first run
- [x] **M2** — NMS: SNMP polling, profiles, discovery, rates, availability. All six
      acceptance criteria met and measured
- [ ] **M3** — logs ← in progress. Both syslog wire formats, RFC 6587 framing, UDP and
      TCP receivers, the shared pipeline, batched inserts and **the daemon** are done: a
      datagram on port 514 becomes a row you can query, and an unknown sender becomes a
      resource rather than a dropped message. The WAL spill, OTLP and the Log Explorer
      are not. TLS is terminated at a proxy by decision, not built in — see STATUS.md
- [ ] M4 — dashboards and alerting

Taken out of order because they were asked for: MAC vendor lookup, device make/model/
serial from a profile, and a site map.

## Try it

```bash
docker compose -f deploy/docker-compose.yml up -d
docker compose -f deploy/docker-compose.yml logs server | grep password
```

Open <http://localhost:8080> and sign in as `admin@example.invalid` with that password.
It is printed once, is not stored anywhere, and cannot be asked for again.

That gives you an API, a UI and a poller — and an empty inventory. A device becomes
*pollable* when it has somewhere to send a packet and something to authenticate with, so
there are three steps rather than one:

1. **Store a credential** — `POST /api/v1/credentials`. Material goes in and never comes
   out: there is no route that returns it, deliberately.
2. **Create a device** — `POST /api/v1/resources`.
3. **Give it an address and the credential** — `PUT /api/v1/resources/{id}/identifiers`
   with a `mgmt_ip`, and `PUT /api/v1/resources/{id}/credential`.

The address is an *identifier* rather than a column on the device, which is why it is its
own step: it is the thing identity resolution matches on, and a device that is re-addressed
should be re-identified and re-dialled by one fact changing once.

The poller re-reads the fleet every minute, so polling starts within a minute of step 3.

To try it against something without wiring up real equipment, start the bundled `net-snmp`
agent:

```bash
docker compose -f deploy/docker-compose.yml --profile test up -d snmp-agent
```

Its address inside the compose network is what goes in the `mgmt_ip`, and its credentials
are in [deploy/snmp-agent/README.md](./deploy/snmp-agent/README.md) — all of them public
on purpose.

### The key-encryption key

Generated on first run into a Docker volume, never committed: a key in a repository is a
key in every clone, every fork and every CI log. The API and the poller read the same
file, because a credential the API sealed that the poller cannot open is a device that
silently never gets polled.

`docker compose down` keeps it. `docker compose down -v` destroys it, and with it the
ability to decrypt every credential already stored — which is the only way to say that on
purpose.

## Name

The product is **Veyronis** — *Unified Infrastructure Observability & Operations
Platform*.

`uops` is a **working codename** and everything in the tree still uses it: the crates,
the binaries, the `UOPS_*` variables, the databases, the Docker images and the NATS
subjects. That is deliberate. The codename exists so the product name can change without
touching code, and it just did — `Aegisora` was rejected (`aegisora-ai/aegisora` is an
active org in an adjacent market) and replaced by `Veyronis` at the cost of eight lines
of documentation.

The identifiers rename to `veyronis-*` **at clearance** — GitHub org, crates.io, npm,
`.com`/`.io`, USPTO TESS and Bangladesh RJSC — which is also the first moment the crates
can be published. Doing it before would re-couple the tree to a name that has had a
preliminary search rather than a clearance, and would touch persistent state including the
key file that decrypts every stored credential. See [RENAME_AUDIT.md](./RENAME_AUDIT.md).

## License

**AGPL-3.0-only, with a Contributor License Agreement.** Decided 2026-09-16.

AGPL for everyone; a commercial license available to buyers whose legal teams maintain
AGPL blocklists — which is most of on-premise enterprise procurement, and is exactly the
market this product is built for. Selling that commercial license requires the right to
license *all* of the code that way, which is what the CLA preserves.

**The CLA is the time-critical half, not the license text.** Once one contribution lands
unsigned, relicensing any part of the project needs that person's individual consent
forever, and the dual-licensing path closes permanently. So a signed CLA is a gate on the
first pull request — see [CONTRIBUTING.md](./CONTRIBUTING.md).

> [CLA.md](./CLA.md) is a working draft modelled on the Apache Individual CLA and **has
> not been reviewed by a lawyer**. That review is the remaining gate before the repository
> is publicised. It is not legal advice.
