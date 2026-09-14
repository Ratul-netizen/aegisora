-- 0003 — relationship edges and graph traversal. SPEC §M0.1.
--
-- The topology *UI* waits for M6. The *edges* do not: interface discovery in M2
-- produces them for free, and the correlation engine in M9 cannot be designed against a
-- table that does not exist.

CREATE TYPE relationship_kind AS ENUM (
    'connected_to',   -- L2/L3 adjacency (M5/M6)
    'depends_on',     -- service → database
    'hosts',          -- hypervisor → vm, host → container
    'runs',           -- host → service
    'routes_to',      -- L3 next hop
    'member_of'       -- interface → device, node → cluster
);

CREATE TABLE resource_relationship (
    id            uuid              PRIMARY KEY,
    tenant_id     uuid              NOT NULL REFERENCES tenant (id),
    source_id     uuid              NOT NULL,
    target_id     uuid              NOT NULL,
    kind          relationship_kind NOT NULL,

    -- Discovered edges are not facts. An LLDP neighbour is near-certain; an edge
    -- inferred from traffic patterns is a guess, and M9 needs to tell them apart.
    confidence    real              NOT NULL DEFAULT 1.0
                                    CHECK (confidence BETWEEN 0 AND 1),
    -- 'lldp' | 'manual' | 'snmp-iftable' | 'otel'. Free text on purpose: a new
    -- discovery source should not need a migration.
    discovered_by text              NOT NULL,
    attributes    jsonb             NOT NULL DEFAULT '{}',

    first_seen    timestamptz       NOT NULL DEFAULT now(),
    last_seen     timestamptz       NOT NULL DEFAULT now(),

    UNIQUE (tenant_id, source_id, target_id, kind),

    -- Both endpoints in the same tenant as the edge. An edge is the one shape that
    -- could bridge two tenants' graphs, and a bridged graph is a cross-tenant read for
    -- every traversal that follows it.
    FOREIGN KEY (source_id, tenant_id) REFERENCES resource (id, tenant_id)
        ON DELETE CASCADE,
    FOREIGN KEY (target_id, tenant_id) REFERENCES resource (id, tenant_id)
        ON DELETE CASCADE,

    CONSTRAINT relationship_is_not_a_self_loop CHECK (source_id <> target_id)
);

CREATE INDEX resource_relationship_source_idx
    ON resource_relationship (tenant_id, source_id, kind);
CREATE INDEX resource_relationship_target_idx
    ON resource_relationship (tenant_id, target_id, kind);

-- Blast radius: everything that depends on `root`, up to `max_depth` hops.
--
-- Written once, here, so that M6 (topology) and M9 (correlation) do not each invent
-- their own recursive CTE — and so the cycle guard is written once rather than
-- remembered three times.
--
-- # Two deviations from the SPEC §M0.1 sketch, both deliberate
--
-- 1. **`tenant` is a parameter, and every step filters on it.** The sketch took
--    `(root, max_depth)` only. Knowing a UUID is not authorization: without the filter,
--    a caller who learns one resource ID from another tenant can walk that tenant's
--    graph. `ResourceCatalog::descendants` in `uops-query` already takes the tenant, so
--    this signature is what the Rust side was written against.
--
-- 2. **`root` is returned, at depth 0.** A blast radius that excludes the thing that
--    broke forces every caller to remember to add it back.
--
-- The cycle guard is not optional. Real networks contain relationship cycles, and the
-- first one hangs an API worker until the request times out.
CREATE FUNCTION resource_dependents(tenant uuid, root uuid, max_depth int)
RETURNS TABLE (resource_id uuid, depth int, path uuid[])
LANGUAGE sql STABLE
AS $$
    WITH RECURSIVE walk AS (
        -- Anchored on the resource table rather than on the argument, so a root that
        -- belongs to another tenant yields nothing instead of walking this tenant's
        -- edges from a foreign starting point.
        SELECT r.id AS source_id, 0 AS depth, ARRAY[r.id] AS path
          FROM resource r
         WHERE r.id = resource_dependents.root
           AND r.tenant_id = resource_dependents.tenant
        UNION ALL
        SELECT r.source_id, w.depth + 1, w.path || r.source_id
          FROM resource_relationship r
          JOIN walk w ON r.target_id = w.source_id
         WHERE r.tenant_id = resource_dependents.tenant
           AND r.kind IN ('depends_on', 'member_of', 'hosts', 'runs')
           AND w.depth < max_depth
           AND NOT r.source_id = ANY (w.path)   -- cycle guard, mandatory
    )
    SELECT source_id, depth, path FROM walk;
$$;
