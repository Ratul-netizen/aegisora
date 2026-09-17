# UI/UX specification — part 1: the foundation and the first screen

Status: **specification.** [`UI.md`](./UI.md) §13 requires this document before the
cockpit UI is written — *"without it every page invents its own pattern, and the product
ends up looking like six products."* This is the part of it that the **Operations
Overview** needs, and no more.

| | |
|---|---|
| **Governs** | design tokens · colour semantics · typography · density and the layout grid · the widget contract · live update · animation · accessibility |
| **Defers** | 2D and 3D topology, the investigation workspace, NOC mode, the command palette, incidents, automation, state management beyond what exists, the package split — each one is specified when it is the next thing built |
| **Supersedes** | nothing. `UI.md` stays the direction; where this document is more specific, this document is what gets built |
| **Applies to** | every page, including the six that already exist — see *Migration* at the end |

Written 2026-09-17, against the shell, Explorer, alerts and dashboards as built.

---

## 0. The three rules everything here answers to

From `UI.md`, restated as the tests this specification can be checked against:

1. **2D carries information; 3D carries spatial relationships.** No chart, table, list,
   form or number is 3D. Ever.
2. **Colour is semantic, never decoration.** If a colour means something, it means the
   same thing on every screen; if it means nothing, it is not a colour, it is a grey.
3. **The UI is a layer over the architecture, not a second architecture.** A screen shows
   a `Query` AST's answer, a `resource_id`, a `TenantScope`. It does not invent a second
   model of what a device is.

And one rule this document adds, because it is the one the existing code has already
been following and it should be written down:

4. **Nothing is conveyed by hue alone.** Every state carries a word or a glyph as well.
   Roughly one man in twelve cannot read a red-green distinction, and a NOC wall is read
   from four metres.

---

## 1. Tokens

Every colour, radius and dimension is a custom property on `:root`. There are no colour
literals anywhere else — `web/src/styles.css` already holds this line and it is now a
rule rather than a habit.

### 1.1 Ground and surfaces

`UI.md` §9 asks for "very dark blue / charcoal" ground and near-black cards. The built
palette is charcoal-neutral; this specification moves it onto the blue axis it asks for,
which is a change of about eight points of hue and nothing else.

| Token | Dark | Light | What it is |
|---|---|---|---|
| `--bg` | `#0d1017` | `#fbfbfc` | the ground; the page behind everything |
| `--bg-raised` | `#161a22` | `#ffffff` | cards, panels, popovers — one step up |
| `--bg-sunken` | `#090b10` | `#f2f3f5` | wells: table headers, inputs, the gauge track |
| `--border` | `#252b36` | `#dfe1e6` | one-pixel separation, never a box for its own sake |
| `--text` | `#e6e8ec` | `#16181d` | body |
| `--text-dim` | `#9aa1ae` | `#5c6270` | labels, units, timestamps, anything secondary |

Dark is the default and the one designed first: a NOC wall runs dark for eight hours.
Light exists because the screenshot that goes into a customer's monthly report runs
light, and retrofitting it later is a rewrite of every colour decision.

### 1.2 Semantic state

These five are the product's vocabulary. A screen may not introduce a sixth, and none of
them may be used decoratively.

| Token | Dark | Light | Means | Word used with it |
|---|---|---|---|---|
| `--ok` | `#57d08a` | `#1a7f4b` | up, healthy, resolved, delivered | `up`, `ok`, `resolved` |
| `--warn` | `#e0b054` | `#9a6400` | degraded, pending, rate-limited, slow | `warning`, `pending`, `degraded` |
| `--danger` | `#ff7b7b` | `#b4232b` | down, critical, firing, failed | `critical`, `down`, `firing` |
| `--unknown` | `#8b93a1` | `#6b7280` | never reported, no data, stale | `unknown`, `no data` |
| `--maintenance` | `#9d7bff` | `#6d4ad6` | suppressed by a maintenance window | `maintenance` |

`--unknown` and `--maintenance` are new: the backend has had both states since migration
0002 and 0012 and the UI has had no way to say either. "No data" drawn as zero and
"suppressed" drawn as healthy are the two most consequential lies a monitoring UI can
tell.

### 1.3 Accent

One family, used sparingly — `UI.md` §9. It marks *where you are* and *what you can
press*, and it never marks state.

| Token | Dark | Light |
|---|---|---|
| `--accent` | `#6d9bff` | `#2c5fd6` |
| `--accent-text` | `#0e1013` | `#ffffff` |
| `--accent-dim` | `#6d9bff26` | `#2c5fd614` | a wash behind an active row or tab |

### 1.4 Series colours

Charts need a categorical palette that is *not* the semantic one, or a line will look
like a status. Six, because a panel with more than six lines is one whose grouping is too
fine to read, and after six they repeat — a visible signal that this has happened.

```
--series-1  #6d9bff   --series-2  #57d08a   --series-3  #e0b054
--series-4  #ff7b7b   --series-5  #a78bfa   --series-6  #2dd4bf
```

They are ordered for distinguishability under the common forms of colour blindness: blue
and amber lead, and the red sits fourth rather than second.

### 1.5 Shape, space and motion

```
--radius      6px     --radius-lg   10px    (cards, popovers)
--space       4px     the unit; every margin and gap is a multiple
--sidebar     224px   --sidebar-collapsed  56px
--header      52px
--row         28px    one table row at default density
--tap         32px    the smallest interactive target
--motion      120ms   the only transition duration
--motion-slow 240ms   panels sliding in, and nothing else
```

One duration, because a product with five easing curves reads as five products. Easing is
`ease-out` on entry and `linear` on anything that repeats.

---

## 2. Typography

| Role | Family | Size | Weight | Use |
|---|---|---|---|---|
| Display | `--font` | 22px | 600 | one per page, the page's name |
| Section | `--font` | 15px | 600 | panel titles, group headings |
| Body | `--font` | 14px | 400 | everything |
| Label | `--font` | 12px | 500 | form labels, column headers, units |
| Stat | `--font` | 34px | 600 | the one number on a stat panel |
| Mono | `--mono` | 13px | 400 | see below |

`--font` is the system stack today. `UI.md` §9 names Inter, Geist or IBM Plex Sans; any of
them is a self-hosted web font, which is a download, a licence and an air-gap question, so
it is a deliberate later decision and not a default. The system stack is not a placeholder
— it is what ships until that decision is made.

**Mono is not a style choice, it is a type.** Anything the operator may need to compare
character by character is mono: IP and MAC addresses, timestamps, log bodies, metric
values, identifiers, query text, dedup keys, hostnames in a table. Prose is never mono.

Numbers in tables and stats use `font-variant-numeric: tabular-nums`, so a column of
values does not shift as it updates.

---

## 3. Density and the layout grid

An operations console is read at a glance by somebody who already knows what they are
looking for. It is dense.

- **Twelve columns**, `--space * 3` gutters. Twelve divides by 2, 3, 4 and 6, which is
  every layout anybody asks for. The dashboard grid already works this way.
- **Row height `--row`** at default density, `--row + 8px` at *comfortable*, which is a
  per-user preference and the only density control.
- **Panels are cards**: `--bg-raised`, one-pixel `--border`, `--radius`, 10px 12px
  padding, a header row with the title left and controls right.
- **Below 70rem the grid collapses to one column.** A dashboard read on a phone is a
  list; two columns at that width is two unreadable columns.

### 3.1 The page frame

```
┌────────────────────────────────────────────────────────────────┐
│ header: brand · context · time range · live · alerts · user    │  --header
├──────────┬─────────────────────────────────────────────────────┤
│ sidebar  │ content                                             │
│ --sidebar│   h1 + one line of context                          │
│          │   page body                                         │
└──────────┴─────────────────────────────────────────────────────┘
```

The sidebar is grouped as `UI.md` §1 draws it — Overview, NETWORK, OBSERVABILITY,
OPERATIONS, DASHBOARDS, ADMIN — and collapses to icons at `--sidebar-collapsed`. Items
for pages that do not exist yet are **not shown**: a menu of links to empty pages teaches
an operator the product does not work, which is the same argument §3 makes about empty
panels.

---

## 4. The widget contract

Every panel on every screen — the default dashboard's and a user's — obeys one contract.
This is what stops the built-in dashboards and the builder from being two products.

A widget is:

```ts
{ id, title, query?: Query, viz: Viz, width, height }
```

and it has exactly five states, each of which must be distinguishable at four metres:

| State | What it shows |
|---|---|
| **loading** | the title, and a quiet placeholder. Never a spinner per panel — twenty spinners is a broken page |
| **empty** | "No data in this window." — and *never* a zero |
| **data** | the visualization |
| **error** | the server's own sentence, in the panel's corner, with the rest of the page alive |
| **stale** | data plus a dimmed timestamp: this is what the panel last knew, and it is older than the window claims |

Rules that follow from the backend and are not negotiable in a panel:

1. **The window comes from the header**, always. A panel stores a window as provenance
   and substitutes the current one on read.
2. **The bucket follows the window.** A panel saved at five-minute buckets, viewed over a
   month, asks for wide buckets — otherwise it silently truncates and looks complete.
3. **Null is not zero.** `avg()` over an empty bucket is null; a chart that draws it as
   zero shows an outage as a collapse to the floor.
4. **A panel is one query.** Twenty panels are twenty requests. One slow panel spins
   alone; one failing panel fails alone.

### 4.1 The shipped visualizations

Five, as SPEC §M4 names, and no more until one is needed: **time series**, **single
stat**, **table**, **gauge**, **alert list**. Not heatmap, pie, geo or topology — two are
choices this product does not need and two are §6's, not a panel's.

Charts are **SVG, hand-drawn, no chart library**, as the Explorer histogram, the site map
and the dashboard panels already are. `UI.md` §11 proposes ECharts; that remains open, and
the bar it has to clear is stated here: a chart library enters this product only when a
visualization is needed that is genuinely hard by hand — and it must clear `deny.toml`
first. A line, a bar, a gauge and a sparkline are a scale, a path and two axes.

---

## 5. Live update

The product has three refresh behaviours and they are not the same thing. Getting this
wrong is either a stale console or a bill.

| Surface | Behaviour | Why |
|---|---|---|
| **Alert list, header alert count** | poll every 10s | small indexed reads of the control plane; this is the screen whose whole purpose is to be current |
| **Explorer** | never, unless following | it runs a query somebody typed over a window somebody chose; a timer on it bills the customer for the page being open |
| **Live tail** | poll every 2s, half-open on `ingested_at` | a stream, and the watermark is the server's |
| **Dashboard panels** | on window change; `staleTime` 60s | a wall display is the case; the time range is what changes them |

**The live indicator in the header is a fact, not a decoration.** It is lit only while
something on the page is actually refreshing on a timer, and it carries the word `LIVE`.

No websockets yet. Nothing on these four surfaces needs sub-second push, and a held-open
connection is the thing on-premise proxies close at sixty seconds — the same argument the
live tail already made. When incidents or topology need push, that is when the transport
decision gets made, and it gets written down here.

---

## 6. Animation

Animation means **state change**, and nothing else. `UI.md` §9's micro-interactions,
specified:

| Event | What moves | Duration |
|---|---|---|
| a resource goes down | its status dot pulses twice, then rests | `--motion-slow` ×2 |
| a new alert arrives | the row fades in from `--accent-dim` | `--motion` |
| a panel loads | opacity only, no movement | `--motion` |
| a drilldown opens | slides from the right | `--motion-slow` |
| a value updates | the number changes; nothing animates | — |

Never: rotation, bouncing, parallax, anything looping that does not represent a live
signal, or a full-screen colour change. A screen that flashes red is a screen somebody
turns off.

`prefers-reduced-motion: reduce` removes every one of these. The pulse becomes a static
ring, the slide becomes an appearance. Nothing in the table above is the *only* carrier of
its information, so removing it loses nothing — which is the test for whether an animation
was decoration.

---

## 7. Accessibility

Not a section to revisit later; each item is checkable on every page.

- **Colour plus a word or glyph, always.** A severity is a coloured pill *with the word in
  it*. A status dot has a label beside it.
- **Contrast**: body text ≥ 4.5:1 against its surface, large text and glyphs ≥ 3:1. The
  dark tokens above are chosen to clear this; a new colour must be checked, not assumed.
- **Keyboard**: every interactive element reachable by Tab in visual order, a visible
  focus ring (`--accent`, 2px, never removed), Escape closes any overlay, Enter activates.
- **Screen readers**: every icon-only control has a label; tables have real `<th>`; a
  live region announces "N alerts firing" when that count changes, and nothing else —
  announcing every metric update makes the page unusable.
- **Scalable text**: the layout survives 200% browser zoom and a 16px minimum body size
  preference. No `px` line heights that clip.
- **NOC distance mode**: a per-user setting that raises body to 16px, `--row` to 36px and
  stat to 44px. Not a separate stylesheet — the same tokens with different values.

---

## 8. The Operations Overview

`UI.md` §3's landing page, specified against what the backend can answer **today**.
Panels whose data does not exist yet are not drawn as empty boxes — they are absent, and
the sequencing table says when they arrive.

```
┌───────────────────────────────────────────────────────────────────────┐
│ OPERATIONS OVERVIEW                          Last 30 min ▾   LIVE ●   │
│ Default tenant · all sites                                            │
├───────────┬───────────┬───────────┬───────────────────────────────────┤
│ RESOURCES │  FIRING   │  PENDING  │  REPORTING                        │
│    248    │     5     │     3     │   231 / 248                       │
├───────────┴───────────┴───────────┴───────────────────────────────────┤
│  WHAT IS FIRING                                                       │
│  ● critical  rtr-01   CPU above 90%              4m   [take]          │
│  ● warning   rtr-04   Interface errors           1m   [take]          │
├─────────────────────────────────┬─────────────────────────────────────┤
│  LOG VOLUME BY SEVERITY         │  BUSIEST RESOURCES                  │
│  (stacked bars, 30 min)         │  (name · errors · last seen)        │
└─────────────────────────────────┴─────────────────────────────────────┘
```

### 8.1 Every number on it, and where it comes from

| Tile | Source | Notes |
|---|---|---|
| Resources | `GET /api/v1/resources` count | excludes decommissioned |
| Firing / Pending | `GET /api/v1/alerts` | the two states are separate tiles because one has woken somebody and the other has not |
| Reporting | resources with telemetry in the window ÷ total | the honest availability number this product can compute today. **Not** called "availability": that implies an SLA calculation with maintenance windows excluded, which is M9 |
| What is firing | `GET /api/v1/alerts`, ordered firing-then-pending | the same component as the alerts page; `take` acknowledges in place |
| Log volume | one `Query`: count by `time_bucket` and `severity` | stacked bars, semantic colours, `--unknown` for unparsed |
| Busiest resources | one `Query`: count by `resource_id`, severity ≥ error | links to the resource page |

Six panels, four queries, one control-plane read. Under the twenty-panel budget measured
at p95 0.42s, with room for the topology and flow panels §3 wants when M6 and M7 land.

### 8.2 What it must not do

- **No empty states pretending to be data.** Zero firing alerts is "Nothing is firing",
  not a `0` in a red tile.
- **No health percentage invented from nothing.** §3's mock shows a 97.4% health ring;
  there is no defensible formula for it yet, so it is not drawn. A number nobody can
  explain is worse than an absent one on the screen an operator trusts first.
- **No topology panel until M6.** A placeholder that says "topology coming soon" on the
  landing page is the product telling the operator it is unfinished, every time they open
  it.

---

## 9. Migration: the six pages that already exist

This specification is retrospective for the shell, resources, map, Explorer, alerts and
dashboards. They mostly comply — the tokens, the twelve-column grid, the panel card, the
five widget states and "colour plus a word" all came from building them. Where they do
not:

| Gap | Where | Fix |
|---|---|---|
| ground is charcoal, not blue-dark | `styles.css` `:root` | §1.1 values |
| no `--unknown` or `--maintenance` | everywhere | add; then use them where the backend already reports those states |
| series colours are ad hoc, partly semantic | `panels.tsx` `LINE_COLOURS` | §1.4 |
| no `--space` scale; margins are hand-picked | `styles.css` | §1.5, mechanical |
| sidebar is flat, not grouped | `layout.tsx` | §3.1, when the group has two items in it |
| no focus-ring rule | `styles.css` | §7 |
| no NOC distance mode | — | §7, a later setting |

None of these is a rewrite. They are the difference between six pages that look similar
because one person wrote them in a week and six pages that look the same because they are
built from one set of decisions.

---

## 10. What this document does not cover

Named so that the next person knows the gap is deliberate: 2D and 3D topology · the
investigation workspace · incidents and their timeline · NOC mode beyond the distance
setting · the command palette · automation · flows and geography · the component and
package architecture · state management beyond TanStack Query as used · websocket and
push · per-page performance budgets beyond the dashboard's measured one.

Each is specified when it is the next thing built, in a part 2 of this document — not
guessed at now, because a specification written a milestone early is a specification that
gets ignored.
