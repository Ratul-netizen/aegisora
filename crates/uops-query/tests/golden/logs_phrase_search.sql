SELECT tenant_id, resource_id, site_id, observed_at, ingested_at, source_kind, source_vendor, severity, facility, body, attributes, trace_id, span_id FROM logs WHERE tenant_id = {p0:UUID} AND observed_at >= {p1:DateTime64(3)} AND observed_at < {p2:DateTime64(3)} AND (hasAllTokens(body, [{p3:String}, {p4:String}, {p5:String}, {p6:String}]) AND positionCaseInsensitive(body, {p7:String}) > 0) ORDER BY resource_id ASC, observed_at DESC LIMIT 100

-- params
--   p0 UUID = 018f0000-0000-7000-8000-000000000001
--   p1 DateTime64(3) = 2026-09-01 00:00:00.000
--   p2 DateTime64(3) = 2026-09-02 00:00:00.000
--   p3 String = changed
--   p4 String = state
--   p5 String = to
--   p6 String = down
--   p7 String = changed state to down

-- table: logs
-- warnings:
--   this reads every resource in the tenant; narrowing to a resource is much faster
--   phrase search cannot use the text index: tokens narrow granules, then word order is verified by scanning them; measured at 2 378 ms over 100M rows
