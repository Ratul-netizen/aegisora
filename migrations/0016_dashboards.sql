-- Dashboards — SPEC §M4.
--
-- "Panels are `{ query: Query, viz: VizSpec }`. Layout is a grid. Time range comes from
-- the global picker unless a panel overrides it."
--
-- So a dashboard is a *document*: a name and an ordered list of panels, edited as a whole
-- and read as a whole. That is why the panels are one jsonb column rather than a table of
-- their own — nothing ever asks for one panel, a panel has no life outside the dashboard
-- it is on, and a `dashboard_panel` table would buy referential integrity for a
-- relationship that only ever goes one way.
--
-- The cost is that moving one panel rewrites the row. A dashboard is a few kilobytes and
-- is edited by one person at a time; the cost of the alternative is a join on every read
-- of every panel for the life of the product.

CREATE TABLE dashboard (
    id          uuid        PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id   uuid        NOT NULL REFERENCES tenant (id) ON DELETE CASCADE,

    name        text        NOT NULL,
    description text        NOT NULL DEFAULT '',

    -- `[{ id, title, query, viz, width, height }, …]` — `uops_store_pg::Panel`.
    --
    -- Each panel's `query` is a `Query` AST: the same type the Explorer posts, a saved
    -- search stores and an alert rule holds. A panel is a saved search with a picture on
    -- it, which is why there is no fourth representation of a question in this schema.
    --
    -- The window inside each query is provenance, exactly as it is for a saved search and
    -- a rule: the dashboard's own time range is substituted when it is read. A panel that
    -- kept asking about the afternoon it was created on would be a screenshot.
    panels      jsonb       NOT NULL DEFAULT '[]'::jsonb,

    created_by  uuid        REFERENCES app_user (id),
    created_at  timestamptz NOT NULL DEFAULT now(),
    updated_at  timestamptz NOT NULL DEFAULT now(),

    CONSTRAINT dashboard_name_is_unique_in_the_tenant UNIQUE (tenant_id, name),
    CONSTRAINT dashboard_name_not_blank CHECK (length(trim(name)) > 0),

    -- An array, because a dashboard is an ordered list of panels and the order is the
    -- layout. `{}` and `"hello"` are both valid jsonb and neither is a dashboard.
    CONSTRAINT dashboard_panels_is_an_array CHECK (jsonb_typeof(panels) = 'array'),

    -- SPEC's dashboard acceptance criterion is twenty panels. Forty is the ceiling
    -- because a dashboard is read at a glance and one with forty panels is not being
    -- glanced at — and because every panel is a telemetry query, so this is also the
    -- limit on what one page load asks of ClickHouse.
    --
    -- Guarded on the type, because PostgreSQL does not promise an order between two
    -- CHECKs on one row: without the guard, inserting `{}` evaluates
    -- `jsonb_array_length('{}')` and fails with *"cannot get array length of a
    -- non-array"* (SQLSTATE 22023) instead of the constraint violation that says what is
    -- actually wrong. Found by the invariant tests, which assert the SQLSTATE rather than
    -- just that something failed.
    CONSTRAINT dashboard_panels_are_a_screenful CHECK (
        jsonb_typeof(panels) <> 'array' OR jsonb_array_length(panels) <= 40
    )
);

CREATE TRIGGER dashboard_set_updated_at
    BEFORE UPDATE ON dashboard
    FOR EACH ROW EXECUTE FUNCTION set_updated_at();

-- The list: a tenant's dashboards by name, which is how somebody looks for one.
CREATE INDEX dashboard_by_tenant_idx ON dashboard (tenant_id, name);

-- The referencing-side index for `created_by`. Migration 0010 explains why every foreign
-- key needs one, and the guard in migrations/tests/ requires it.
CREATE INDEX dashboard_created_by_idx ON dashboard (created_by);
