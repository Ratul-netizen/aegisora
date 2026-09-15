SELECT toStartOfInterval(observed_at, INTERVAL 300 SECOND) AS g0, resource_id AS g1, avg(rate) AS bytes_per_second, max(rate) AS peak FROM (SELECT tenant_id, resource_id, site_id, metric, observed_at, ingested_at, value, unit, labels, if(rn > 1 AND observed_at > prev_at AND value >= prev_value, (value - prev_value) / ((toUnixTimestamp64Milli(observed_at) - toUnixTimestamp64Milli(prev_at)) / 1000), NULL) AS rate FROM (SELECT tenant_id, resource_id, site_id, metric, observed_at, ingested_at, value, unit, labels, lagInFrame(value) OVER w AS prev_value, lagInFrame(observed_at) OVER w AS prev_at, row_number() OVER w AS rn FROM metrics WHERE tenant_id = {p0:UUID} AND observed_at >= {p1:DateTime64(3)} AND observed_at < {p2:DateTime64(3)} AND metric = {p3:String} WINDOW w AS (PARTITION BY resource_id, metric, labels ORDER BY observed_at ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW))) GROUP BY g0, g1 ORDER BY peak DESC LIMIT 100

-- params
--   p0 UUID = 018f0000-0000-7000-8000-000000000001
--   p1 DateTime64(3) = 2026-09-01 00:00:00.000
--   p2 DateTime64(3) = 2026-09-01 06:00:00.000
--   p3 String = network.io.receive

-- table: metrics
-- warnings:
--   this reads every resource in the tenant; narrowing to a resource is much faster
