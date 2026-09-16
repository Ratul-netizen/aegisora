# UI direction — the operations cockpit

Status: **direction, not a commitment.** Nothing here is implemented beyond what
`STATUS.md` records (the shell, auth, tenant switcher, inventory, resource detail, a
first explorer and a tile-free site map). This document exists so that when the UI work
starts it starts from one coherent design rather than a different pattern per page.

Written 2026-09-16 from a research pass across Grafana, Kibana, Dynatrace, Motadata
ObserveOps, Netdata, Kentik and current 3D-topology products.

---

## The one-sentence philosophy

> Veyronis should not look like "Grafana plus a 3D map". It should look like a
> purpose-built network-operations and observability cockpit, where 3D is used
> selectively for topology and spatial context while the dashboard itself stays
> information-dense, fast and operationally useful.

Restated as rules:

| | |
|---|---|
| **2D** | information |
| **3D** | spatial relationships |
| **animation** | state change |
| **colour** | semantic state, never decoration |
| **interaction** | investigation |

And the rule that keeps the two halves of the product honest:

> The UI is a visualization and investigation layer over the architecture that already
> exists — **not a second architecture.** `Resource` identity, the `TelemetryEnvelope`,
> the Query AST and the correlation model are the frontend's foundation too.

---

## 1. Shell and navigation

Dark-first. Near-black navy ground, subtle translucent surfaces, restrained gradients,
thin borders, high density, soft glow on active states. The reference points are
Dynatrace Smartscape for context, Grafana for density, and a NOC wall for legibility —
not neon borders and rotating panels, which stop being interesting in about four minutes.

```
╔══════════════════════════════╗
║ V VEYRONIS                   ║
╠══════════════════════════════╣
║  ◉ Overview                  ║
║                              ║
║  NETWORK                     ║
║  ├─ Resources                ║
║  ├─ Interfaces               ║
║  ├─ Topology                 ║
║  ├─ Traffic                  ║
║  └─ Discovery                ║
║                              ║
║  OBSERVABILITY               ║
║  ├─ Metrics                  ║
║  ├─ Logs                     ║
║  ├─ Traces                   ║
║  └─ Events                   ║
║                              ║
║  OPERATIONS                  ║
║  ├─ Alerts                   ║
║  ├─ Incidents                ║
║  ├─ Investigations           ║
║  └─ Automation               ║
║                              ║
║  DASHBOARDS                  ║
║  ├─ My Dashboards            ║
║  ├─ Templates                ║
║  └─ NOC Screens              ║
║                              ║
║  ADMIN                       ║
║  ├─ Tenants                  ║
║  ├─ Users & RBAC             ║
║  ├─ Integrations             ║
║  └─ Settings                 ║
╚══════════════════════════════╝
```

Collapsible to icon-only. Top bar carries search, org/tenant, the global time range, a
live indicator, alerts and the user menu.

---

## 2. Context Mode — the thing worth building first

A single control at the top:

```
Context: All Resources ▾
```

which can be narrowed to:

```
All Resources → Organization → Tenant → Site → Resource Group → Resource → Interface → Service
```

and once it is set, **the whole application is scoped to it** — dashboard, topology,
logs, metrics, alerts, incidents, flows.

This is the UI expression of the thing the backend was built around, and it is cheap
here precisely because the backend already scopes everything by `TenantScope` and
resolves every signal to a `resource_id`. A product where the operator sets the context
once and every view obeys it is a different experience from one where they re-filter on
every page.

**Time is global too.** Selecting *Last 6 hours* means metrics, logs, events, alerts,
flows and topology state all understand that window.

---

## 3. The default dashboard has to be good before anybody configures anything

A user installs Veyronis, adds a few resources, and gets something useful immediately.
Netdata makes this a selling point and it is the right instinct: a product whose first
screen is empty has to be learned before it can be judged.

```
┌──────────────────────────────────────────────────────────────────────────┐
│ OPERATIONS OVERVIEW                         Last 30 min ▾   LIVE ●       │
│ All Tenants / All Sites                                                  │
├──────────────────────────────────────────────────────────────────────────┤
│  RESOURCES       ALERTS          INCIDENTS       AVAILABILITY            │
│     248             12                3             99.97%               │
│   ● 231 UP       ▲ 5 critical      ● 1 major       +0.12%                │
├───────────────────────────────────────┬──────────────────────────────────┤
│       NETWORK HEALTH                  │       INCIDENTS / ALERTS         │
│       ╭───────────────╮               │  ● Core-RTR-01   Link Down       │
│       │       97.4%   │               │  ● SW-DC-04      High CPU        │
│       │     HEALTH    │               │  ● FW-EDGE-02    Packet Loss     │
│       ╰───────────────╯               │  ● API-GW-01     Latency         │
├───────────────────────────────────────┴──────────────────────────────────┤
│                         LIVE TOPOLOGY                                    │
│                    ◉ Core Router                                         │
│                  ╱   │       ╲                                           │
│                ◉     ◉         ◉                                         │
│               SW1   FW1       SW2                                        │
├───────────────────────────────────────┬──────────────────────────────────┤
│ TRAFFIC                               │ ERROR / PERFORMANCE              │
├───────────────────────────────────────┴──────────────────────────────────┤
│ TOP RESOURCES                                                            │
│ Resource       CPU     Memory    Traffic     Latency    Status           │
└──────────────────────────────────────────────────────────────────────────┘
```

**Templates must be data-aware.** Zero flow collectors means no giant empty Flow
dashboard. Five hundred SNMP resources means the Network dashboard is already populated.
Enabling OTel adds services, traces, latency, errors and dependencies. A dashboard that
shows an operator eight empty panels has taught them the product does not work.

### The shipped library

1. **Operations Overview** — the landing page
2. **Network Operations** — devices, interfaces, traffic, loss, errors, availability, topology
3. **Infrastructure** — CPU, memory, disk, VMs, hosts, containers
4. **Logs & Events** — volume, error rate, top sources, event stream, critical events
5. **Traffic / Flow** — top talkers, destinations, protocols, throughput, Sankey, geography
6. **Incident Command** — active incidents, timeline, affected resources, topology, alerts
7. **Security Operations** — later

---

## 4. Everything is contextual, and "Investigate" is everywhere

The failure mode to avoid:

> dashboard → chart → click device → another page → find logs → another page

Clicking a resource anywhere slides out a panel:

```
CORE-RTR-01
Cisco ASR1001-X
──────────────────────
● HEALTHY
Availability   99.98%
CPU              42%
Memory           67%
Interfaces    47 / 48 UP
Traffic       8.2 Gbps
──────────────────────
ACTIVE EVENTS
▲ Interface Gi0/0/3 — packet loss 4.2%
──────────────────────
Metrics · Logs · Flows · Topology · Alerts · Config · Events
[ Open Investigation ]
```

Every widget gets the same overflow menu: open resource, view metrics, view logs, view
events, view topology, investigate.

**Cross-filtering**: clicking `Core-RTR-01` in any panel scopes every other panel on the
dashboard to it, and centres it in the topology.

---

## 5. The Investigation Workspace

SPEC already calls this the long-term product concept; this is what it looks like.

```
┌──────────────────────────────────────────────────────────────────┐
│ Investigation: Core-RTR-01                     Started 14:32     │
├──────────────────────────────────────────────────────────────────┤
│ TIMELINE                                                         │
│ 14:31 ────── 14:32 ────── 14:33 ────── 14:34 ──────              │
│                ▲ INCIDENT                                        │
├────────────────────────────┬─────────────────────────────────────┤
│ TOPOLOGY                   │ METRICS                             │
│       ◉──●──◉              │ CPU / Traffic / Loss                │
├────────────────────────────┼─────────────────────────────────────┤
│ LOGS                       │ EVENTS                              │
│ 14:32:01 link state down   │ Interface Gi0/0/3 DOWN              │
│ 14:32:04 BGP reset         │ Packet loss threshold exceeded      │
│ 14:32:08 route changed     │ Incident created                    │
└────────────────────────────┴─────────────────────────────────────┘
```

The seed of it already has a name in SPEC §M3: **"show all signals for this resource
around this timestamp"** — the one interaction that demonstrates the thesis in ten
seconds, and the one that must exist before any of the rest is worth building.

---

## 6. Topology, in 2D and 3D

Dashboard stays a 2D cockpit. Topology is where 3D earns its place, because the thing
being shown is genuinely spatial.

**Modes:** 2D graph · 3D scene · geographic.

**Interactions:** rotate, pan, zoom, select, multi-select, focus, collapse and expand
groups, hide healthy nodes, show only unhealthy, show traffic, show dependencies, L2, L3,
application dependencies, geographic relationships — and **time travel**: *topology at
14:32* versus *topology now*.

**Semantic zoom** is what stops 2 000 nodes becoming an unusable hairball:

```
[Dhaka DC]
   ↓
[Core Network] [Compute] [Storage] [Security]
   ↓
Core-01  Core-02  SW-01  SW-02
   ↓
individual interfaces / services
```

This maps directly onto the resource hierarchy that already exists — site → resource →
child resource → interface.

**Animated links** show direction and load with subtle moving particles: grey for
unknown, normal for healthy, amber for a utilization warning, red for critical, dashed
for a degraded or unconfirmed relationship. Animation must mean *something is happening*,
not *the designer found CSS animations*.

**And it is an investigation tool, not eye candy.** SW1 goes down; selecting it
highlights the affected downstream resources, interfaces, alerts, packet loss, logs,
flows and incidents. That is worth far more than a pretty graph.

---

## 7. Dashboard builder

Take Grafana's and Kibana's ideas; do not take their complexity.

```
Create Dashboard
  Blank · Network Operations · Infrastructure · Observability · Security Operations
```

Editor: a widget palette on the left, drag, drop, resize, duplicate, delete, configure.

### Widgets

Metric · Metric Group · Time Series · Area · Bar · Gauge · Table · Alert List ·
Incident List · Event Stream · Log Stream · Topology · 3D Topology · Geographic Map ·
Heatmap · Histogram · Sankey · Dependency Graph · Resource Status · Availability ·
Markdown · Image · Text · Embedded View.

### Layouts

Freeform · Grid · Fullscreen NOC · TV rotation (Overview → Network → Topology →
Incidents → Traffic → Overview).

---

## 8. Logs, and search

Logs should read like an observability product, not a `timestamp | message` table:

```
14:32:04.183  ERROR   Core-RTR-01   BGP
Peer 10.20.1.5 session reset

severity   ERROR        resource  Core-RTR-01
site       DC-01        protocol  BGP           peer  10.20.1.5
```

Expanding one exposes related metrics, events, alerts, topology, incident and traces.

**Search** is one box that accepts a resource name, a condition (`packet loss > 5%`), a
phrase (`logs from firewall`) and eventually natural language. The UI never needs to know
whether that becomes ClickHouse SQL, a resource lookup or a topology traversal — that is
what the Query AST is for, and it is already the boundary.

**Command palette** on `Ctrl-K`: open a resource, investigate incidents, view topology,
search logs, create a dashboard, add a resource, open NOC mode, switch tenant.

---

## 9. Visual identity

| | |
|---|---|
| Background | very dark blue / charcoal |
| Surfaces | near-black cards, subtle elevation |
| Accent | a single cyan / blue-violet family, used sparingly |
| Healthy | green |
| Warning | amber |
| Critical | red |
| Unknown | grey |
| Maintenance | blue / purple |
| Type | Inter, Geist or IBM Plex Sans |
| Mono | IPs, timestamps, log bodies, metric values, query text, identifiers |

**Micro-interactions**, all small: a resource going down pulses rather than flashing the
screen red; a new alert gets a tiny indicator; a new incident drops a timeline marker;
traffic moves as particles along topology links; a refresh never reloads the page; a
drilldown slides rather than navigates.

---

## 10. Accessibility is not negotiable

Keyboard navigation · high-contrast mode · colour **and** icon, never colour alone ·
scalable text · reduced-motion · screen-reader labels · a NOC distance mode · fullscreen
· responsive layout. A status conveyed only by hue is unreadable to roughly one man in
twelve, and a NOC wall is viewed from four metres away.

---

## 11. Proposed frontend stack

Already fixed by SPEC: **React + TypeScript**. The rest:

```
                 VEYRONIS UI
                     │
       ┌─────────────┼─────────────┐
    Dashboard     Topology       Maps
       │             │             │
    ECharts     React Flow    MapLibre
                     │
              React Three Fiber
                     │
                  Three.js
```

- **Apache ECharts** — the chart layer. Broad visualization set, Canvas and SVG,
  progressive rendering, and accessibility features built in.
- **React Flow** — editable 2D graphs: dragging, zooming, panning, selection, minimap,
  custom nodes.
- **React Three Fiber** over Three.js — the 3D topology scene, as React components.
- **MapLibre GL** — geographic views, vector tiles, globe and 3D terrain. Note that the
  current site map is deliberately **tile-free** (`web/src/map.tsx`) so that an
  air-gapped install has no external dependency; MapLibre would be an enhancement for
  installs that can reach a tile server, not a replacement for it.
- **TanStack Virtual** — virtualized log, event and resource lists.

**Licences must be checked against `deny.toml` before any of these is added.** That has
already decided one architectural question in this product (no TLS, because both rustls
crypto providers carry OpenSSL-licensed code) and it will decide some of these.

---

## 12. What not to do

No 3D for CPU charts, log tables, alert lists, metrics, ordinary dashboards, settings or
forms. It looks impressive for a day and hurts readability forever.

---

## 13. Before any of this is written

A **UI/UX specification** covering: navigation · design system · colour and token system
· typography · default dashboard · dashboard builder · widget specification · 2D topology
· 3D topology · resource detail · investigation workspace · logs · metrics ·
alerts and incidents · NOC mode · command palette · responsive behaviour · animation
rules · accessibility · component architecture · package structure · state management ·
websocket and live-update behaviour · performance budgets · and the implementation
sequence.

That document comes first. Without it every page invents its own pattern, and the product
ends up looking like six products.

---

## 14. Sequencing against the backend

Nothing here is reachable before the data is. In dependency order:

| Needs | Before it can exist |
|---|---|
| Log Explorer, live tail, "all signals for this resource" | M3 — in progress |
| Dashboard builder, widgets, alert and incident lists | M4 |
| 2D topology, semantic zoom, dependency highlighting | M6 — the edges exist, the layout does not |
| Animated links, traffic on the graph | M7 flows |
| 3D topology, time travel | after M6, and only once 2D is good |
| Investigation Workspace, cross-filtering | M9 correlation, though the M3 "all signals" interaction is its seed |
| Geographic map with real coordinates | **exists** — `site_location`, `place_site`, `site_overview`, `web/src/map.tsx` |

The geographic layer the user asked about is the one piece of this that is already
built: sites carry latitude and longitude, the API returns a status rollup per site, and
the map renders without a tile server. Extending it to per-resource coordinates, a globe
and traffic arcs is an enhancement of something real rather than a new subsystem.
