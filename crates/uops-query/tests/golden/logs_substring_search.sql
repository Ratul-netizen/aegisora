SELECT tenant_id, resource_id, site_id, observed_at, ingested_at, source_kind, source_vendor, severity, facility, body, attributes, trace_id, span_id FROM logs WHERE tenant_id = {p0:UUID} AND observed_at >= {p1:DateTime64(3)} AND observed_at < {p2:DateTime64(3)} AND resource_id IN ({p3:UUID}, {p4:UUID}, {p5:UUID}) AND (severity >= {p6:String} AND positionCaseInsensitive(body, {p7:String}) > 0) ORDER BY resource_id ASC, observed_at DESC LIMIT 100

-- params
--   p0 UUID = 018f0000-0000-7000-8000-000000000001
--   p1 DateTime64(3) = 2026-09-01 00:00:00.000
--   p2 DateTime64(3) = 2026-09-01 01:00:00.000
--   p3 UUID = 018f0000-0000-7000-8000-0000000000aa
--   p4 UUID = 018f0000-0000-7000-8000-0000000000bb
--   p5 UUID = 018f0000-0000-7000-8000-0000000000cc
--   p6 String = error
--   p7 String = updown

-- table: logs
-- warnings:
--   substring search cannot use the text index: a substring can start inside a token, so the token index cannot prune granules; measured at 1 928 ms over 100M rows
