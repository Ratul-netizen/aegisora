-- Resource groups, and operator tags kept apart from system attributes.
--
-- Both are from the architecture review of 2026-09-16, classified FOUNDATIONAL there for
-- the same reason: they are cheap now and expensive after M4, because every alert rule,
-- dashboard, notification policy and maintenance window written against the old shape
-- would have to be migrated.

-- ---------------------------------------------------------------------------
-- Resource groups
-- ---------------------------------------------------------------------------
--
-- An operator-defined set: "Dhaka Core Routers", "Critical Servers", "Internet Edge",
-- "Production". Deliberately none of the three groupings that already exist:
--
--   site         where a thing physically is
--   parent_id    what it is part of        (interface -> device, container -> host)
--   relationship how it is connected       (the topology graph)
--
-- Those are all discovered. A group is *decided* — it encodes an operator's judgement
-- about which resources matter together, and nothing can infer it. "Critical Servers" is
-- not a place, not a containment and not a link; it is a sentence somebody wrote down.
--
-- Every M4 feature needs to name a set of resources: an alert rule's scope, a dashboard's
-- filter, a notification routing rule, a maintenance window's target. Without this they
-- would each name a site or a list of IDs, and a list of IDs goes stale the moment a
-- device is added.

CREATE TABLE resource_group (
    id          uuid        PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id   uuid        NOT NULL REFERENCES tenant (id) ON DELETE CASCADE,
    name        text        NOT NULL,
    description text        NOT NULL DEFAULT '',
    created_at  timestamptz NOT NULL DEFAULT now(),
    updated_at  timestamptz NOT NULL DEFAULT now(),

    -- Two customers of one MSP may both have a "Core Routers"; one customer may not have
    -- two. Scoped uniqueness, like every other name in this schema.
    UNIQUE (tenant_id, name),

    -- The composite key that makes membership's foreign key tenant-safe. Same pattern as
    -- resource and site: a child row cannot reference a parent in another tenant,
    -- because the tenant is part of what it references.
    UNIQUE (id, tenant_id),

    CONSTRAINT resource_group_name_not_blank CHECK (length(trim(name)) > 0)
);

CREATE TRIGGER resource_group_set_updated_at
    BEFORE UPDATE ON resource_group
    FOR EACH ROW EXECUTE FUNCTION set_updated_at();

-- Membership is explicit rows rather than a predicate stored on the group.
--
-- A rule-based group ("everything tagged criticality=critical") is the obvious
-- alternative and is a *later* feature, not this one: it needs the tag query language to
-- exist first, it has to be re-evaluated on every resource write, and an alert scoped to
-- a rule that silently starts matching 400 more devices is a genuinely bad surprise. An
-- explicit list is what an operator can audit.
--
-- When rule-based groups arrive they materialise into this same table, so nothing
-- downstream changes.
CREATE TABLE resource_group_member (
    tenant_id   uuid        NOT NULL,
    group_id    uuid        NOT NULL,
    resource_id uuid        NOT NULL,
    added_at    timestamptz NOT NULL DEFAULT now(),

    PRIMARY KEY (tenant_id, group_id, resource_id),

    -- Both endpoints carry the tenant into the reference, so a group in tenant A cannot
    -- contain a resource in tenant B even if somebody guesses the uuid. This is the
    -- structural half of tenant isolation that SPEC M0.8 asks for: not a WHERE clause
    -- somebody has to remember.
    FOREIGN KEY (group_id, tenant_id)
        REFERENCES resource_group (id, tenant_id) ON DELETE CASCADE,
    FOREIGN KEY (resource_id, tenant_id)
        REFERENCES resource (id, tenant_id) ON DELETE CASCADE
);

-- "Which groups is this resource in" — the resource detail page, and the reverse of the
-- primary key's "which resources are in this group". Also the index the resource_id
-- foreign key needs; see migration 0010 for why every one of them needs its own.
CREATE INDEX resource_group_member_resource_idx
    ON resource_group_member (tenant_id, resource_id);

-- The group's own foreign key to tenant. 0010's guard requires it.
CREATE INDEX resource_group_tenant_idx
    ON resource_group (tenant_id);

-- ---------------------------------------------------------------------------
-- Operator tags
-- ---------------------------------------------------------------------------
--
-- `resource.attributes` holds what a collector discovered: OpenTelemetry semantic
-- conventions, written by discovery on every walk. `resource.tags` holds what a human
-- decided: environment=production, criticality=critical, owner=network-team.
--
-- One column for both would work right up until the first time discovery overwrote
-- `criticality=critical`, which it would, silently, and the resulting alert-routing bug
-- would be unreproducible because the evidence would have been overwritten too.
--
-- The precedent is already in this schema and was already right. From 0002:
--
--   display_name  User override. Never written automatically — if a human named it,
--                 discovery must not silently rename it underneath them.
--
-- Tags are that argument applied to attributes. The rule, enforced by convention in the
-- repository layer rather than by the database, because "which process wrote this" is
-- not something a CHECK constraint can see:
--
--   attributes  collectors write, humans read
--   tags        humans write, collectors never touch
--
-- jsonb rather than a key/value table. Tags are a small flat map read on every resource
-- fetch, never joined against, and the GIN index below makes containment queries —
-- "every resource tagged environment=production" — an index scan. A side table would
-- make the common read a join to save a cost nothing is paying.
ALTER TABLE resource ADD COLUMN tags jsonb NOT NULL DEFAULT '{}';

-- Flat string-to-string. A nested tag is not a tag, and discovering that a routing rule
-- silently ignored `owner.team` because it was an object is not a debugging session
-- anybody should have.
--
-- Via a function because a CHECK constraint may not contain a subquery, and testing
-- "every value is a string" needs one. IMMUTABLE is true here — the result depends only
-- on the argument — which is what makes it legal in a constraint at all.
CREATE FUNCTION jsonb_is_flat_string_map(j jsonb) RETURNS boolean
LANGUAGE sql IMMUTABLE PARALLEL SAFE
AS $$
    SELECT jsonb_typeof(j) = 'object'
       AND NOT EXISTS (
           SELECT 1 FROM jsonb_each(j) AS e(key, value)
            WHERE jsonb_typeof(e.value) <> 'string'
       );
$$;

ALTER TABLE resource ADD CONSTRAINT resource_tags_are_flat_strings
    CHECK (jsonb_is_flat_string_map(tags));

-- jsonb_path_ops, matching resource_attributes_idx: smaller and faster than the default
-- for the containment queries this exists to serve, at the cost of key-existence
-- queries, which tag selection does not use — "tagged environment=production" is
-- containment, and "has any environment tag" is not a thing an alert rule asks.
CREATE INDEX resource_tags_idx ON resource USING gin (tags jsonb_path_ops);
