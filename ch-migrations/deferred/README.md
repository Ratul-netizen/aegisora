# Deferred DDL

Declared now, created in the milestone that needs them. SPEC §M0.6: traces and flows
"are declared in `ch-migrations/` but not created until M7/M8".

The runner ignores this directory — it only reads `NNNN_*.sql` at the top level. These
files become numbered migrations when the collectors that fill them are built.

Declaring them now costs an hour and forces the envelope (`uops-core`) and the Query AST
(`uops-query`) to stay general enough that adding a signal later is additive rather than
a redesign. `SignalType::Trace` and `SignalType::Flow` already exist in the AST and
compile to a clear "not implemented until M7/M8" error rather than to nothing.
