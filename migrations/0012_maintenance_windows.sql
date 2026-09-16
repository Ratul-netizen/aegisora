-- Maintenance windows — when not to wake somebody up.
--
-- `resource_status` has had a `maintenance` value since migration 0002 and nothing has
-- ever written it. This is what writes it, and more importantly it is what the alert
-- engine will consult before it fires.
--
-- The problem is concrete: somebody reboots 500 switches on a Saturday night, and without
-- this the product pages the on-call engineer 500 times about work they scheduled
-- themselves. An alerting system that cannot be told about planned work is an alerting
-- system people turn off, and a monitoring platform nobody trusts is worse than none.
--
-- From the architecture review of 2026-09-16, classified M1-M4: design the model now,
-- because the alert engine has to consult it and retrofitting suppression onto a rule
-- engine that already fires is a rewrite.

CREATE TABLE maintenance_window (
    id          uuid        PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id   uuid        NOT NULL REFERENCES tenant (id) ON DELETE CASCADE,

    -- Why. Required, and deliberately not nullable: a window somebody finds open six
    -- months later with no explanation is one nobody dares delete, and it goes on
    -- silencing alerts forever.
    reason      text        NOT NULL,

    -- ----------------------------------------------------------------------
    -- What it covers: exactly one of three
    -- ----------------------------------------------------------------------
    --
    -- Three nullable columns and a CHECK rather than a polymorphic (kind, id) pair,
    -- because this way each one keeps its composite foreign key. A window in tenant A
    -- cannot target tenant B's site even if somebody guesses the uuid — the tenant is
    -- part of what it references, which is the structural half of isolation SPEC M0.8
    -- asks for.
    --
    -- A group target is why groups had to be built first: "everything I put in Dhaka Core
    -- Routers" is what an operator actually means, and neither a site nor a single
    -- resource says it.
    target_resource_id uuid,
    target_group_id    uuid,
    target_site_id     uuid,

    FOREIGN KEY (target_resource_id, tenant_id)
        REFERENCES resource (id, tenant_id) ON DELETE CASCADE,
    FOREIGN KEY (target_group_id, tenant_id)
        REFERENCES resource_group (id, tenant_id) ON DELETE CASCADE,
    FOREIGN KEY (target_site_id, tenant_id)
        REFERENCES site (id, tenant_id) ON DELETE CASCADE,

    CONSTRAINT maintenance_window_targets_exactly_one CHECK (
        (target_resource_id IS NOT NULL)::int
      + (target_group_id    IS NOT NULL)::int
      + (target_site_id     IS NOT NULL)::int = 1
    ),

    -- ----------------------------------------------------------------------
    -- When
    -- ----------------------------------------------------------------------
    --
    -- starts_at is the first occurrence. Recurrences are computed from the *local* time
    -- it lands on, in `timezone`.
    starts_at        timestamptz NOT NULL,

    -- A duration rather than an end instant, because a recurring window's end moves with
    -- its start. Storing both would let them disagree after the first occurrence, and
    -- they would — on a daylight-saving boundary, silently, in whichever direction is
    -- least convenient.
    duration_minutes integer     NOT NULL,

    -- An IANA name, not an offset. "Every Saturday 02:00-04:00" means 02:00 where the
    -- equipment is; an installation storing +06:00 would move its window by an hour twice
    -- a year in every country that observes daylight saving, and would then either alert
    -- during the work or stay silent for an hour afterwards.
    --
    -- Validated in Rust against chrono-tz rather than here: PostgreSQL's own zone table
    -- and the IANA release chrono-tz embeds are different lists, and a CHECK against
    -- pg_timezone_names would accept names the application then could not resolve.
    timezone         text        NOT NULL,

    -- 'once' | 'daily' | 'weekly' | 'monthly'. Text with a CHECK rather than an enum type:
    -- a fifth recurrence is a migration either way, and a text column is one an operator
    -- can read in psql without looking up an oid.
    recurrence       text        NOT NULL,
    -- 0-6, Monday=0, for 'weekly'. NULL otherwise.
    recur_weekday    smallint,
    -- 1-31, for 'monthly'. NULL otherwise. A month without that day has no occurrence —
    -- the 31st does not exist in April, and sliding to the 30th would suppress alerts on
    -- a day nobody chose.
    recur_day        smallint,
    -- When it stops recurring. NULL is forever, which is what a standing Saturday-night
    -- change window is.
    until            timestamptz,

    -- ----------------------------------------------------------------------
    -- What it suppresses
    -- ----------------------------------------------------------------------
    --
    -- Two flags because they are genuinely different requests. "Suppress notifications"
    -- means keep alerting, keep the history, just do not wake anybody — what an operator
    -- watching their own work wants. "Suppress alerts" means the rule does not fire at
    -- all, which is the bigger hammer and the default, because an alert that fired during
    -- planned work is still in the history afterwards and somebody has to explain each one.
    suppress_alerts        boolean NOT NULL DEFAULT true,
    suppress_notifications boolean NOT NULL DEFAULT true,

    created_by  uuid        REFERENCES app_user (id),
    created_at  timestamptz NOT NULL DEFAULT now(),
    updated_at  timestamptz NOT NULL DEFAULT now(),

    CONSTRAINT maintenance_window_reason_not_blank CHECK (length(trim(reason)) > 0),

    -- A minute at least, a week at most. The ceiling is not a technical limit: a window
    -- longer than a week is almost always a mis-typed end date, and the consequence of
    -- that mistake is an estate that stops alerting with nobody noticing, which is the
    -- worst failure this feature can have. An operator who genuinely wants a month of
    -- silence can say so four times, and will remember doing it.
    CONSTRAINT maintenance_window_duration_is_sane CHECK (
        duration_minutes BETWEEN 1 AND 24 * 7 * 60
    ),

    CONSTRAINT maintenance_window_recurrence_is_known CHECK (
        recurrence IN ('once', 'daily', 'weekly', 'monthly')
    ),

    -- The recurrence and its parameter have to agree. Without this, a 'weekly' window
    -- with a NULL weekday is storable and silently never opens — a window an operator
    -- created, can see in the UI, and which does nothing.
    CONSTRAINT maintenance_window_recurrence_has_its_parameter CHECK (
        (recurrence = 'weekly')  = (recur_weekday IS NOT NULL)
        AND (recurrence = 'monthly') = (recur_day IS NOT NULL)
    ),
    CONSTRAINT maintenance_window_weekday_is_a_weekday CHECK (
        recur_weekday IS NULL OR recur_weekday BETWEEN 0 AND 6
    ),
    CONSTRAINT maintenance_window_day_is_a_day CHECK (
        recur_day IS NULL OR recur_day BETWEEN 1 AND 31
    ),
    CONSTRAINT maintenance_window_until_is_after_it_starts CHECK (
        until IS NULL OR until >= starts_at
    )
);

CREATE TRIGGER maintenance_window_set_updated_at
    BEFORE UPDATE ON maintenance_window
    FOR EACH ROW EXECUTE FUNCTION set_updated_at();

-- The query the alert engine runs: every window in the tenant that has not expired.
-- Which of them is *open* is arithmetic the application does, because the occurrence
-- rules — a skipped spring-forward hour, the earlier of two ambiguous local times — are
-- not something to write twice in two languages.
--
-- Partial on `until`, because an estate accumulates finished one-off windows forever and
-- none of them can ever open again.
CREATE INDEX maintenance_window_live_idx
    ON maintenance_window (tenant_id, starts_at)
    WHERE until IS NULL OR until > '2000-01-01'::timestamptz;

-- The three foreign keys' referencing-side indexes. Migration 0010 explains why every one
-- of them needs its own, and the guard in migrations/tests/ requires it.
CREATE INDEX maintenance_window_resource_idx
    ON maintenance_window (target_resource_id, tenant_id)
    WHERE target_resource_id IS NOT NULL;
CREATE INDEX maintenance_window_group_idx
    ON maintenance_window (target_group_id, tenant_id)
    WHERE target_group_id IS NOT NULL;
CREATE INDEX maintenance_window_site_idx
    ON maintenance_window (target_site_id, tenant_id)
    WHERE target_site_id IS NOT NULL;
CREATE INDEX maintenance_window_tenant_idx
    ON maintenance_window (tenant_id);
CREATE INDEX maintenance_window_created_by_idx
    ON maintenance_window (created_by);
