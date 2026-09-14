-- 0002 — the resource model. SPEC §M0.1.
--
-- The centre of the data model. Everything else in the product is an opinion about
-- rows in this table.
--
-- # Cross-tenant references are impossible here, not merely avoided
--
-- `parent_id` and `site_id` are foreign keys to tables that are themselves
-- tenant-scoped, so the obvious spelling —
--
--     parent_id uuid REFERENCES resource (id)
--
-- — would happily let a resource in tenant A be the child of a resource in tenant B.
-- No application code intends that, which is precisely why nobody notices when it
-- happens. Every intra-tenant reference in this schema is therefore a *composite* key
-- carrying tenant_id, so the database refuses it.
--
-- This is the SQL counterpart of `TenantScope` in `uops-core`: the same invariant, held
-- at the other end of the wire. A missing tenant filter is a compile error in Rust and
-- a constraint violation in PostgreSQL.

CREATE TYPE resource_kind AS ENUM (
    'device', 'interface', 'host', 'vm', 'container', 'service',
    'application', 'database', 'cloud_resource', 'site'
);

CREATE TYPE resource_status AS ENUM (
    'up', 'down', 'degraded', 'unknown', 'maintenance', 'decommissioned'
);

CREATE TABLE resource (
    id             uuid            PRIMARY KEY,
    tenant_id      uuid            NOT NULL REFERENCES tenant (id),
    site_id        uuid,
    -- interface → device, container → host.
    parent_id      uuid,

    kind           resource_kind   NOT NULL,
    -- Canonical and system-chosen. Identity resolution may rewrite it.
    name           text            NOT NULL,
    -- A user override. Never auto-written, so an operator's label survives rediscovery.
    display_name   text,

    vendor         text,
    model          text,
    os             text,
    os_version     text,
    status         resource_status NOT NULL DEFAULT 'unknown',

    -- → monitoring_profile (M2). Unconstrained until that table exists.
    profile_id     uuid,
    -- → credential (M0.4). A reference, never the material. FK added in 0005.
    credential_ref uuid,

    -- OTel semantic-convention keys, so enrichment and the Query AST agree on names.
    attributes     jsonb           NOT NULL DEFAULT '{}',

    first_seen     timestamptz     NOT NULL DEFAULT now(),
    last_seen      timestamptz     NOT NULL DEFAULT now(),
    created_at     timestamptz     NOT NULL DEFAULT now(),
    updated_at     timestamptz     NOT NULL DEFAULT now(),

    -- The composite-FK targets. See the header.
    UNIQUE (id, tenant_id),

    -- MATCH SIMPLE (the default) means a NULL site_id or parent_id satisfies the
    -- constraint, which is what "no site" and "no parent" need.
    FOREIGN KEY (site_id, tenant_id)   REFERENCES site (id, tenant_id),
    FOREIGN KEY (parent_id, tenant_id) REFERENCES resource (id, tenant_id),

    -- A resource that is its own parent is a one-row cycle, and every traversal in the
    -- product would have to defend against it separately.
    CONSTRAINT resource_not_its_own_parent CHECK (parent_id IS DISTINCT FROM id)
);

-- Inventory listing and profile assignment: "every device in this tenant".
CREATE INDEX resource_tenant_kind_idx ON resource (tenant_id, kind);
-- Walking down the containment tree: a device's interfaces, a host's containers.
CREATE INDEX resource_tenant_parent_idx ON resource (tenant_id, parent_id);
-- jsonb_path_ops is roughly half the size of the default opclass and faster for the
-- containment queries (@>) that attribute search actually issues. It does not support
-- key-existence (?), which attribute search does not use.
CREATE INDEX resource_attributes_idx ON resource USING gin (attributes jsonb_path_ops);

CREATE TRIGGER resource_set_updated_at
    BEFORE UPDATE ON resource
    FOR EACH ROW EXECUTE FUNCTION set_updated_at();
