-- Alert rules and their state — SPEC §M4.
--
-- A rule is a saved `Query` plus a condition. That is the whole model, and it is why the
-- AST was built in M0 and saved searches in M3: "alert from a saved search" needs no new
-- machinery here, because `alert_rule.query` and `saved_search.query` hold the same bytes.
--
-- The two tables answer different questions and are shaped differently because of it.
-- `alert_rule` is what a human wrote and changes rarely. `alert_state` is what the engine
-- believes right now, one row per *series* — a rule over five thousand resources has up
-- to five thousand of them — and is written on every evaluation cycle.

CREATE TABLE alert_rule (
    id            uuid        PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id     uuid        NOT NULL REFERENCES tenant (id) ON DELETE CASCADE,

    name          text        NOT NULL,
    description   text        NOT NULL DEFAULT '',

    -- 'threshold' | 'absence'. Denormalised from `condition->>'kind'` and kept honest by
    -- the CHECK below, for the same reason `saved_search.signal` is: listing a tenant's
    -- rules and grouping them by kind must not parse a jsonb document per row.
    kind          text        NOT NULL,

    -- The `Query` AST — §M0.5, the same type the Explorer posts and a saved search
    -- stores. Its time window is *provenance*, exactly as it is for a saved search: the
    -- evaluator substitutes its own evaluation window, because a rule that kept asking
    -- about last Tuesday afternoon would be a rule that never fires again.
    query         jsonb       NOT NULL,

    -- `{"kind":"threshold","op":"gt","value":90,"hold_seconds":300}` or
    -- `{"kind":"absence","after_seconds":300}` — `uops_core::alert::Condition`.
    --
    -- Tagged, so that an absence rule cannot carry a threshold's operator. The
    -- alternative — a flat (op, value, for) triple — stores two columns that mean nothing
    -- for half the rows and that a UI then has to decide whether to show.
    condition     jsonb       NOT NULL,

    severity      text        NOT NULL,

    -- A disabled rule is not evaluated and keeps its state rows. Disabling is what an
    -- operator reaches for at 3am instead of deleting, and a rule that lost its history
    -- when it was silenced would make the next morning's question unanswerable.
    enabled       boolean     NOT NULL DEFAULT true,

    -- How often the engine asks. Not how long a breach must last — that is `hold_seconds`
    -- inside the condition, and conflating the two is how a rule ends up firing on its
    -- first evaluation because somebody set the interval instead of the dwell.
    eval_interval interval    NOT NULL DEFAULT '60 seconds',

    -- Channel references, resolved when notification channels exist. `[]` means the rule
    -- changes state and tells nobody, which is a legitimate thing to want while a rule is
    -- being tuned — and is the default, so a rule created before any channel exists does
    -- not fail.
    notify        jsonb       NOT NULL DEFAULT '[]'::jsonb,

    created_by    uuid        REFERENCES app_user (id),
    created_at    timestamptz NOT NULL DEFAULT now(),
    updated_at    timestamptz NOT NULL DEFAULT now(),

    CONSTRAINT alert_rule_name_is_unique_in_the_tenant UNIQUE (tenant_id, name),
    -- The composite-FK target, so `alert_state` cannot hold a rule from another tenant
    -- even if somebody guesses the uuid. Same pattern as every intra-tenant reference
    -- since migration 0002.
    UNIQUE (id, tenant_id),
    CONSTRAINT alert_rule_name_not_blank CHECK (length(trim(name)) > 0),

    CONSTRAINT alert_rule_query_is_an_object CHECK (jsonb_typeof(query) = 'object'),
    CONSTRAINT alert_rule_condition_is_an_object CHECK (jsonb_typeof(condition) = 'object'),
    CONSTRAINT alert_rule_kind_matches_the_condition CHECK (kind = condition->>'kind'),
    CONSTRAINT alert_rule_kind_is_known CHECK (kind IN ('threshold', 'absence')),
    CONSTRAINT alert_rule_severity_is_known CHECK (
        severity IN ('info', 'warning', 'critical')
    ),

    -- A floor, not a preference. SPEC's scale target is 1 000 rules inside one 60-second
    -- cycle; a rule evaluating every second would spend that budget on itself, and the
    -- operator who typed it meant "quickly", not "constantly". The ceiling is a day
    -- because a rule evaluated less often than that is a report.
    CONSTRAINT alert_rule_interval_is_sane CHECK (
        eval_interval BETWEEN interval '10 seconds' AND interval '1 day'
    )
);

CREATE TRIGGER alert_rule_set_updated_at
    BEFORE UPDATE ON alert_rule
    FOR EACH ROW EXECUTE FUNCTION set_updated_at();

-- ----------------------------------------------------------------------------
-- State
-- ----------------------------------------------------------------------------
--
-- One row per series that is not healthy. A healthy series has no row, deliberately: a
-- rule matching five thousand resources that are all fine would otherwise write five
-- thousand rows every cycle to say that nothing happened, and the table the UI reads to
-- answer "what is wrong right now" would be almost entirely noise.
--
-- `state = 'ok'` is still storable, because a series that *was* firing returns to ok and
-- the row is where the engine remembers it has already announced the resolution.

CREATE TABLE alert_state (
    id          uuid        PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id   uuid        NOT NULL REFERENCES tenant (id) ON DELETE CASCADE,

    rule_id     uuid        NOT NULL,
    -- Deleting a rule deletes what it believed. The alternative is state rows referring
    -- to a rule nobody can look up, which is an alert in the UI that cannot be explained,
    -- acknowledged or silenced.
    FOREIGN KEY (rule_id, tenant_id) REFERENCES alert_rule (id, tenant_id) ON DELETE CASCADE,

    resource_id uuid        NOT NULL,
    FOREIGN KEY (resource_id, tenant_id) REFERENCES resource (id, tenant_id) ON DELETE CASCADE,

    -- rule + resource + label fingerprint — `uops_core::alert::dedup_key`. Readable
    -- rather than hashed, because it appears here, in the API, and in the sentence
    -- somebody types into a support ticket.
    --
    -- This UNIQUE is the deduplication. Two evaluators racing on the same series — a
    -- restart overlapping its predecessor, two replicas of the engine — write the same
    -- key, and the second one updates rather than creating a second alert about one
    -- problem.
    dedup_key   text        NOT NULL,

    state       text        NOT NULL,
    -- When the current state began. Carried forward while a state persists, because
    -- `pending` measures from the first breach; advancing it every cycle would mean a
    -- rule with a dwell never fires. `uops_core::alert::step` owns that arithmetic.
    since       timestamptz NOT NULL,
    last_eval   timestamptz NOT NULL,
    last_value  double precision,

    -- Acknowledgement silences the *notification*, never the state. An acknowledged alert
    -- is still firing and still in the list; what it is not is a second page at 4am about
    -- something somebody is already holding a laptop over.
    acked_by    uuid        REFERENCES app_user (id),
    acked_at    timestamptz,

    CONSTRAINT alert_state_is_one_per_series UNIQUE (tenant_id, dedup_key),
    CONSTRAINT alert_state_state_is_known CHECK (
        state IN ('ok', 'pending', 'firing', 'resolved')
    ),
    -- An acknowledgement is a person and a time or it is neither. Half of one is a row
    -- the UI shows as acknowledged by nobody.
    CONSTRAINT alert_state_ack_is_whole CHECK (
        (acked_by IS NULL) = (acked_at IS NULL)
    )
);

-- What the UI asks on every page load: what is wrong in this tenant right now, worst
-- first. Partial, because the rows that are 'ok' are the ones nobody is looking for and
-- there are eventually more of them than anything else.
CREATE INDEX alert_state_active_idx
    ON alert_state (tenant_id, since DESC)
    WHERE state IN ('pending', 'firing');

-- What the evaluator asks: everything this rule currently believes, so a cycle reads one
-- rule's state in one go rather than one row at a time.
CREATE INDEX alert_state_by_rule_idx ON alert_state (rule_id, tenant_id);

-- "Why is this device not alerting" — asked from a resource page, not from a rule.
CREATE INDEX alert_state_by_resource_idx ON alert_state (resource_id, tenant_id);

-- The referencing-side indexes for the remaining foreign keys. Migration 0010 explains
-- why every one of them needs its own, and the guard in migrations/tests/ requires it.
CREATE INDEX alert_rule_tenant_idx ON alert_rule (tenant_id);
CREATE INDEX alert_rule_created_by_idx ON alert_rule (created_by);
CREATE INDEX alert_state_tenant_idx ON alert_state (tenant_id);
CREATE INDEX alert_state_acked_by_idx ON alert_state (acked_by);
