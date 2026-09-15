-- 0007 — monitoring profiles. SPEC §M2.
--
-- The alternative to this table is `if vendor == "Cisco"` spreading through the poller
-- until nobody can change it. A profile says what to walk, what to turn into a child
-- resource, what to poll and how often — declaratively, versioned, and overridable by a
-- customer who has a device we have never seen.

CREATE TABLE monitoring_profile (
    id          uuid        PRIMARY KEY,

    -- NULL means built-in: shipped with the product and visible to every tenant. A
    -- tenant's own row shadows a built-in with the same key, which is how a customer
    -- fixes a wrong OID on a Thursday without waiting for a release.
    tenant_id   uuid        REFERENCES tenant (id),

    profile_key text        NOT NULL,
    version     int         NOT NULL CHECK (version > 0),

    -- The profile itself. jsonb rather than a dozen tables: it is read whole, by one
    -- consumer, and shaped by a Rust type that validates far more than a schema could —
    -- that an interface-scoped metric has a discovery rule to attach to, that every OID
    -- parses, that no two metrics share a name. See uops-profile.
    definition  jsonb       NOT NULL,

    enabled     bool        NOT NULL DEFAULT true,
    created_at  timestamptz NOT NULL DEFAULT now(),
    updated_at  timestamptz NOT NULL DEFAULT now(),

    UNIQUE (tenant_id, profile_key, version)
);

-- Built-ins are global, so the usual (tenant_id, …) index shape does not serve the
-- lookup that matters: "every profile this tenant can use", which is theirs plus every
-- built-in. NULLS FIRST puts the built-ins where a range scan finds them.
CREATE INDEX monitoring_profile_lookup_idx
    ON monitoring_profile (profile_key, version DESC)
    WHERE enabled;

-- A partial unique index, because UNIQUE (tenant_id, …) does not constrain NULLs in
-- PostgreSQL: without this, two built-ins with the same key and version are allowed,
-- and which one wins is whichever the planner reaches first.
CREATE UNIQUE INDEX monitoring_profile_builtin_key
    ON monitoring_profile (profile_key, version)
    WHERE tenant_id IS NULL;

COMMENT ON TABLE monitoring_profile IS
    'Declarative polling definitions. tenant_id NULL = built-in, shipped with the product.';

-- Which profile a resource was resolved to, so that "why is this device polling every
-- 30 seconds" has an answer that is not "read the code". NULL means unresolved — either
-- never polled, or a device whose sysObjectID matched nothing and has not yet fallen
-- back.
ALTER TABLE resource
    ADD COLUMN profile_resolved_at timestamptz;

-- resource.profile_id already exists (0002) and is the explicit override: set it and
-- sysObjectID matching is skipped entirely. The foreign key is added here because the
-- table it points at did not exist until now.
ALTER TABLE resource
    ADD CONSTRAINT resource_profile_id_fkey
    FOREIGN KEY (profile_id) REFERENCES monitoring_profile (id)
    ON DELETE SET NULL;
