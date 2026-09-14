-- 0004 — identity resolution storage. SPEC §M0.2.
--
-- The highest-leverage component in the system: when `router-01` appears in SNMP,
-- syslog, NetFlow, LLDP, a config backup and an alert, all six must resolve to one
-- resource_id. Everything downstream is worthless if this is wrong.

CREATE TYPE identifier_kind AS ENUM (
    -- Tier 1 — globally unique by specification, base confidence 1.00
    'chassis_id',        -- LLDP chassis ID
    'serial',            -- entPhysicalSerialNum
    'snmp_engine_id',    -- unique per SNMPv3 agent
    'otel_host_id',      -- OTel host.id (machine ID)
    -- Tier 2 — 0.90
    'mac',               -- unique, but NICs move between chassis
    -- Tier 3 — 0.80
    'mgmt_ip',           -- DHCP and re-IP break this
    'flow_exporter',     -- same failure mode
    -- Tier 4 — names, 0.65 / 0.60
    'hostname',          -- sysName, syslog HOSTNAME, host.name
    'service_name'       -- OTel service.name; often many resources, not one host
);

CREATE TABLE resource_identifier (
    id          uuid            PRIMARY KEY,
    tenant_id   uuid            NOT NULL REFERENCES tenant (id),
    resource_id uuid            NOT NULL,
    kind        identifier_kind NOT NULL,
    value       text            NOT NULL,
    confidence  real            NOT NULL CHECK (confidence BETWEEN 0 AND 1),
    -- 'snmp' | 'syslog' | 'otlp' | 'manual'
    source      text            NOT NULL,
    first_seen  timestamptz     NOT NULL DEFAULT now(),
    last_seen   timestamptz     NOT NULL DEFAULT now(),

    -- THE resolution index. This one constraint is the whole mechanism: resolution is a
    -- lookup, not a scan, which is what lets a syslog receiver at 50k msg/s resolve
    -- against a cache backed by an index rather than a sequential scan.
    --
    -- A violation here is not an error. It is a genuine identity conflict — two devices
    -- claiming one hostname — and it belongs in the review queue, which is why
    -- uops_core::Error::IdentityConflict maps to 409 and not 500.
    UNIQUE (tenant_id, kind, value),

    FOREIGN KEY (resource_id, tenant_id) REFERENCES resource (id, tenant_id)
        ON DELETE CASCADE
);

-- "Everything known about this resource", for the identity review UI.
CREATE INDEX resource_identifier_resource_idx
    ON resource_identifier (tenant_id, resource_id);

-- Every decision is recorded. This is what makes identity mistakes debuggable instead
-- of mystifying six months later, when nobody remembers why two switches became one.
CREATE TABLE identity_decision (
    id          uuid        PRIMARY KEY,
    tenant_id   uuid        NOT NULL REFERENCES tenant (id),
    -- NULL when the decision created a new resource rather than matching one.
    resource_id uuid,

    outcome     text        NOT NULL CHECK (outcome IN (
                                'auto_merge', 'review', 'new',
                                'manual_merge', 'manual_split')),
    confidence  real        NOT NULL CHECK (confidence BETWEEN 0 AND 1),

    -- [{"kind":"serial","value":"FTX…","confidence":1.0}]
    matched_by  jsonb       NOT NULL,
    -- The full identifier set presented, including the ones that did not match. A
    -- decision cannot be re-argued from the matches alone.
    observed    jsonb       NOT NULL,
    source      text        NOT NULL,
    -- Set only for manual decisions, so "who merged these" has an answer.
    actor_id    uuid,
    decided_at  timestamptz NOT NULL DEFAULT now(),

    FOREIGN KEY (resource_id, tenant_id) REFERENCES resource (id, tenant_id)
        ON DELETE SET NULL
);

CREATE INDEX identity_decision_review_idx
    ON identity_decision (tenant_id, decided_at DESC)
    WHERE outcome = 'review';

-- Telemetry already written to ClickHouse under a pre-merge resource_id is never
-- rewritten. This table maps the historical ID onto the surviving one, which is what
-- makes a merge O(1) instead of a re-ingest — and is the reason the query layer owns
-- resource-ID expansion rather than each caller doing it.
CREATE TABLE resource_alias (
    tenant_id     uuid        NOT NULL,
    historical_id uuid        NOT NULL,
    current_id    uuid        NOT NULL,
    merged_at     timestamptz NOT NULL DEFAULT now(),
    decision_id   uuid        REFERENCES identity_decision (id),

    PRIMARY KEY (tenant_id, historical_id),

    FOREIGN KEY (current_id, tenant_id) REFERENCES resource (id, tenant_id),

    CONSTRAINT alias_is_not_circular CHECK (historical_id <> current_id)
);

-- Resolves the open question in SPEC §M0.2 ("alias chain depth") in favour of the
-- recommended option: collapse on write.
--
-- A merged into B, then B merged into C. Either A→B is rewritten to A→C when B→C is
-- created (collapse on write), or every read resolves transitively (a recursive lookup
-- on the hot path of every telemetry query). Merges are rare and reads are constant, so
-- the cost belongs on the write.
--
-- Enforced here rather than in application code because the invariant this buys —
-- "no historical_id is ever also a current_id", so one lookup is always enough — is
-- only true if *every* writer collapses, including a DBA fixing something by hand at
-- 3am.
CREATE FUNCTION resource_alias_collapse() RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    resolved uuid;
BEGIN
    -- Merging into a resource that has itself already been merged away: follow the
    -- chain now, so the row lands pointing at something that still exists.
    SELECT a.current_id INTO resolved
      FROM resource_alias a
     WHERE a.tenant_id = NEW.tenant_id
       AND a.historical_id = NEW.current_id;

    IF resolved IS NOT NULL THEN
        NEW.current_id := resolved;
    END IF;

    IF NEW.current_id = NEW.historical_id THEN
        RAISE EXCEPTION 'alias would point at itself after collapse: %', NEW.historical_id
            USING ERRCODE = 'check_violation';
    END IF;

    -- Anything that pointed at the resource now being merged away follows it.
    UPDATE resource_alias
       SET current_id = NEW.current_id,
           merged_at  = now()
     WHERE tenant_id = NEW.tenant_id
       AND current_id = NEW.historical_id;

    RETURN NEW;
END;
$$;

CREATE TRIGGER resource_alias_collapse_on_write
    BEFORE INSERT ON resource_alias
    FOR EACH ROW EXECUTE FUNCTION resource_alias_collapse();
