-- 0004 — metrics, and the 5-minute rollup.
--
-- Raw points live 30 days; the rollups carry the long tail. `uops_query::plan` picks
-- raw / 5m / 1h from the query's time span, so a caller never names one of these
-- tables — which is also why the rollup's column list is a contract: the planner will
-- only emit avg/min/max/count against it, because those are the only states stored.

CREATE TABLE IF NOT EXISTS metrics
(
    tenant_id   UUID,
    resource_id UUID,
    site_id     UUID,
    metric      LowCardinality(String),
    observed_at DateTime64(3, 'UTC'),
    ingested_at DateTime64(3, 'UTC'),
    -- Counters are stored RAW. Rates are computed at query time: storing a
    -- pre-computed rate makes re-interpretation impossible and a counter wrap
    -- unrecoverable.
    value       Float64,
    unit        LowCardinality(String),
    labels      Map(LowCardinality(String), String)
)
ENGINE = MergeTree
PARTITION BY toYYYYMM(observed_at)
-- `metric` sits between resource and time: a metric query is almost always
-- "this series for this resource", and putting it after observed_at would scatter one
-- series across every granule in the window.
ORDER BY (tenant_id, resource_id, metric, observed_at)
TTL toDateTime(observed_at) + INTERVAL 30 DAY DELETE;

CREATE TABLE IF NOT EXISTS metrics_5m
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
TTL bucket + INTERVAL 365 DAY DELETE;

-- Labels are deliberately NOT carried into the rollup. Dropping them is most of why it
-- stays small enough to keep for a year, and `uops_query::plan` refuses a rollup query
-- that groups or filters on one rather than silently returning the wrong series.
CREATE MATERIALIZED VIEW IF NOT EXISTS metrics_5m_mv TO metrics_5m AS
SELECT
    tenant_id,
    resource_id,
    metric,
    toStartOfFiveMinute(observed_at) AS bucket,
    minState(value)   AS min_v,
    maxState(value)   AS max_v,
    avgState(value)   AS avg_v,
    countState()      AS cnt
FROM metrics
GROUP BY tenant_id, resource_id, metric, bucket;
