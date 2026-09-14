-- 0001 — the logs table. SPEC §M0.6, amended by W1.
--
-- The sort key is the product decision, not a tuning choice. W1 measured Q05 ("all
-- signals for one resource in a window") at 9 ms reading 16 380 rows at BOTH 10M and
-- 100M rows: resource-scoped investigation is independent of table size. Changing
-- ORDER BY here is a full re-ingest, not a migration.
--
-- Every table in this directory uses the SAME sort key for the same reason.

CREATE TABLE IF NOT EXISTS logs
(
    tenant_id     UUID,
    resource_id   UUID,
    site_id       UUID,
    observed_at   DateTime64(3, 'UTC'),
    -- Both timestamps are required. The difference is ingest lag, and without it there
    -- is no way to tell a quiet network from a broken collector.
    ingested_at   DateTime64(3, 'UTC'),
    source_kind   LowCardinality(String),
    source_vendor LowCardinality(String),
    severity      Enum8('trace'=1,'debug'=2,'info'=3,'notice'=4,'warn'=5,
                        'error'=6,'critical'=7,'alert'=8,'emergency'=9),
    facility      UInt8,
    body          String,
    attributes    Map(LowCardinality(String), String),
    trace_id      String,
    span_id       String,

    -- W1's most expensive finding, in two lines. The slowest query in the whole suite
    -- was not a text search — it was GROUP BY attributes['host.name'] at 2 252 ms,
    -- because a Map column decompresses in full for every row. These two keys are
    -- grouped on constantly, so they are real columns.
    --
    -- `uops_query::plan` rewrites the matching Attr fields onto these names. The list
    -- there and the list here must stay in step; uops_core::attr::semconv::MATERIALIZED
    -- is the shared record of which keys are involved.
    host_name     String                 MATERIALIZED attributes['host.name'],
    service_name  LowCardinality(String) MATERIALIZED attributes['service.name'],

    -- Text search. W1: a selective token read 8 190 rows at both 10M and 100M — the
    -- index works. It also cost 71% of compressed data at 100M, and the overhead GREW
    -- with scale rather than amortising, which is why per-source opt-out is required
    -- (M2 monitoring profiles) rather than optional.
    INDEX idx_body body TYPE text(tokenizer = 'splitByNonAlpha') GRANULARITY 1,
    INDEX idx_sev  severity TYPE set(16) GRANULARITY 4
)
ENGINE = MergeTree
PARTITION BY toYYYYMMDD(observed_at)
ORDER BY (tenant_id, resource_id, observed_at)
-- Plain DELETE, no volumes. SPEC §M0.6 shows `TO VOLUME 'warm'`/'cold' against a
-- `tiered` storage policy, which does not exist on a default install — a deployment
-- that has not configured one would fail this migration outright. Tiering arrives with
-- the deployment profiles that configure the policy; see STATUS.md open items.
--
-- Retention is changed by a LATER migration (ALTER TABLE ... MODIFY TTL), never by
-- editing this file: an edited applied migration is a checksum mismatch, which the
-- runner refuses on purpose.
TTL toDateTime(observed_at) + INTERVAL 365 DAY DELETE
SETTINGS index_granularity = 8192;
