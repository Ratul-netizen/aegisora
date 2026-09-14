-- DECLARED, NOT CREATED. M7 (flows) and M8 (traces).
--
-- Kept here so that the sort key, the tenant column and the attribute shape are settled
-- before anything is written against them — the same reason the traits exist in
-- uops-core and the signal variants exist in the Query AST.

CREATE TABLE IF NOT EXISTS traces
(
    tenant_id     UUID,
    resource_id   UUID,
    site_id       UUID,
    observed_at   DateTime64(3, 'UTC'),
    ingested_at   DateTime64(3, 'UTC'),
    trace_id      String,
    span_id       String,
    parent_span_id String,
    name          LowCardinality(String),
    kind          LowCardinality(String),
    duration_ns   UInt64,
    status_code   LowCardinality(String),
    attributes    Map(LowCardinality(String), String),

    -- Trace lookup is by trace_id, which the sort key cannot serve: it leads with
    -- resource. A skip index is the difference between a lookup and a tenant scan.
    INDEX idx_trace trace_id TYPE bloom_filter GRANULARITY 1
)
ENGINE = MergeTree
PARTITION BY toYYYYMMDD(observed_at)
ORDER BY (tenant_id, resource_id, observed_at)
TTL toDateTime(observed_at) + INTERVAL 30 DAY DELETE;

CREATE TABLE IF NOT EXISTS flows
(
    tenant_id        UUID,
    resource_id      UUID,       -- the exporter
    site_id          UUID,
    observed_at      DateTime64(3, 'UTC'),
    ingested_at      DateTime64(3, 'UTC'),
    src_address      IPv6,       -- IPv4 is stored mapped; one column, both families
    dst_address      IPv6,
    src_port         UInt16,
    dst_port         UInt16,
    protocol         UInt8,
    bytes            UInt64,
    packets          UInt64,
    tcp_flags        UInt16,
    src_resource_id  UUID,       -- resolved endpoints, when identity can place them
    dst_resource_id  UUID,
    attributes       Map(LowCardinality(String), String)
)
ENGINE = MergeTree
PARTITION BY toYYYYMMDD(observed_at)
ORDER BY (tenant_id, resource_id, observed_at)
TTL toDateTime(observed_at) + INTERVAL 30 DAY DELETE;
