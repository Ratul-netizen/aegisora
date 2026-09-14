-- 0006 — events and state transitions.
--
-- Same shape, same sort key, same reason: "everything about resource R in window W" has
-- to be a contiguous range read on every table, or the Investigation Workspace is a
-- fan-out of six different access patterns.

CREATE TABLE IF NOT EXISTS events
(
    tenant_id      UUID,
    resource_id    UUID,
    site_id        UUID,
    observed_at    DateTime64(3, 'UTC'),
    ingested_at    DateTime64(3, 'UTC'),
    source_kind    LowCardinality(String),
    source_vendor  LowCardinality(String),
    severity       Enum8('trace'=1,'debug'=2,'info'=3,'notice'=4,'warn'=5,
                         'error'=6,'critical'=7,'alert'=8,'emergency'=9),
    event_category LowCardinality(String),
    event_type     LowCardinality(String),
    summary        String,
    attributes     Map(LowCardinality(String), String),

    -- Events carry the same materialised keys as logs, because `uops_query::plan`
    -- rewrites Attr fields onto real columns for Log AND Event. If these were missing,
    -- the compiler would emit SQL naming columns that do not exist.
    host_name      String                 MATERIALIZED attributes['host.name'],
    service_name   LowCardinality(String) MATERIALIZED attributes['service.name']
)
ENGINE = MergeTree
PARTITION BY toYYYYMM(observed_at)
ORDER BY (tenant_id, resource_id, observed_at)
TTL toDateTime(observed_at) + INTERVAL 365 DAY DELETE;

CREATE TABLE IF NOT EXISTS states
(
    tenant_id       UUID,
    resource_id     UUID,
    site_id         UUID,
    observed_at     DateTime64(3, 'UTC'),
    ingested_at     DateTime64(3, 'UTC'),
    severity        Enum8('trace'=1,'debug'=2,'info'=3,'notice'=4,'warn'=5,
                          'error'=6,'critical'=7,'alert'=8,'emergency'=9),
    previous_status LowCardinality(String),
    current_status  LowCardinality(String),
    reason          String,
    attributes      Map(LowCardinality(String), String)
)
ENGINE = MergeTree
PARTITION BY toYYYYMM(observed_at)
ORDER BY (tenant_id, resource_id, observed_at)
-- State transitions are small and are the spine of every availability report. They
-- outlive the telemetry that produced them.
TTL toDateTime(observed_at) + INTERVAL 1095 DAY DELETE;
