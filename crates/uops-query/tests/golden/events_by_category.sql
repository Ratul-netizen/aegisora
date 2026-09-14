SELECT event_category AS g0, count() AS n FROM events WHERE tenant_id = {p0:UUID} AND observed_at >= {p1:DateTime64(3)} AND observed_at < {p2:DateTime64(3)} GROUP BY g0 ORDER BY n DESC LIMIT 25

-- params
--   p0 UUID = 018f0000-0000-7000-8000-000000000001
--   p1 DateTime64(3) = 2026-09-01 00:00:00.000
--   p2 DateTime64(3) = 2026-09-02 00:00:00.000

-- table: events
-- warnings:
--   this reads every resource in the tenant; narrowing to a resource is much faster
