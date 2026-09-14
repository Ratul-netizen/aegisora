SELECT bucket AS g0, avgMerge(avg_v) AS avg_cpu, maxMerge(max_v) AS peak_cpu FROM metrics_1h WHERE tenant_id = {p0:UUID} AND bucket >= {p1:DateTime64(3)} AND bucket < {p2:DateTime64(3)} AND resource_id IN ({p3:UUID}) AND metric = {p4:String} GROUP BY g0 ORDER BY g0 ASC LIMIT 2000

-- params
--   p0 UUID = 018f0000-0000-7000-8000-000000000001
--   p1 DateTime64(3) = 2026-06-01 00:00:00.000
--   p2 DateTime64(3) = 2026-08-30 00:00:00.000
--   p3 UUID = 018f0000-0000-7000-8000-0000000000aa
--   p4 String = system.cpu.utilization

-- table: metrics_1h
-- warnings:
--   showing pre-aggregated 3600s buckets from metrics_1h; raw points are past their retention
