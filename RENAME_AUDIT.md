# Rename audit — Aegisora → Veyronis

Phase 1 audit, and the decision taken from it.

Last updated: 2026-09-16 · **Decided: product name is `Veyronis`; code keeps the
codename `uops` until clearance.**

---

## The headline, before the tables

**`Aegisora` was already rejected in this repository, for the reason you found.**
PLAN.md §1 and README.md §Name both say so, and they name `aegisora-ai/aegisora`
specifically. The decision was made when the naming section was written and the codebase
has never used the name for anything.

That is why the migration you scoped is much smaller than it looks. The product name was
deliberately kept out of every identifier:

| Surface | Identifier used | Contains "Aegisora"? |
|---|---|---|
| Rust crates (16) | `uops-*` | No |
| Binaries | `uops-server`, `uops-poller`, `uops-ch-migrate`, `uops-pg-migrate` | No |
| Environment variables (18) | `UOPS_*` | No |
| Docker images / containers / services | `uops:dev`, `uops-server`, `uops-dev-pg`, … | No |
| PostgreSQL database / role | `uops` | No |
| ClickHouse database / user | `uops` | No |
| NATS subjects | `uops.*` | No |
| npm package | `uops-web` | No |
| Migration identifiers | numeric (`0001_…`) | No |
| CI workflows | `uops-*` | No |
| SQL schema, tables, columns | domain names (`resource`, `logs`, …) | No |

**Total occurrences of `aegisora` in the tracked tree: 8, across 4 files.** Three of them
are the URL of the GitHub remote; three are prose explaining that the name was rejected;
two are the clone command.

Phases 4–10 and 14–15 of your migration prompt therefore have **no work in them**. There
are no crate names to rename, no env vars to alias, no database identifiers at risk, no
Docker volumes to migrate, no OTel `service.name` to change, no test fixtures asserting a
product name, and no persistent state that a rename could destroy. The rename is a repo
rename and eight lines of text.

---

## Phase 1 — the complete inventory

Every occurrence, with a recommendation.

| # | File | Line | Existing text | Type | Rename? | Target | Risk | Note |
|---|---|---|---|---|---|---|---|---|
| 1 | `Cargo.toml` | 14 | `repository = "https://github.com/Ratul-netizen/aegisora"` | workspace metadata | **Yes, with the GitHub rename** | `…/veyronis` | None | Not published to crates.io; nothing resolves this URL at build time. Left pointing at the repository that exists until it is renamed |
| 2 | `PLAN.md` | 116 | `` `aegisora-ai/aegisora` is an active GitHub org … `` | prose, historical | **No — keep** | — | None | This is the *record of the decision*. Deleting it loses why the name was rejected, and somebody proposes it again in six months |
| 3 | `PLAN.md` | 117 | `**Do not use Aegisora.**` | prose, historical | **No — keep** | — | None | Same |
| 4 | `README.md` | 121–122 | `` `Aegisora` was rejected — `aegisora-ai/aegisora` … `` | prose, historical | **Rewritten** ✅ | see §Naming policy | None | Rejection kept, replacement added |
| 5 | `STATUS.md` | 3 | ``repo: `github.com/Ratul-netizen/aegisora` `` | doc header | **Yes, with the GitHub rename** | `…/veyronis` | None | Same |
| 6 | `STATUS.md` | 73 | `git clone https://github.com/Ratul-netizen/aegisora && cd aegisora` | doc command | **Yes, with the GitHub rename** | `…/veyronis && cd veyronis` | None | A clone command is *executed*. Changing it before the rename makes the documented first step fail |
| 7 | `STATUS.md` | 636 | Open-items row: "Repo is still named `aegisora`" | doc table | **Rewritten** ✅ | — | None | Moved from Open items to Decided |
| 8 | `.git/config` | — | `origin` remote | git config | **Yes, after the GitHub rename** | `…/veyronis.git` | Low | GitHub keeps a redirect, so the old URL keeps working — low rather than none |

Untracked and out of scope: `target/`, `target-linux/`, `.git/` object contents, and the
local directory name `Downloads/Compressed/aegisora` (cosmetic, and renaming a
checked-out directory is a local action, not a repository change).

---

## The decision, taken

**Do the crates become `veyronis-*`? — Not now. At clearance.** Decided 2026-09-16.

The original migration brief said they should. I argued against it; that is the
recommendation below, and it is what was chosen.

`uops` was chosen as a codename *specifically so that the product name could change
without touching code* — PLAN.md §1: *"Renaming a local workspace is a `sed`; renaming a
published crate is not."* That bet just paid off: the name changed and the cost was eight
lines.

Renaming 16 crates, 18 environment variables, the PostgreSQL role and database, the
ClickHouse database, every Docker image and container name, the NATS subject prefix and
822 source references would re-couple the code to a name that, by your own words, has had
*"a preliminary naming check, not a trademark/domain/legal clearance."* If clearance comes
back bad — an existing mark in class 9/42, a taken `.com`, a taken crates.io name — we pay
that cost twice. And unlike the docs, this rename **does** touch persistent state: the
database name and role, the Docker volumes, and `UOPS_KEK_FILE`, which points at the key
that decrypts every stored credential.

What I would do:

1. **Now**: rename the repository and the eight documentation references. Positioning
   becomes Veyronis. Zero risk, and it settles the brand question today.
2. **On clearance** (GitHub org + crates.io + npm + `.com`/`.io` + USPTO + RJSC): rename
   the crates, env vars and infrastructure identifiers in one commit, with a documented
   database and volume migration. That is the moment the cost is justified, because it is
   also the moment the crates can be published.

**What was applied.** Step 1, minus the repository rename, which I cannot perform:

- `PLAN.md` §1 — Veyronis is the product name; the `uops` codename and why it stays.
- `README.md` §Name — the same, from the reader's side.
- `STATUS.md` — moved out of Open items into Decided, with the reasoning.
- This file.
- **Not changed:** the four `github.com/Ratul-netizen/aegisora` URLs, in `Cargo.toml`
  line 14 and `STATUS.md` lines 3 and 73. They point at the repository that actually
  exists. They change in the same commit as the GitHub rename — documentation naming a
  URL that does not resolve is worse than documentation one rename behind.
- **Not changed:** the crates, binaries, `UOPS_*` variables, databases, Docker images,
  NATS subjects and npm package. That is the decision.

---

## Naming policy (for whenever it is applied)

| Slot | Value |
|---|---|
| Product | `Veyronis` |
| Lowercase / CLI | `veyronis` |
| Positioning | Unified Infrastructure Observability & Operations Platform |
| Repository | `Ratul-netizen/veyronis` |
| Crates (deferred) | `veyronis-*` |
| npm (deferred) | `@veyronis/*` |
| Docker (deferred) | `veyronis/<service>` |
| Codename in use until clearance | `uops` — **decided 2026-09-16** |

No variants — not `Veyron`, `VeyronOS`, `Veyronis AI` or `Veyronis NMS`.

**Clearance is not done.** Your search was preliminary and so is this audit. Before any
crate or package is published under the name, someone needs: crates.io, npm, the GitHub
org, `.com`/`.io`, a USPTO TESS search in classes 9 and 42, and Bangladesh RJSC if
incorporating locally. That list is unchanged from PLAN.md §1; only the candidate name
changed.

---

## The GitHub rename — done

`Ratul-netizen/aegisora` → **`Ratul-netizen/veyronis`**, renamed by hand on 2026-09-16.
`gh` is not installed on this machine and the operation needs repo-admin credentials, so
it was not something I could do.

What followed in this repository, once it was done:

- `Cargo.toml` line 14 and `STATUS.md` lines 3 and 73 now name the new URL.
- `origin` re-pointed with `git remote set-url`, verified by a `git fetch` that succeeds
  against the new URL.

GitHub keeps a redirect from the old name, so an existing clone and the old URL both keep
working; nothing is required of anyone who has already cloned.

Still outstanding, and also needing repo-admin: the repository **description** and
**topics**. Suggested description: `Veyronis — Unified Infrastructure Observability &
Operations Platform`.

---

## What this audit deliberately did not do

- Did not modify anything during Phase 1; the documentation edits above came after the
  decision.
- Did not touch `PLAN.md`'s or `README.md`'s record of *why* Aegisora was rejected.
- Did not rename crates, env vars, database identifiers or Docker volumes — see the
  decision section above.
- Did not rewrite git history or force-push anything.
