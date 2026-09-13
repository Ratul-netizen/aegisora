-- W1 benchmark: logs table.
-- Mirrors SPEC.md §M0.6 exactly. If this DDL needs to change to apply, the SPEC
-- changes too — that is one of the things W1 exists to find out.

DROP TABLE IF EXISTS bench.logs;

CREATE TABLE bench.logs
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

    INDEX idx_body body TYPE text(tokenizer = 'splitByNonAlpha') GRANULARITY 1,
    INDEX idx_sev  severity TYPE set(16) GRANULARITY 4
)
ENGINE = MergeTree
PARTITION BY toYYYYMMDD(observed_at)
ORDER BY (tenant_id, resource_id, observed_at)
SETTINGS index_granularity = 8192;

-- NOTE: TTL / storage_policy from the SPEC are omitted here on purpose.
-- W1 measures query latency and compression on a single volume; tiering to S3/MinIO
-- is a separate question and adding it now would confound the numbers.
