# Contributing

## Contributor License Agreement — required before any code is merged

**Every contributor must sign the [CLA](./CLA.md) before their first pull request is
merged.** There are no exceptions, including for small fixes.

This is not bureaucracy, and it is worth explaining rather than just asserting.

The project is licensed **AGPL-3.0-only**. AGPL protects against someone taking this
work and running it as a competing hosted service. But a large share of the intended
buyers — government, defence, law enforcement and regulated enterprises deploying
on-premise — operate legal policies that block AGPL software outright, sometimes even
for purely internal use.

The standard resolution is **dual licensing**: AGPL for everyone, and a commercial
license for buyers whose legal teams cannot accept AGPL. Offering a commercial license
requires the right to license *all* of the code that way.

**That right cannot be reclaimed after the fact.** Once a contribution lands without a
CLA, relicensing any part of the project needs that contributor's individual consent,
forever — and in practice contributors become unreachable, change employers, or simply
decline. A single unsigned contribution can permanently foreclose the commercial
licensing path, and with it the ability to sell to a whole class of buyer.

So the CLA gate is strict *because* the cost of getting it wrong is irreversible,
while the cost of signing is a few minutes.

> **This is not legal advice.** [CLA.md](./CLA.md) is a working draft modelled on the
> widely-used Apache Individual CLA. Have a lawyer review it before relying on it
> commercially, and before accepting contributions under it.

## Before you open a pull request

1. `cargo fmt --all`
2. `cargo clippy --workspace --all-targets -- -D warnings`
3. `cargo test --workspace --all-targets && cargo test --workspace --doc`

The doctests matter more than usual here: the `compile_fail` examples on `Secret<T>`
and `TenantScope` are what enforce the security invariants (SPEC §M0.4, §M0.8). They
only run as doctests on public items, so do not move them into `#[cfg(test)]` modules —
rustdoc will silently stop collecting them and they will pass without compiling
anything.

## Invariants that are not negotiable

These come from [SPEC.md](./SPEC.md) §M0 and are cheap now, expensive or impossible
later. A pull request that breaks one will be asked to change, however good it
otherwise is.

| | |
|---|---|
| `tenant_id` on every row | In both databases, including lookup tables |
| `TenantScope` to build any query | A missing tenant filter must be a compile error, not a review catch |
| `resource_id` on every telemetry row | With the ClickHouse sort key `(tenant_id, resource_id, observed_at)` |
| Both `observed_at` and `ingested_at` | The difference is ingest lag; without it you cannot tell a quiet network from a broken collector |
| Credentials only through `SecretStore` | Never a plaintext column, never in an envelope, never in a log |
| Crypto only through `AeadProvider` | Selected at build time — validation attaches to a binary, not a code path |
| No egress in the `onprem` profile | No update check, no crash reporting, no license callback |
| Audit every mutation, and every credential read | Defence buyers audit who *saw* what, not only who changed it |

## Commit messages

Explain **why**, not what — the diff already shows what. Where a decision was driven by
a measurement, cite the number. `bench/results/FINDINGS.md` is the model: it records
that the Investigation Workspace query reads 16,380 rows at both 10M and 100M, which is
the evidence for a sort key that would otherwise look arbitrary.
