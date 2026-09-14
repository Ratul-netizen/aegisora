SELECT bucket AS g0, severity AS g1, countMerge(cnt) AS c FROM logs_counts_5m WHERE tenant_id = {p0:UUID} AND bucket >= {p1:DateTime64(3)} AND bucket < {p2:DateTime64(3)} GROUP BY g0, g1 ORDER BY g0 ASC LIMIT 500

-- params
--   p0 UUID = 018f0000-0000-7000-8000-000000000001
--   p1 DateTime64(3) = 2026-09-01 00:00:00.000
--   p2 DateTime64(3) = 2026-09-02 00:00:00.000

-- table: logs_counts_5m
-- warnings: none
