SELECT tenant_id, resource_id, site_id, observed_at, ingested_at, source_kind, source_vendor, severity, facility, body, attributes, trace_id, span_id FROM logs WHERE tenant_id = {p0:UUID} AND observed_at >= {p1:DateTime64(3)} AND observed_at < {p2:DateTime64(3)} AND resource_id IN ({p3:UUID}, {p4:UUID}) ORDER BY resource_id ASC, observed_at DESC LIMIT 200

-- params
--   p0 UUID = 018f0000-0000-7000-8000-000000000001
--   p1 DateTime64(3) = 2026-09-01 00:00:00.000
--   p2 DateTime64(3) = 2026-09-01 01:00:00.000
--   p3 UUID = 018f0000-0000-7000-8000-0000000000aa
--   p4 UUID = 018f0000-0000-7000-8000-0000000000bb

-- table: logs
-- warnings: none
