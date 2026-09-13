-- W1 benchmark: metrics + 5m rollup. Mirrors SPEC.md §M0.6.

DROP TABLE IF EXISTS bench.metrics_5m_mv;
DROP TABLE IF EXISTS bench.metrics_5m;
DROP TABLE IF EXISTS bench.metrics;

CREATE TABLE bench.metrics
(
    tenant_id   UUID,
    resource_id UUID,
    site_id     UUID,
    metric      LowCardinality(String),
    observed_at DateTime64(3, 'UTC'),
    ingested_at DateTime64(3, 'UTC'),
    value       Float64,
    unit        LowCardinality(String),
    labels      Map(LowCardinality(String), String)
)
ENGINE = MergeTree
PARTITION BY toYYYYMM(observed_at)
ORDER BY (tenant_id, resource_id, metric, observed_at)
SETTINGS index_granularity = 8192;

CREATE TABLE bench.metrics_5m
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
ORDER BY (tenant_id, resource_id, metric, bucket);

-- NOTE: the MV is created but the load script can skip it via --no-mv.
-- Rollup-on-ingest costs write throughput; W1 must measure raw ingest rate BOTH ways
-- so the cost of rollups is a known number rather than a surprise in M4.
CREATE MATERIALIZED VIEW bench.metrics_5m_mv TO bench.metrics_5m AS
SELECT
    tenant_id,
    resource_id,
    metric,
    toStartOfFiveMinute(observed_at) AS bucket,
    minState(value)   AS min_v,
    maxState(value)   AS max_v,
    avgState(value)   AS avg_v,
    countState()      AS cnt
FROM bench.metrics
GROUP BY tenant_id, resource_id, metric, bucket;
