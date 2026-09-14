-- 0005 — the hourly rollup: raw 30d → 5m for 1y → 1h for 3y.
--
-- Closes the open item left by `uops-query`, which already plans onto this table for
-- any aggregate window longer than 30 days. Until this migration existed, a 90-day
-- graph compiled to SQL naming a table that was not there.

CREATE TABLE IF NOT EXISTS metrics_1h
(
    tenant_id   UUID,
    resource_id UUID,
    metric      LowCardinality(String),
    bucket      DateTime('UTC'),
    min_v       AggregateFunction(min,   Float64),
    max_v       AggregateFunction(max,   Float64),
    avg_v       AggregateFunction(avg,   Float64),
    cnt         AggregateFunction(count, UInt64)
)
ENGINE = AggregatingMergeTree
PARTITION BY toYYYYMM(bucket)
ORDER BY (tenant_id, resource_id, metric, bucket)
TTL bucket + INTERVAL 1095 DAY DELETE;

-- Chained off metrics_5m rather than off metrics: a materialised view fires on inserts
-- made by another materialised view, so the hourly buckets are built by re-aggregating
-- 12 five-minute states instead of re-reading every raw point. The -MergeState
-- combinator is what makes that legal — it merges existing states and emits a new one,
-- where plain minState() would try to treat a state as a value.
--
-- The averages stay correct because avgState carries its own count: an average of
-- twelve five-minute averages weighted by their counts is the true hourly average, not
-- an average of averages.
CREATE MATERIALIZED VIEW IF NOT EXISTS metrics_1h_mv TO metrics_1h AS
SELECT
    tenant_id,
    resource_id,
    metric,
    toStartOfHour(bucket) AS bucket,
    minMergeState(min_v)   AS min_v,
    maxMergeState(max_v)   AS max_v,
    avgMergeState(avg_v)   AS avg_v,
    countMergeState(cnt)   AS cnt
FROM metrics_5m
GROUP BY tenant_id, resource_id, metric, bucket;
