SELECT host_name AS g0, count() AS events FROM logs WHERE tenant_id = {p0:UUID} AND observed_at >= {p1:DateTime64(3)} AND observed_at < {p2:DateTime64(3)} AND service_name = {p3:String} GROUP BY g0 ORDER BY events DESC LIMIT 50

-- params
--   p0 UUID = 018f0000-0000-7000-8000-000000000001
--   p1 DateTime64(3) = 2026-09-01 00:00:00.000
--   p2 DateTime64(3) = 2026-09-02 00:00:00.000
--   p3 String = bgpd

-- table: logs
-- warnings:
--   this reads every resource in the tenant; narrowing to a resource is much faster
