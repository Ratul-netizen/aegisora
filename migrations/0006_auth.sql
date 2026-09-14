-- 0006 — users, sessions, roles, and the two audit logs. SPEC §M0.8, §M1 RBAC.
--
-- Everything M0 built rests on one function being reachable: `TenantScope`'s
-- `from_authenticated`. These tables are what makes it reachable honestly.

CREATE TYPE tenant_role AS ENUM ('viewer', 'operator', 'admin');

CREATE TABLE app_user (
    id            uuid        PRIMARY KEY,
    -- Users belong to an organization, and get roles on that organization's tenants.
    -- An MSP engineer is one account with a role per customer, not one account each.
    org_id        uuid        NOT NULL REFERENCES organization (id),
    email         text        NOT NULL,
    display_name  text        NOT NULL,
    -- PHC string: algorithm and cost parameters travel with the hash, so the cost can
    -- be raised later without invalidating anyone's password. See uops_secrets::password.
    password_hash text        NOT NULL,
    -- Disabled rather than deleted: an account that acted must remain nameable in the
    -- audit log afterwards.
    disabled_at   timestamptz,
    created_at    timestamptz NOT NULL DEFAULT now(),
    updated_at    timestamptz NOT NULL DEFAULT now(),

    -- Case-insensitive: nobody remembers whether they signed up as Ratul@ or ratul@,
    -- and two accounts differing only in case is an account-takeover vector.
    UNIQUE (org_id, email),
    UNIQUE (id, org_id)
);

CREATE UNIQUE INDEX app_user_email_ci_idx ON app_user (org_id, lower(email));

CREATE TRIGGER app_user_set_updated_at
    BEFORE UPDATE ON app_user
    FOR EACH ROW EXECUTE FUNCTION set_updated_at();

-- SPEC §M1: "Role is per (user, tenant), so one MSP operator can be admin on one tenant
-- and viewer on another." Three roles in v0.1; resist adding more until a customer asks.
CREATE TABLE user_tenant_role (
    user_id    uuid        NOT NULL REFERENCES app_user (id) ON DELETE CASCADE,
    tenant_id  uuid        NOT NULL REFERENCES tenant (id) ON DELETE CASCADE,
    role       tenant_role NOT NULL,
    granted_at timestamptz NOT NULL DEFAULT now(),
    granted_by uuid        REFERENCES app_user (id),

    PRIMARY KEY (user_id, tenant_id)
);

-- "Which tenants can this user see" is asked on every authenticated request.
CREATE INDEX user_tenant_role_user_idx ON user_tenant_role (user_id);

CREATE TABLE session (
    id           uuid        PRIMARY KEY,
    user_id      uuid        NOT NULL REFERENCES app_user (id) ON DELETE CASCADE,

    -- The HASH of the opaque token, never the token. A stolen database backup must not
    -- hand the thief a set of live sessions. The token is high-entropy random, so a
    -- plain SHA-256 is right here — there is no low-entropy secret to make expensive.
    token_hash   bytea       NOT NULL,

    -- Two clocks, both required (SPEC §M0.8: 12h idle / 7d absolute). Idle timeout ends
    -- an abandoned session on a shared screen; the absolute one bounds a stolen token
    -- that is being kept alive by use.
    created_at   timestamptz NOT NULL DEFAULT now(),
    last_seen_at timestamptz NOT NULL DEFAULT now(),
    expires_at   timestamptz NOT NULL,

    -- Recorded for the audit trail and for "sign out my other devices".
    ip           inet,
    user_agent   text,
    revoked_at   timestamptz,

    UNIQUE (token_hash)
);

-- Session lookup happens on every single request, so it is a unique-index probe on the
-- hash and nothing else.
CREATE INDEX session_user_idx ON session (user_id) WHERE revoked_at IS NULL;
-- Expired-session cleanup scans this.
CREATE INDEX session_expiry_idx ON session (expires_at) WHERE revoked_at IS NULL;

-- Every mutating call. SPEC §M0.8.
CREATE TABLE audit_log (
    id        bigserial   PRIMARY KEY,
    tenant_id uuid        NOT NULL,
    -- 'user:<uuid>' | 'collector' | 'system', from Actor::as_audit_str.
    actor     text        NOT NULL,
    action    text        NOT NULL,           -- 'resource.create', 'identity.merge'
    target    text        NOT NULL,           -- the id acted upon
    -- Before and after, so a change is reviewable without replaying the whole history.
    before    jsonb,
    after     jsonb,
    ip        inet,
    at        timestamptz NOT NULL DEFAULT now()
);

CREATE INDEX audit_log_tenant_at_idx ON audit_log (tenant_id, at DESC);
CREATE INDEX audit_log_actor_idx ON audit_log (tenant_id, actor, at DESC);

-- Every READ of a credential, a resource detail, or a telemetry query. SPEC §M0.8
-- calls this out separately and explains why: defence and law-enforcement buyers audit
-- who SAW what, not only who changed it. Trivial to add now; invasive to retrofit.
CREATE TABLE access_log (
    id         bigserial   PRIMARY KEY,
    tenant_id  uuid        NOT NULL,
    actor      text        NOT NULL,
    target     text        NOT NULL,          -- 'resource:<id>' | 'query' | 'credential:<id>'
    -- A hash of the query shape rather than the query: the shape is what an auditor
    -- needs, and the parameters can contain a customer's hostnames and addresses.
    fingerprint text,
    row_count  bigint,
    ip         inet,
    at         timestamptz NOT NULL DEFAULT now()
);

CREATE INDEX access_log_tenant_at_idx ON access_log (tenant_id, at DESC);
CREATE INDEX access_log_actor_idx ON access_log (tenant_id, actor, at DESC);

-- NOTE — no foreign key from either log to what it describes, and none to app_user.
-- The logs outlive their subjects on purpose: deleting a resource, or a user, must not
-- delete the record of who read or changed it. A cascade here would quietly remove
-- exactly the rows an investigation needs, at exactly the moment someone wanted them
-- gone. Same reasoning as credential_access_log in 0005.
