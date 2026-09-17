-- Notification channels, and the record of what was sent — SPEC §M4.
--
-- SPEC is specific about why this table has limits in it: *"per-channel rate limiting and
-- a per-tenant notification budget, because the first misconfigured rule **will** try to
-- send 10 000 emails."* That is not a hypothetical. A rule matching 5 000 resources with
-- a zero dwell, written at 2am by somebody testing, sends 5 000 notifications in one
-- evaluation — and the damage is not the mail server, it is that the customer's alerting
-- address is now on a blocklist and nobody notices for a week.
--
-- So delivery is refused by default beyond a rate, the refusals are recorded, and the
-- refusal is visible in the same place the deliveries are.

CREATE TABLE notification_channel (
    id          uuid        PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id   uuid        NOT NULL REFERENCES tenant (id) ON DELETE CASCADE,

    name        text        NOT NULL,
    -- 'webhook' | 'email'. Text with a CHECK rather than an enum type: a third kind is a
    -- migration either way, and a text column is one an operator can read in psql.
    kind        text        NOT NULL,

    -- Per kind. A webhook's is `{"url": "...", "headers": {...}}`; an email's is
    -- `{"to": ["ops@example.com"], "from": "..."}`. jsonb rather than a column per field,
    -- because the fields a channel needs are the channel kind's business and adding a
    -- kind should not be a migration that widens every row.
    --
    -- **Secrets do not go here.** A webhook that needs a bearer token references a sealed
    -- credential by id, the way a device does — see uops-secrets. This column is readable
    -- by anyone who can read the row.
    config      jsonb       NOT NULL,

    -- A channel that is off keeps its history and sends nothing. What an operator reaches
    -- for when the pager is misbehaving at 3am, and the reason this is not a delete.
    enabled     boolean     NOT NULL DEFAULT true,

    -- The per-channel rate limit, in notifications a minute.
    --
    -- Twelve by default, which is one every five seconds: enough that a real incident
    -- across a dozen devices arrives promptly, and low enough that a misconfigured rule
    -- is stopped inside the first minute rather than after the first ten thousand. An
    -- operator who genuinely wants more can say so, and the number is visible in the UI
    -- next to what it has actually sent.
    max_per_minute integer  NOT NULL DEFAULT 12,

    created_by  uuid        REFERENCES app_user (id),
    created_at  timestamptz NOT NULL DEFAULT now(),
    updated_at  timestamptz NOT NULL DEFAULT now(),

    CONSTRAINT notification_channel_name_is_unique_in_the_tenant UNIQUE (tenant_id, name),
    CONSTRAINT notification_channel_name_not_blank CHECK (length(trim(name)) > 0),
    UNIQUE (id, tenant_id),

    CONSTRAINT notification_channel_kind_is_known CHECK (kind IN ('webhook', 'email')),
    CONSTRAINT notification_channel_config_is_an_object CHECK (
        jsonb_typeof(config) = 'object'
    ),
    -- Zero would be a channel that exists and can never deliver, which is what `enabled`
    -- is for and says more clearly. The ceiling is a rate no human reads: beyond one a
    -- second the thing on the other end is a system, and a system should be reading the
    -- alert API rather than being sent a thousand messages a minute.
    CONSTRAINT notification_channel_rate_is_sane CHECK (
        max_per_minute BETWEEN 1 AND 60
    )
);

CREATE TRIGGER notification_channel_set_updated_at
    BEFORE UPDATE ON notification_channel
    FOR EACH ROW EXECUTE FUNCTION set_updated_at();

-- ----------------------------------------------------------------------------
-- The per-tenant budget
-- ----------------------------------------------------------------------------
--
-- A column on `tenant` rather than a table of its own: there is exactly one number per
-- tenant, it is read on every send, and a second table would be a join to answer "how
-- many more may I send today".
--
-- A thousand a day is high enough that no real estate reaches it and low enough that a
-- runaway rule stops on the first day rather than the first month. It is the backstop
-- behind the per-channel rate: the rate limits how *fast*, this limits how *many*, and
-- the two failures are different — a storm and a slow leak.
ALTER TABLE tenant
    ADD COLUMN notification_budget_per_day integer NOT NULL DEFAULT 1000;

ALTER TABLE tenant
    ADD CONSTRAINT tenant_notification_budget_is_sane
    CHECK (notification_budget_per_day BETWEEN 0 AND 100000);

-- ----------------------------------------------------------------------------
-- What was sent, and what was refused
-- ----------------------------------------------------------------------------
--
-- Every attempt, including the ones the limits stopped. A refusal that leaves no trace is
-- indistinguishable from a rule that never fired, and "why did nobody get paged" is the
-- question this table exists to answer.

CREATE TABLE notification_sent (
    id          uuid        PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id   uuid        NOT NULL REFERENCES tenant (id) ON DELETE CASCADE,

    channel_id  uuid        NOT NULL,
    FOREIGN KEY (channel_id, tenant_id)
        REFERENCES notification_channel (id, tenant_id) ON DELETE CASCADE,

    -- The alert this was about. Deliberately *not* a foreign key to `alert_state`: state
    -- rows are deleted with their rule, and the record of having woken somebody up at 3am
    -- must outlive the rule somebody deleted at 9am to stop it happening again.
    rule_id     uuid        NOT NULL,
    dedup_key   text        NOT NULL,
    phase       text        NOT NULL,

    -- 'sent' | 'failed' | 'rate_limited' | 'over_budget'.
    outcome     text        NOT NULL,
    -- The transport's own words when it failed. An operator debugging a webhook needs the
    -- status code and the body, not "delivery failed".
    detail      text        NOT NULL DEFAULT '',

    sent_at     timestamptz NOT NULL DEFAULT now(),

    CONSTRAINT notification_sent_outcome_is_known CHECK (
        outcome IN ('sent', 'failed', 'rate_limited', 'over_budget')
    ),
    CONSTRAINT notification_sent_phase_is_known CHECK (
        phase IN ('firing', 'resolved')
    )
);

-- The rate-limit question: how many did this channel actually send in the last minute.
-- Partial on the outcome, because the refusals are the rows this index must not carry —
-- a channel that is being rate-limited accumulates them fastest, and they are precisely
-- the ones that do not count towards the limit.
CREATE INDEX notification_sent_rate_idx
    ON notification_sent (channel_id, sent_at DESC)
    WHERE outcome = 'sent';

-- The budget question: how many has this tenant sent today, across every channel.
CREATE INDEX notification_sent_budget_idx
    ON notification_sent (tenant_id, sent_at DESC)
    WHERE outcome = 'sent';

-- "Why did nobody get paged for this alert" — asked about one alert, not one channel.
CREATE INDEX notification_sent_by_alert_idx
    ON notification_sent (tenant_id, dedup_key, sent_at DESC);

-- The remaining foreign keys' referencing-side indexes. Migration 0010 explains why every
-- one of them needs its own, and the guard in migrations/tests/ requires it — which is
-- how the composite one below was found missing: the three indexes above are partial or
-- lead with the wrong column, and none of them serves `(channel_id, tenant_id)` when a
-- channel is deleted.
CREATE INDEX notification_sent_channel_idx ON notification_sent (channel_id, tenant_id);
CREATE INDEX notification_channel_tenant_idx ON notification_channel (tenant_id);
CREATE INDEX notification_channel_created_by_idx ON notification_channel (created_by);
CREATE INDEX notification_sent_tenant_idx ON notification_sent (tenant_id);
