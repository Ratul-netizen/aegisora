-- The documented fallback from SPEC.md §M0.6: a projection ordered by time rather
-- than by resource, for tenant-wide search across all resources.
--
-- Created as a SEPARATE TABLE rather than a projection so W1 can measure both sort
-- orders independently and report the storage cost of keeping both. If the primary
-- sort key turns out to be fast enough for tenant-wide search, this gets deleted and
-- the [OPEN] item in SPEC.md closes as "not needed".

DROP TABLE IF EXISTS bench.logs_by_time;

CREATE TABLE bench.logs_by_time
(
    tenant_id     UUID,
    resource_id   UUID,
    site_id       UUID,
    observed_at   DateTime64(3, 'UTC'),
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

    INDEX idx_body body TYPE text(tokenizer = 'splitByNonAlpha') GRANULARITY 1
)
ENGINE = MergeTree
PARTITION BY toYYYYMMDD(observed_at)
ORDER BY (tenant_id, observed_at)
SETTINGS index_granularity = 8192;
