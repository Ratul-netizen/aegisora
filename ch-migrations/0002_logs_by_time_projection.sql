-- 0002 — the tail. W1 FIX 1.
--
-- Measured: the tail query ("newest N lines across the tenant") read the ENTIRE tenant
-- at 2 303 ms and 33.8M rows, because the sort key leads with resource_id and a
-- time-ordered scan cannot prune on it. With this projection: 72 ms and 254K rows. 32x.
--
-- Cost: roughly 1.9x total storage for the logs table. Time-ordering also compresses
-- ~58% worse than resource-ordering (4.79 GiB vs 3.03 GiB for identical data), because
-- sorting by resource groups rows that share vendor, source and host. Budget for it.
--
-- Kept as its own migration because it is the expensive one: an operator watching an
-- upgrade should see it as a distinct step rather than as part of "create the logs
-- table".

-- Deliberately does NOT carry the text index. Search resolves against the base table,
-- and projections do not support secondary indexes in any case.
ALTER TABLE logs ADD PROJECTION IF NOT EXISTS p_by_time
(
    SELECT * ORDER BY (tenant_id, observed_at)
);

-- Instant on an empty table, a mutation on a populated one. Re-running re-materialises
-- rather than failing, which is what makes this migration safe to resume.
ALTER TABLE logs MATERIALIZE PROJECTION p_by_time;
