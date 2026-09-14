-- 0003 — the Explorer histogram. W1 FIX 2.
--
-- A different root cause from 0002, and neither fix substitutes for the other. The
-- histogram ran at 1 066 ms, and the p_by_time projection does NOT help it: rows read
-- barely moved (33.47M → 33.39M), because a histogram over the retention window touches
-- every row whatever the sort order. Only pre-aggregation helps.
--
-- This one matters more than the tail: it re-renders on every search and every filter
-- change, so its latency is felt per keystroke rather than per page load.
--
-- `uops_query::plan` routes count-only bucket queries here automatically. The columns
-- below are exactly what that planner is allowed to reference; adding a column here
-- without teaching the planner about it just wastes space.

CREATE TABLE IF NOT EXISTS logs_counts_5m
(
    tenant_id   UUID,
    resource_id UUID,
    severity    LowCardinality(String),
    bucket      DateTime('UTC'),
    cnt         AggregateFunction(count, UInt64)
)
ENGINE = AggregatingMergeTree
PARTITION BY toYYYYMM(bucket)
-- Ordered by bucket before resource: this table is read time-first, which is the
-- opposite of how the base table is read. That is the whole point of it existing.
ORDER BY (tenant_id, bucket, severity, resource_id)
TTL bucket + INTERVAL 365 DAY DELETE;

CREATE MATERIALIZED VIEW IF NOT EXISTS logs_counts_5m_mv TO logs_counts_5m AS
SELECT
    tenant_id,
    resource_id,
    severity,
    toStartOfFiveMinute(observed_at) AS bucket,
    countState() AS cnt
FROM logs
GROUP BY tenant_id, resource_id, severity, bucket;
