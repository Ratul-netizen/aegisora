-- 0001 — organization, tenant, site. SPEC §M0.1.
--
-- The hierarchy exists from day one so an MSP can own many tenants without any
-- cross-tenant data access. A single-company deployment has exactly one organization
-- and one tenant; the cost of that generality is one join.
--
--   Organization ──┬── Tenant ──┬── Site ──┬── Resource ──┬── Resource (child)
--                  │            │          │              └── Resource (child)
--                  └── Tenant   └── Site   └── Resource

-- `organization` is the one table in the schema without a tenant_id, and it is not an
-- exception to SPEC's "every table carries tenant_id" rule — it sits *above* the
-- isolation boundary rather than inside it. Every table below this line has one.
CREATE TABLE organization (
    id         uuid        PRIMARY KEY,
    name       text        NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE tenant (
    id         uuid        PRIMARY KEY,
    org_id     uuid        NOT NULL REFERENCES organization (id),
    name       text        NOT NULL,
    slug       text        NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now(),

    UNIQUE (org_id, slug)
);

CREATE TABLE site (
    id        uuid PRIMARY KEY,
    tenant_id uuid NOT NULL REFERENCES tenant (id),
    name      text NOT NULL,
    -- IANA name, e.g. 'Asia/Dhaka'. Everything below the UI is UTC; this is only for
    -- rendering and for maintenance-window scheduling in M4.
    timezone  text NOT NULL DEFAULT 'UTC',

    UNIQUE (tenant_id, name),

    -- Not redundant with the primary key: it is the target of the composite foreign
    -- keys below, which is how a row in one tenant is prevented from referencing a row
    -- in another. See 0002.
    UNIQUE (id, tenant_id)
);

-- `updated_at` columns that are only ever set by a DEFAULT are decoration: they record
-- when the row was created and then lie forever. A trigger is the only way to make the
-- column mean what its name says.
CREATE FUNCTION set_updated_at() RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    NEW.updated_at := now();
    RETURN NEW;
END;
$$;
