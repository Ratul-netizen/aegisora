SELECT attributes[{p0:String}] AS g0, count() AS c FROM logs WHERE tenant_id = {p1:UUID} AND observed_at >= {p2:DateTime64(3)} AND observed_at < {p3:DateTime64(3)} GROUP BY g0 LIMIT 20

-- params
--   p0 String = device.role
--   p1 UUID = 018f0000-0000-7000-8000-000000000001
--   p2 DateTime64(3) = 2026-09-01 00:00:00.000
--   p3 DateTime64(3) = 2026-09-01 01:00:00.000

-- table: logs
-- warnings:
--   `device.role` is not a materialised column, so every row must be decompressed to read it
--   this reads every resource in the tenant; narrowing to a resource is much faster
