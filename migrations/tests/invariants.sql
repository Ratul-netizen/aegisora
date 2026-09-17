-- Schema invariants. Run against a database that has every migration applied.
--
--     psql "$DATABASE_URL" -v ON_ERROR_STOP=1 -f migrations/tests/invariants.sql
--
-- These assert the properties the schema is *for*, not that the tables exist. Anything
-- here that fails is a tenant-isolation hole, a hang, or a lost audit trail — the three
-- things the DDL comments claim are impossible.
--
-- The whole file runs in one transaction and rolls back, so it leaves no rows behind
-- and is safe to run against a development database.

\set ON_ERROR_STOP on

BEGIN;

-- ---------------------------------------------------------------- helpers

CREATE FUNCTION pg_temp.must_fail(stmt text, expected_sqlstate text) RETURNS void
LANGUAGE plpgsql AS $$
BEGIN
    BEGIN
        EXECUTE stmt;
    EXCEPTION WHEN others THEN
        IF SQLSTATE <> expected_sqlstate THEN
            RAISE EXCEPTION 'expected SQLSTATE % but got % (%) from: %',
                expected_sqlstate, SQLSTATE, SQLERRM, stmt;
        END IF;
        RETURN;
    END;
    -- Reached only if the statement succeeded, and outside the handler above, so it
    -- cannot be swallowed by it.
    RAISE EXCEPTION 'statement was accepted but must have been refused: %', stmt;
END;
$$;

CREATE FUNCTION pg_temp.check(condition bool, what text) RETURNS void
LANGUAGE plpgsql AS $$
BEGIN
    IF condition IS NOT TRUE THEN
        RAISE EXCEPTION 'FAILED: %', what;
    END IF;
END;
$$;

-- ---------------------------------------------------------------- fixtures
--
-- One MSP, two customers. Every isolation test below is "can tenant A's row reach
-- tenant B's row", which is the shape an MSP deployment actually has.

INSERT INTO organization (id, name) VALUES
    ('00000000-0000-0000-0000-0000000000f0', 'An MSP');

INSERT INTO tenant (id, org_id, name, slug) VALUES
    ('00000000-0000-0000-0000-00000000000a',
     '00000000-0000-0000-0000-0000000000f0', 'Customer A', 'cust-a'),
    ('00000000-0000-0000-0000-00000000000b',
     '00000000-0000-0000-0000-0000000000f0', 'Customer B', 'cust-b');

INSERT INTO site (id, tenant_id, name) VALUES
    ('00000000-0000-0000-0000-0000000000a1',
     '00000000-0000-0000-0000-00000000000a', 'Dhaka DC'),
    ('00000000-0000-0000-0000-0000000000b1',
     '00000000-0000-0000-0000-00000000000b', 'Chittagong DC');

-- Tenant A: a device, two of its interfaces, and a service.
INSERT INTO resource (id, tenant_id, site_id, kind, name) VALUES
    ('00000000-0000-0000-0000-0000000000a2',
     '00000000-0000-0000-0000-00000000000a',
     '00000000-0000-0000-0000-0000000000a1', 'device',  'rtr-01'),
    ('00000000-0000-0000-0000-0000000000a3',
     '00000000-0000-0000-0000-00000000000a', NULL, 'interface', 'Gi0/1'),
    ('00000000-0000-0000-0000-0000000000a4',
     '00000000-0000-0000-0000-00000000000a', NULL, 'interface', 'Gi0/2'),
    ('00000000-0000-0000-0000-0000000000a5',
     '00000000-0000-0000-0000-00000000000a', NULL, 'service',   'bgpd');

-- Tenant B: one device, deliberately similar to A's.
INSERT INTO resource (id, tenant_id, site_id, kind, name) VALUES
    ('00000000-0000-0000-0000-0000000000b2',
     '00000000-0000-0000-0000-00000000000b',
     '00000000-0000-0000-0000-0000000000b1', 'device', 'rtr-01');

-- ================================================================
-- Tenant isolation is enforced by the schema, not by convention
-- ================================================================

-- A child in one tenant with a parent in another. No application code intends this,
-- which is exactly why nobody would notice it.
SELECT pg_temp.must_fail($$
    UPDATE resource SET parent_id = '00000000-0000-0000-0000-0000000000b2'
     WHERE id = '00000000-0000-0000-0000-0000000000a3'
$$, '23503');

-- A resource placed at another tenant's site.
SELECT pg_temp.must_fail($$
    UPDATE resource SET site_id = '00000000-0000-0000-0000-0000000000b1'
     WHERE id = '00000000-0000-0000-0000-0000000000a3'
$$, '23503');

-- An edge bridging two tenants' graphs. One of these makes every later traversal a
-- cross-tenant read.
SELECT pg_temp.must_fail($$
    INSERT INTO resource_relationship
        (id, tenant_id, source_id, target_id, kind, discovered_by)
    VALUES ('00000000-0000-0000-0000-0000000000c1',
            '00000000-0000-0000-0000-00000000000a',
            '00000000-0000-0000-0000-0000000000a3',
            '00000000-0000-0000-0000-0000000000b2', 'member_of', 'lldp')
$$, '23503');

-- The same parent, within one tenant, is fine. Isolation must not be over-constraint.
UPDATE resource SET parent_id = '00000000-0000-0000-0000-0000000000a2'
 WHERE id IN ('00000000-0000-0000-0000-0000000000a3',
              '00000000-0000-0000-0000-0000000000a4');

SELECT pg_temp.check(
    (SELECT count(*) FROM resource
      WHERE parent_id = '00000000-0000-0000-0000-0000000000a2') = 2,
    'an interface must be able to belong to its own device');

-- A resource cannot be its own parent: a one-row cycle that every traversal in the
-- product would otherwise have to defend against separately.
SELECT pg_temp.must_fail($$
    UPDATE resource SET parent_id = id
     WHERE id = '00000000-0000-0000-0000-0000000000a2'
$$, '23514');

-- ================================================================
-- The resolution index
-- ================================================================

INSERT INTO resource_identifier
    (id, tenant_id, resource_id, kind, value, confidence, source)
VALUES ('00000000-0000-0000-0000-0000000000d1',
        '00000000-0000-0000-0000-00000000000a',
        '00000000-0000-0000-0000-0000000000a2', 'hostname', 'rtr-01', 0.65, 'syslog');

-- Two resources claiming one hostname inside a tenant is a genuine identity conflict.
-- It must surface as a constraint violation (→ 409, → review queue), never as a
-- silently-overwritten row.
SELECT pg_temp.must_fail($$
    INSERT INTO resource_identifier
        (id, tenant_id, resource_id, kind, value, confidence, source)
    VALUES ('00000000-0000-0000-0000-0000000000d2',
            '00000000-0000-0000-0000-00000000000a',
            '00000000-0000-0000-0000-0000000000a5', 'hostname', 'rtr-01', 0.65, 'otlp')
$$, '23505');

-- But two *tenants* may each have a device called rtr-01, and most will. If this ever
-- fails, the uniqueness constraint has lost its tenant_id and one customer's discovery
-- is blocking another's.
INSERT INTO resource_identifier
    (id, tenant_id, resource_id, kind, value, confidence, source)
VALUES ('00000000-0000-0000-0000-0000000000d3',
        '00000000-0000-0000-0000-00000000000b',
        '00000000-0000-0000-0000-0000000000b2', 'hostname', 'rtr-01', 0.65, 'syslog');

-- Confidence is a probability. Anything else means the noisy-OR combination in
-- uops-core is being fed a number it cannot interpret.
SELECT pg_temp.must_fail($$
    INSERT INTO resource_identifier
        (id, tenant_id, resource_id, kind, value, confidence, source)
    VALUES ('00000000-0000-0000-0000-0000000000d4',
            '00000000-0000-0000-0000-00000000000a',
            '00000000-0000-0000-0000-0000000000a5', 'serial', 'FTX1', 1.5, 'snmp')
$$, '23514');

-- ================================================================
-- Graph traversal terminates, respects depth, and stays in its tenant
-- ================================================================

-- device → interface → service, plus a back edge making a cycle. Real networks contain
-- these; the first one used to hang an API worker until the request timed out.
INSERT INTO resource_relationship
    (id, tenant_id, source_id, target_id, kind, discovered_by)
VALUES
    ('00000000-0000-0000-0000-0000000000e1',
     '00000000-0000-0000-0000-00000000000a',
     '00000000-0000-0000-0000-0000000000a3',
     '00000000-0000-0000-0000-0000000000a2', 'member_of', 'snmp-iftable'),
    ('00000000-0000-0000-0000-0000000000e2',
     '00000000-0000-0000-0000-00000000000a',
     '00000000-0000-0000-0000-0000000000a5',
     '00000000-0000-0000-0000-0000000000a3', 'runs', 'otel'),
    -- the back edge: service → device, closing the loop
    ('00000000-0000-0000-0000-0000000000e3',
     '00000000-0000-0000-0000-00000000000a',
     '00000000-0000-0000-0000-0000000000a2',
     '00000000-0000-0000-0000-0000000000a5', 'depends_on', 'manual');

SELECT pg_temp.check(
    (SELECT count(DISTINCT resource_id)
       FROM resource_dependents('00000000-0000-0000-0000-00000000000a',
                                '00000000-0000-0000-0000-0000000000a2', 16)) = 3,
    'traversal must terminate on a cyclic graph and reach every node once');

SELECT pg_temp.check(
    (SELECT max(depth)
       FROM resource_dependents('00000000-0000-0000-0000-00000000000a',
                                '00000000-0000-0000-0000-0000000000a2', 1)) = 1,
    'max_depth must bound the walk');

SELECT pg_temp.check(
    (SELECT count(*)
       FROM resource_dependents('00000000-0000-0000-0000-00000000000a',
                                '00000000-0000-0000-0000-0000000000a2', 0)) = 1,
    'depth 0 returns the root itself, so callers need not add it back');

-- Knowing a UUID is not authorization. Tenant B's root must yield nothing under tenant
-- A's scope, even though the ID is perfectly valid.
SELECT pg_temp.check(
    (SELECT count(*)
       FROM resource_dependents('00000000-0000-0000-0000-00000000000a',
                                '00000000-0000-0000-0000-0000000000b2', 8)) = 0,
    'a root from another tenant must not be walkable');

-- ================================================================
-- Alias chains collapse on write
-- ================================================================

-- A merged into B.
INSERT INTO resource_alias (tenant_id, historical_id, current_id) VALUES
    ('00000000-0000-0000-0000-00000000000a',
     '00000000-0000-0000-0000-0000000000a4',
     '00000000-0000-0000-0000-0000000000a3');

-- then B merged into C. A must now point at C, not at a resource that is itself gone.
INSERT INTO resource_alias (tenant_id, historical_id, current_id) VALUES
    ('00000000-0000-0000-0000-00000000000a',
     '00000000-0000-0000-0000-0000000000a3',
     '00000000-0000-0000-0000-0000000000a2');

SELECT pg_temp.check(
    (SELECT current_id FROM resource_alias
      WHERE tenant_id = '00000000-0000-0000-0000-00000000000a'
        AND historical_id = '00000000-0000-0000-0000-0000000000a4')
    = '00000000-0000-0000-0000-0000000000a2',
    'A→B then B→C must leave A pointing at C');

-- The property the collapse buys: one lookup is always enough, because no historical
-- ID is ever also a current ID. Every telemetry query depends on this — it is why
-- alias expansion is not a recursive CTE on the hot path.
SELECT pg_temp.check(
    NOT EXISTS (
        SELECT 1 FROM resource_alias a
          JOIN resource_alias b
            ON b.tenant_id = a.tenant_id AND b.historical_id = a.current_id),
    'no alias may point at another alias');

-- Merging into something already merged away collapses on the way in, too.
INSERT INTO resource_alias (tenant_id, historical_id, current_id) VALUES
    ('00000000-0000-0000-0000-00000000000a',
     '00000000-0000-0000-0000-0000000000a5',
     '00000000-0000-0000-0000-0000000000a3');

SELECT pg_temp.check(
    (SELECT current_id FROM resource_alias
      WHERE tenant_id = '00000000-0000-0000-0000-00000000000a'
        AND historical_id = '00000000-0000-0000-0000-0000000000a5')
    = '00000000-0000-0000-0000-0000000000a2',
    'inserting an alias onto a merged-away target must follow the chain');

SELECT pg_temp.must_fail($$
    INSERT INTO resource_alias (tenant_id, historical_id, current_id) VALUES
        ('00000000-0000-0000-0000-00000000000a',
         '00000000-0000-0000-0000-0000000000a2',
         '00000000-0000-0000-0000-0000000000a2')
$$, '23514');

-- ================================================================
-- Credentials
-- ================================================================

INSERT INTO credential
    (id, tenant_id, name, kind, version, kek_id,
     wrapped_dek, dek_nonce, ciphertext, nonce, backend_id)
VALUES ('00000000-0000-0000-0000-0000000000f1',
        '00000000-0000-0000-0000-00000000000a', 'core-switches', 'snmpv3', 1,
        'kek-2026-01', '\x00'::bytea, '\x00'::bytea, '\x00'::bytea, '\x00'::bytea,
        'rustcrypto');

-- Rotation is a new version, not an overwrite.
INSERT INTO credential
    (id, tenant_id, name, kind, version, kek_id,
     wrapped_dek, dek_nonce, ciphertext, nonce, backend_id)
VALUES ('00000000-0000-0000-0000-0000000000f2',
        '00000000-0000-0000-0000-00000000000a', 'core-switches', 'snmpv3', 2,
        'kek-2026-01', '\x00'::bytea, '\x00'::bytea, '\x00'::bytea, '\x00'::bytea,
        'rustcrypto');

SELECT pg_temp.must_fail($$
    INSERT INTO credential
        (id, tenant_id, name, kind, version, kek_id,
         wrapped_dek, dek_nonce, ciphertext, nonce, backend_id)
    VALUES ('00000000-0000-0000-0000-0000000000f3',
            '00000000-0000-0000-0000-00000000000a', 'core-switches', 'snmpv3', 2,
            'kek-2026-01', '\x00'::bytea, '\x00'::bytea, '\x00'::bytea, '\x00'::bytea,
            'rustcrypto')
$$, '23505');

-- A resource must not be able to poll using another tenant's credential. This is the
-- isolation failure with the worst blast radius in the product: it would authenticate
-- one customer's collector against another customer's network.
SELECT pg_temp.must_fail($$
    UPDATE resource SET credential_ref = '00000000-0000-0000-0000-0000000000f1'
     WHERE id = '00000000-0000-0000-0000-0000000000b2'
$$, '23503');

UPDATE resource SET credential_ref = '00000000-0000-0000-0000-0000000000f1'
 WHERE id = '00000000-0000-0000-0000-0000000000a2';

-- The access log outlives what it describes: deleting a credential must not delete the
-- record of who used it.
INSERT INTO credential_access_log
    (tenant_id, credential_id, actor, purpose, succeeded)
VALUES ('00000000-0000-0000-0000-00000000000a',
        '00000000-0000-0000-0000-0000000000f2', 'collector', 'snmp-poll', true),
       ('00000000-0000-0000-0000-00000000000a',
        '00000000-0000-0000-0000-0000000000f2', 'user:someone', 'snmp-poll', false);

DELETE FROM credential WHERE id = '00000000-0000-0000-0000-0000000000f2';

SELECT pg_temp.check(
    (SELECT count(*) FROM credential_access_log
      WHERE credential_id = '00000000-0000-0000-0000-0000000000f2') = 2,
    'access log rows must survive deletion of the credential they describe');

SELECT pg_temp.check(
    (SELECT count(*) FROM credential_access_log WHERE NOT succeeded) = 1,
    'denials must be recorded, not only grants');

-- ================================================================
-- Cascades and timestamps
-- ================================================================

-- Deleting a resource takes its identifiers and edges with it. Orphaned identifiers
-- would keep resolving traffic onto a resource that no longer exists.
DELETE FROM resource_identifier
 WHERE resource_id = '00000000-0000-0000-0000-0000000000a2';
DELETE FROM resource_alias
 WHERE tenant_id = '00000000-0000-0000-0000-00000000000a';
UPDATE resource SET credential_ref = NULL
 WHERE id = '00000000-0000-0000-0000-0000000000a2';
UPDATE resource SET parent_id = NULL
 WHERE tenant_id = '00000000-0000-0000-0000-00000000000a';

INSERT INTO resource_identifier
    (id, tenant_id, resource_id, kind, value, confidence, source)
VALUES ('00000000-0000-0000-0000-0000000000d5',
        '00000000-0000-0000-0000-00000000000a',
        '00000000-0000-0000-0000-0000000000a5', 'serial', 'FTX9', 1.0, 'snmp');

DELETE FROM resource WHERE id = '00000000-0000-0000-0000-0000000000a5';

SELECT pg_temp.check(
    NOT EXISTS (SELECT 1 FROM resource_identifier
                 WHERE id = '00000000-0000-0000-0000-0000000000d5'),
    'identifiers must not outlive their resource');

SELECT pg_temp.check(
    NOT EXISTS (SELECT 1 FROM resource_relationship
                 WHERE source_id = '00000000-0000-0000-0000-0000000000a5'
                    OR target_id = '00000000-0000-0000-0000-0000000000a5'),
    'edges must not outlive their endpoints');

-- updated_at has to be maintained by something, or it silently records creation time
-- forever and every "what changed recently" query is wrong.
--
-- Asserted by writing a lie and watching the trigger overwrite it, rather than by
-- comparing against created_at: now() is transaction-time, so within this one
-- transaction every timestamp is identical and that comparison would prove nothing.
-- Overriding a supplied value is the stronger property anyway — a writer cannot
-- backdate a row.
UPDATE resource SET name = 'rtr-01-renamed',
                    updated_at = timestamptz '2000-01-01 00:00:00Z'
 WHERE id = '00000000-0000-0000-0000-0000000000a2';

SELECT pg_temp.check(
    (SELECT updated_at = now() FROM resource
      WHERE id = '00000000-0000-0000-0000-0000000000a2'),
    'updated_at must be set by the trigger, overriding whatever the writer supplied');

-- ---------------------------------------------------------------------------
-- Resource groups: membership cannot cross a tenant (migration 0011)
-- ---------------------------------------------------------------------------
--
-- The composite foreign keys are the structural half of tenant isolation — the half
-- that does not depend on anyone remembering a WHERE clause. Asserted by trying the
-- thing they exist to refuse.

INSERT INTO resource_group (id, tenant_id, name) VALUES
    ('00000000-0000-0000-0000-0000000000c1',
     '00000000-0000-0000-0000-00000000000a', 'Core Routers');

-- Tenant B may have a group of the same name. Scoped uniqueness, like every other name
-- in this schema: two customers of one MSP both have core routers.
INSERT INTO resource_group (id, tenant_id, name) VALUES
    ('00000000-0000-0000-0000-0000000000d1',
     '00000000-0000-0000-0000-00000000000b', 'Core Routers');

-- Scoped to this fixture's two tenants, not counted globally. An earlier version of
-- this assertion counted every `Core Routers` in the database and passed only while the
-- integration suites had not run — which is the same shared-database contamination that
-- has bitten this project twice already.
SELECT pg_temp.check(
    (SELECT count(*) FROM resource_group
      WHERE name = 'Core Routers'
        AND tenant_id IN ('00000000-0000-0000-0000-00000000000a',
                          '00000000-0000-0000-0000-00000000000b')) = 2,
    'two tenants may each have a group of the same name');

INSERT INTO resource_group_member (tenant_id, group_id, resource_id) VALUES
    ('00000000-0000-0000-0000-00000000000a',
     '00000000-0000-0000-0000-0000000000c1',
     '00000000-0000-0000-0000-0000000000a2');

-- Tenant A's group must not be able to contain tenant B's resource, even though both
-- uuids exist and the inserting tenant_id is A's own.
DO $$
BEGIN
    INSERT INTO resource_group_member (tenant_id, group_id, resource_id) VALUES
        ('00000000-0000-0000-0000-00000000000a',
         '00000000-0000-0000-0000-0000000000c1',
         '00000000-0000-0000-0000-0000000000b2');
    RAISE EXCEPTION 'FAILED: a group must not be able to contain another tenant''s resource';
EXCEPTION WHEN foreign_key_violation THEN
    NULL;
END $$;

-- And the mirror: naming tenant B's group with tenant A's id is refused by the same key.
DO $$
BEGIN
    INSERT INTO resource_group_member (tenant_id, group_id, resource_id) VALUES
        ('00000000-0000-0000-0000-00000000000a',
         '00000000-0000-0000-0000-0000000000d1',
         '00000000-0000-0000-0000-0000000000a2');
    RAISE EXCEPTION 'FAILED: a member row must not reach a group in another tenant';
EXCEPTION WHEN foreign_key_violation THEN
    NULL;
END $$;

-- Removing a resource removes its memberships. A group listing a resource that no
-- longer exists would break every alert scoped to it, one row at a time.
DELETE FROM resource WHERE id = '00000000-0000-0000-0000-0000000000a2';
SELECT pg_temp.check(
    NOT EXISTS (SELECT 1 FROM resource_group_member
                 WHERE resource_id = '00000000-0000-0000-0000-0000000000a2'),
    'membership must not outlive the resource');

-- ---------------------------------------------------------------------------
-- Tags are a flat string map (migration 0011)
-- ---------------------------------------------------------------------------
--
-- A nested tag is not a tag. Without this, a routing rule silently ignores `owner.team`
-- because it is an object, and nothing reports it.

UPDATE resource SET tags = '{"environment": "production", "criticality": "critical"}'
 WHERE id = '00000000-0000-0000-0000-0000000000a3';

SELECT pg_temp.check(
    (SELECT tags ->> 'environment' FROM resource
      WHERE id = '00000000-0000-0000-0000-0000000000a3') = 'production',
    'a flat string map is accepted');

DO $$
BEGIN
    UPDATE resource SET tags = '{"owner": {"team": "network"}}'
     WHERE id = '00000000-0000-0000-0000-0000000000a3';
    RAISE EXCEPTION 'FAILED: a nested tag value must be refused';
EXCEPTION WHEN check_violation THEN
    NULL;
END $$;

DO $$
BEGIN
    UPDATE resource SET tags = '{"replicas": 3}'
     WHERE id = '00000000-0000-0000-0000-0000000000a3';
    RAISE EXCEPTION 'FAILED: a non-string tag value must be refused';
EXCEPTION WHEN check_violation THEN
    NULL;
END $$;

-- Tags and attributes are separate columns, which is the entire point: discovery writes
-- one and a human writes the other, and neither can silently overwrite the other's work.
SELECT pg_temp.check(
    (SELECT tags <> attributes FROM resource
      WHERE id = '00000000-0000-0000-0000-0000000000a3'),
    'tags and attributes must be distinct columns');

-- ---------------------------------------------------------------------------
-- Maintenance windows (migration 0012)
-- ---------------------------------------------------------------------------

-- Exactly one target. Two would be ambiguous and zero would be a window that silences
-- nothing while looking like it silences something, which is worse.
DO $$
BEGIN
    INSERT INTO maintenance_window
        (tenant_id, reason, starts_at, duration_minutes, timezone, recurrence)
    VALUES ('00000000-0000-0000-0000-00000000000a', 'nothing at all',
            now(), 60, 'UTC', 'once');
    RAISE EXCEPTION 'FAILED: a window with no target must be refused';
EXCEPTION WHEN check_violation THEN
    NULL;
END $$;

DO $$
BEGIN
    INSERT INTO maintenance_window
        (tenant_id, reason, target_resource_id, target_site_id,
         starts_at, duration_minutes, timezone, recurrence)
    VALUES ('00000000-0000-0000-0000-00000000000a', 'two targets',
            '00000000-0000-0000-0000-0000000000a3',
            '00000000-0000-0000-0000-0000000000a1',
            now(), 60, 'UTC', 'once');
    RAISE EXCEPTION 'FAILED: a window with two targets must be refused';
EXCEPTION WHEN check_violation THEN
    NULL;
END $$;

-- A window must not reach across tenants. The composite foreign key is what refuses it,
-- not a predicate anybody has to remember.
DO $$
BEGIN
    INSERT INTO maintenance_window
        (tenant_id, reason, target_resource_id,
         starts_at, duration_minutes, timezone, recurrence)
    VALUES ('00000000-0000-0000-0000-00000000000a', 'someone else''s device',
            '00000000-0000-0000-0000-0000000000b2',
            now(), 60, 'UTC', 'once');
    RAISE EXCEPTION 'FAILED: a window must not target another tenant''s resource';
EXCEPTION WHEN foreign_key_violation THEN
    NULL;
END $$;

-- A recurrence and its parameter have to agree. Without this a 'weekly' window with a
-- NULL weekday is storable and silently never opens: a window an operator created, can
-- see in the UI, and which does nothing.
DO $$
BEGIN
    INSERT INTO maintenance_window
        (tenant_id, reason, target_site_id,
         starts_at, duration_minutes, timezone, recurrence)
    VALUES ('00000000-0000-0000-0000-00000000000a', 'weekly with no weekday',
            '00000000-0000-0000-0000-0000000000a1',
            now(), 60, 'UTC', 'weekly');
    RAISE EXCEPTION 'FAILED: a weekly window needs a weekday';
EXCEPTION WHEN check_violation THEN
    NULL;
END $$;

DO $$
BEGIN
    INSERT INTO maintenance_window
        (tenant_id, reason, target_site_id,
         starts_at, duration_minutes, timezone, recurrence, recur_weekday)
    VALUES ('00000000-0000-0000-0000-00000000000a', 'once with a weekday',
            '00000000-0000-0000-0000-0000000000a1',
            now(), 60, 'UTC', 'once', 5);
    RAISE EXCEPTION 'FAILED: a one-off window must not carry a weekday';
EXCEPTION WHEN check_violation THEN
    NULL;
END $$;

-- A month of silence is almost always a mis-typed end date, and the consequence is an
-- estate that stops alerting with nobody noticing.
DO $$
BEGIN
    INSERT INTO maintenance_window
        (tenant_id, reason, target_site_id,
         starts_at, duration_minutes, timezone, recurrence)
    VALUES ('00000000-0000-0000-0000-00000000000a', 'a whole month',
            '00000000-0000-0000-0000-0000000000a1',
            now(), 30 * 24 * 60, 'UTC', 'once');
    RAISE EXCEPTION 'FAILED: a window longer than a week must be refused';
EXCEPTION WHEN check_violation THEN
    NULL;
END $$;

-- A window with no reason is one nobody dares delete six months later, so it goes on
-- silencing alerts forever.
DO $$
BEGIN
    INSERT INTO maintenance_window
        (tenant_id, reason, target_site_id,
         starts_at, duration_minutes, timezone, recurrence)
    VALUES ('00000000-0000-0000-0000-00000000000a', '   ',
            '00000000-0000-0000-0000-0000000000a1',
            now(), 60, 'UTC', 'once');
    RAISE EXCEPTION 'FAILED: a window needs a reason';
EXCEPTION WHEN check_violation THEN
    NULL;
END $$;

-- The valid shapes, one per target kind, so the constraints above are proven to refuse
-- rather than to refuse everything.
INSERT INTO maintenance_window
    (id, tenant_id, reason, target_site_id,
     starts_at, duration_minutes, timezone, recurrence, recur_weekday)
VALUES ('00000000-0000-0000-0000-0000000000e1',
        '00000000-0000-0000-0000-00000000000a', 'Saturday change window',
        '00000000-0000-0000-0000-0000000000a1',
        now(), 120, 'Asia/Dhaka', 'weekly', 5);

INSERT INTO maintenance_window
    (tenant_id, reason, target_group_id, starts_at, duration_minutes, timezone, recurrence)
VALUES ('00000000-0000-0000-0000-00000000000a', 'core router firmware',
        '00000000-0000-0000-0000-0000000000c1', now(), 60, 'UTC', 'once');

SELECT pg_temp.check(
    (SELECT count(*) FROM maintenance_window
      WHERE tenant_id = '00000000-0000-0000-0000-00000000000a') = 2,
    'the valid shapes must be accepted');

-- Deleting the thing a window covers deletes the window. A window pointing at a site
-- that no longer exists cannot be evaluated and cannot be found in any UI, so it would
-- sit in the table forever.
DELETE FROM site WHERE id = '00000000-0000-0000-0000-0000000000a1';
SELECT pg_temp.check(
    NOT EXISTS (SELECT 1 FROM maintenance_window
                 WHERE id = '00000000-0000-0000-0000-0000000000e1'),
    'a window must not outlive its target');

-- ---------------------------------------------------------------- saved searches
--
-- The properties migration 0013 claims: a stored search is an answerable question, its
-- denormalised signal cannot lie about the AST it came from, and a name belongs to one
-- tenant rather than to the installation.

SELECT pg_temp.must_fail($$
    INSERT INTO saved_search (tenant_id, name, signal, query)
    VALUES ('00000000-0000-0000-0000-00000000000a', 'not an object', 'log',
            '"just a string"'::jsonb)
$$, '23514');

-- The denormalisation that pays for the list view has to be kept honest by the database,
-- or it is just a second copy of a field that will eventually disagree with the first.
SELECT pg_temp.must_fail($$
    INSERT INTO saved_search (tenant_id, name, signal, query)
    VALUES ('00000000-0000-0000-0000-00000000000a', 'disagrees with itself', 'metric',
            '{"signal":"log"}'::jsonb)
$$, '23514');

-- A search over a signal with no table behind it could be saved, listed, opened, and
-- never run. `trace` is in the AST and the compiler refuses it until M8.
SELECT pg_temp.must_fail($$
    INSERT INTO saved_search (tenant_id, name, signal, query)
    VALUES ('00000000-0000-0000-0000-00000000000a', 'traces', 'trace',
            '{"signal":"trace"}'::jsonb)
$$, '23514');

SELECT pg_temp.must_fail($$
    INSERT INTO saved_search (tenant_id, name, signal, query)
    VALUES ('00000000-0000-0000-0000-00000000000a', '   ', 'log', '{"signal":"log"}'::jsonb)
$$, '23514');

INSERT INTO saved_search (tenant_id, name, signal, query) VALUES
    ('00000000-0000-0000-0000-00000000000a', 'BGP flaps', 'log', '{"signal":"log"}'::jsonb);

-- The same name again in the same tenant is a conflict...
SELECT pg_temp.must_fail($$
    INSERT INTO saved_search (tenant_id, name, signal, query)
    VALUES ('00000000-0000-0000-0000-00000000000a', 'BGP flaps', 'log',
            '{"signal":"log"}'::jsonb)
$$, '23505');

-- ...and in the other customer's tenant it is simply their own search. Names are scoped
-- to a tenant, not to the installation: an MSP running this for forty customers would
-- otherwise have the first of them claim "BGP flaps" for everyone.
INSERT INTO saved_search (tenant_id, name, signal, query) VALUES
    ('00000000-0000-0000-0000-00000000000b', 'BGP flaps', 'log', '{"signal":"log"}'::jsonb);

SELECT pg_temp.check(
    (SELECT count(*) FROM saved_search WHERE name = 'BGP flaps') = 2,
    'a search name belongs to a tenant, not to the installation');

-- Removing a customer removes their searches with them. Asserted on the constraint
-- rather than by deleting the tenant: `site` deliberately has no cascade — a tenant with
-- sites cannot be deleted at all — so the only way to exercise this one by hand would be
-- to dismantle every fixture above it first, which tests the fixtures rather than this.
SELECT pg_temp.check(
    (SELECT confdeltype
       FROM pg_constraint
      WHERE conrelid = 'saved_search'::regclass
        AND confrelid = 'tenant'::regclass
        AND contype = 'f') = 'c',
    'a saved search must not outlive its tenant');

-- ---------------------------------------------------------------- alerting
--
-- The properties migration 0014 claims: a rule's kind cannot lie about its condition, an
-- evaluation interval is bounded, one series has one alert, and a rule's state cannot
-- reach across tenants.

SELECT pg_temp.must_fail($$
    INSERT INTO alert_rule (tenant_id, name, kind, query, condition, severity)
    VALUES ('00000000-0000-0000-0000-00000000000a', 'disagrees', 'absence',
            '{"signal":"metric"}'::jsonb,
            '{"kind":"threshold","op":"gt","value":90,"hold_seconds":300}'::jsonb,
            'critical')
$$, '23514');

SELECT pg_temp.must_fail($$
    INSERT INTO alert_rule (tenant_id, name, kind, query, condition, severity)
    VALUES ('00000000-0000-0000-0000-00000000000a', 'shouting', 'absence',
            '{"signal":"metric"}'::jsonb, '{"kind":"absence","after_seconds":300}'::jsonb,
            'emergency')
$$, '23514');

-- A rule evaluating every second spends the whole cycle budget on itself; SPEC's target
-- is 1 000 rules inside 60 seconds.
SELECT pg_temp.must_fail($$
    INSERT INTO alert_rule (tenant_id, name, kind, query, condition, severity, eval_interval)
    VALUES ('00000000-0000-0000-0000-00000000000a', 'too eager', 'absence',
            '{"signal":"metric"}'::jsonb, '{"kind":"absence","after_seconds":300}'::jsonb,
            'warning', interval '1 second')
$$, '23514');

-- Its own resources, one per tenant. The sections above delete and re-point the shared
-- fixtures, and a test that depends on what an earlier section left behind fails for a
-- reason that has nothing to do with what it is testing.
INSERT INTO resource (id, tenant_id, kind, name) VALUES
    ('00000000-0000-0000-0000-0000000000d1',
     '00000000-0000-0000-0000-00000000000a', 'device', 'alerted-01'),
    ('00000000-0000-0000-0000-0000000000d2',
     '00000000-0000-0000-0000-00000000000b', 'device', 'theirs-01');

INSERT INTO alert_rule (id, tenant_id, name, kind, query, condition, severity) VALUES
    ('00000000-0000-0000-0000-0000000000f1', '00000000-0000-0000-0000-00000000000a',
     'CPU hot', 'threshold', '{"signal":"metric"}'::jsonb,
     '{"kind":"threshold","op":"gt","value":90,"hold_seconds":300}'::jsonb, 'critical');

INSERT INTO alert_state
    (tenant_id, rule_id, resource_id, dedup_key, state, since, last_eval)
VALUES ('00000000-0000-0000-0000-00000000000a',
        '00000000-0000-0000-0000-0000000000f1',
        '00000000-0000-0000-0000-0000000000d1',
        'f1/d1', 'firing', now(), now());

-- One series, one alert. Two evaluators racing — a restart overlapping its predecessor —
-- must update one row rather than create a second alert about one problem.
SELECT pg_temp.must_fail($$
    INSERT INTO alert_state
        (tenant_id, rule_id, resource_id, dedup_key, state, since, last_eval)
    VALUES ('00000000-0000-0000-0000-00000000000a',
            '00000000-0000-0000-0000-0000000000f1',
            '00000000-0000-0000-0000-0000000000d1',
            'f1/d1', 'pending', now(), now())
$$, '23505');

-- An acknowledgement is a person and a time, or it is neither.
SELECT pg_temp.must_fail($$
    UPDATE alert_state SET acked_at = now() WHERE dedup_key = 'f1/d1'
$$, '23514');

-- The other customer's resource cannot be given this tenant's alert. The composite
-- foreign key is what refuses it, not a predicate anybody has to remember.
SELECT pg_temp.must_fail($$
    INSERT INTO alert_state
        (tenant_id, rule_id, resource_id, dedup_key, state, since, last_eval)
    VALUES ('00000000-0000-0000-0000-00000000000a',
            '00000000-0000-0000-0000-0000000000f1',
            '00000000-0000-0000-0000-0000000000d2',
            'f1/d2', 'firing', now(), now())
$$, '23503');

-- Deleting a rule deletes what it believed. State rows naming a rule nobody can look up
-- are alerts in the UI that cannot be explained, acknowledged or silenced.
DELETE FROM alert_rule WHERE id = '00000000-0000-0000-0000-0000000000f1';
SELECT pg_temp.check(
    NOT EXISTS (SELECT 1 FROM alert_state WHERE dedup_key = 'f1/d1'),
    'alert state must not outlive its rule');

-- ---------------------------------------------------------------- notifications
--
-- The properties migration 0015 claims: a channel is one of the kinds that exist, its
-- rate is bounded, every attempt records an outcome that means something, and the record
-- of having woken somebody up cannot reach across tenants.

SELECT pg_temp.must_fail($$
    INSERT INTO notification_channel (tenant_id, name, kind, config)
    VALUES ('00000000-0000-0000-0000-00000000000a', 'carrier pigeon', 'pigeon', '{}'::jsonb)
$$, '23514');

SELECT pg_temp.must_fail($$
    INSERT INTO notification_channel (tenant_id, name, kind, config)
    VALUES ('00000000-0000-0000-0000-00000000000a', 'not an object', 'webhook', '[]'::jsonb)
$$, '23514');

-- A channel with no rate at all is one that exists and can never deliver, which is what
-- `enabled` says more clearly.
SELECT pg_temp.must_fail($$
    INSERT INTO notification_channel (tenant_id, name, kind, config, max_per_minute)
    VALUES ('00000000-0000-0000-0000-00000000000a', 'silent', 'webhook', '{}'::jsonb, 0)
$$, '23514');

INSERT INTO notification_channel (id, tenant_id, name, kind, config) VALUES
    ('00000000-0000-0000-0000-0000000000c9', '00000000-0000-0000-0000-00000000000a',
     'ops webhook', 'webhook', '{"url":"http://example.invalid/hook"}'::jsonb);

-- The budget is a number on the tenant, and it is bounded.
SELECT pg_temp.check(
    (SELECT notification_budget_per_day FROM tenant
      WHERE id = '00000000-0000-0000-0000-00000000000a') = 1000,
    'a tenant has a notification budget by default');
SELECT pg_temp.must_fail($$
    UPDATE tenant SET notification_budget_per_day = -1
     WHERE id = '00000000-0000-0000-0000-00000000000a'
$$, '23514');

-- Every attempt is recorded, including the refusals — a refusal that leaves no trace is
-- indistinguishable from a rule that never fired.
INSERT INTO notification_sent
    (tenant_id, channel_id, rule_id, dedup_key, phase, outcome)
VALUES ('00000000-0000-0000-0000-00000000000a',
        '00000000-0000-0000-0000-0000000000c9',
        '00000000-0000-0000-0000-0000000000f2', 'r/d', 'firing', 'rate_limited');

SELECT pg_temp.must_fail($$
    INSERT INTO notification_sent
        (tenant_id, channel_id, rule_id, dedup_key, phase, outcome)
    VALUES ('00000000-0000-0000-0000-00000000000a',
            '00000000-0000-0000-0000-0000000000c9',
            '00000000-0000-0000-0000-0000000000f2', 'r/d', 'firing', 'lost')
$$, '23514');

-- `pending` is not a phase anybody is told about, so it cannot be recorded as one.
SELECT pg_temp.must_fail($$
    INSERT INTO notification_sent
        (tenant_id, channel_id, rule_id, dedup_key, phase, outcome)
    VALUES ('00000000-0000-0000-0000-00000000000a',
            '00000000-0000-0000-0000-0000000000c9',
            '00000000-0000-0000-0000-0000000000f2', 'r/d', 'pending', 'sent')
$$, '23514');

-- The other customer's channel cannot be sent to on this tenant's behalf. The composite
-- foreign key is what refuses it.
SELECT pg_temp.must_fail($$
    INSERT INTO notification_sent
        (tenant_id, channel_id, rule_id, dedup_key, phase, outcome)
    VALUES ('00000000-0000-0000-0000-00000000000b',
            '00000000-0000-0000-0000-0000000000c9',
            '00000000-0000-0000-0000-0000000000f2', 'r/d', 'firing', 'sent')
$$, '23503');

-- Deleting a channel takes its history with it. The alternative is rows naming a channel
-- nobody can look up, in the table somebody reads to find out why they were paged.
DELETE FROM notification_channel WHERE id = '00000000-0000-0000-0000-0000000000c9';
SELECT pg_temp.check(
    NOT EXISTS (SELECT 1 FROM notification_sent
                 WHERE channel_id = '00000000-0000-0000-0000-0000000000c9'),
    'a delivery record must not outlive its channel');

-- ---------------------------------------------------------------- dashboards
--
-- The properties migration 0016 claims: a dashboard is an ordered list of panels, it is a
-- screenful rather than an archive, and its name belongs to its tenant.

SELECT pg_temp.must_fail($$
    INSERT INTO dashboard (tenant_id, name, panels)
    VALUES ('00000000-0000-0000-0000-00000000000a', 'not a list', '{}'::jsonb)
$$, '23514');

-- Every panel is a telemetry query, so this is also the limit on what one page load asks
-- of ClickHouse.
SELECT pg_temp.must_fail($$
    INSERT INTO dashboard (tenant_id, name, panels)
    VALUES ('00000000-0000-0000-0000-00000000000a', 'too many',
            (SELECT jsonb_agg(jsonb_build_object('id', n::text))
               FROM generate_series(1, 41) AS n))
$$, '23514');

INSERT INTO dashboard (tenant_id, name, panels) VALUES
    ('00000000-0000-0000-0000-00000000000a', 'Core routers', '[]'::jsonb);

SELECT pg_temp.must_fail($$
    INSERT INTO dashboard (tenant_id, name)
    VALUES ('00000000-0000-0000-0000-00000000000a', 'Core routers')
$$, '23505');

-- The same name in the other customer's tenant is simply their own dashboard.
INSERT INTO dashboard (tenant_id, name) VALUES
    ('00000000-0000-0000-0000-00000000000b', 'Core routers');
SELECT pg_temp.check(
    (SELECT count(*) FROM dashboard WHERE name = 'Core routers') = 2,
    'a dashboard name belongs to a tenant, not to the installation');

-- ---------------------------------------------------------------- discovery
--
-- The properties migration 0017 claims: a sweep is bounded by the schema and not only by
-- the application, a run's counters cannot describe something that did not happen, and a
-- candidate is a thing rather than a sighting.

-- §2.3. A /8 is 16 million addresses, and an operator who types one means "everything",
-- which is not a range.
SELECT pg_temp.must_fail($$
    INSERT INTO discovery_job (tenant_id, name, ranges, credential_refs)
    VALUES ('00000000-0000-0000-0000-00000000000a', 'the internet',
            ARRAY['10.0.0.0/8']::cidr[],
            ARRAY['00000000-0000-0000-0000-0000000000c1']::uuid[])
$$, '23514');

-- And ten legal /16s are refused for the same reason one illegal /12 is: the cap is on
-- the job, not on the prettiest range in it.
SELECT pg_temp.must_fail($$
    INSERT INTO discovery_job (tenant_id, name, ranges, credential_refs)
    VALUES ('00000000-0000-0000-0000-00000000000a', 'ten sixteens',
            (SELECT array_agg(('10.' || n || '.0.0/16')::cidr)
               FROM generate_series(1, 10) AS n),
            ARRAY['00000000-0000-0000-0000-0000000000c1']::uuid[])
$$, '23514');

-- A job with no credential does not probe quietly and report an empty estate; it is
-- refused. §2.2 — supplied, never guessed, and the empty list is not a licence to guess.
SELECT pg_temp.must_fail($$
    INSERT INTO discovery_job (tenant_id, name, ranges, credential_refs)
    VALUES ('00000000-0000-0000-0000-00000000000a', 'no credentials',
            ARRAY['192.168.1.0/24']::cidr[], ARRAY[]::uuid[])
$$, '23514');

SELECT pg_temp.must_fail($$
    INSERT INTO discovery_job (tenant_id, name, ranges, credential_refs)
    VALUES ('00000000-0000-0000-0000-00000000000a', 'nowhere',
            ARRAY[]::cidr[],
            ARRAY['00000000-0000-0000-0000-0000000000c1']::uuid[])
$$, '23514');

-- IPv6 is not swept. The address-count arithmetic means nothing on a /64, and a /64 is
-- the smallest thing anybody assigns.
SELECT pg_temp.must_fail($$
    INSERT INTO discovery_job (tenant_id, name, ranges, credential_refs)
    VALUES ('00000000-0000-0000-0000-00000000000a', 'v6',
            ARRAY['2001:db8::/64']::cidr[],
            ARRAY['00000000-0000-0000-0000-0000000000c1']::uuid[])
$$, '23514');

-- A sweep every minute is traffic a security team will ask about, teaching nothing: the
-- estate does not change that often.
SELECT pg_temp.must_fail($$
    INSERT INTO discovery_job (tenant_id, name, ranges, credential_refs, schedule)
    VALUES ('00000000-0000-0000-0000-00000000000a', 'relentless',
            ARRAY['192.168.1.0/24']::cidr[],
            ARRAY['00000000-0000-0000-0000-0000000000c1']::uuid[], interval '1 minute')
$$, '23514');

INSERT INTO discovery_job (id, tenant_id, name, ranges, credential_refs, schedule) VALUES
    ('00000000-0000-0000-0000-0000000000e1', '00000000-0000-0000-0000-00000000000a',
     'Branch offices', ARRAY['192.168.1.0/24', '192.168.2.0/24']::cidr[],
     ARRAY['00000000-0000-0000-0000-0000000000c1']::uuid[], interval '1 day');

SELECT pg_temp.check(
    (SELECT discovery_address_count(ranges) FROM discovery_job
      WHERE id = '00000000-0000-0000-0000-0000000000e1') = 512,
    'two /24s are 512 addresses');

SELECT pg_temp.must_fail($$
    INSERT INTO discovery_job (tenant_id, name, ranges, credential_refs)
    VALUES ('00000000-0000-0000-0000-00000000000a', 'Branch offices',
            ARRAY['10.1.0.0/24']::cidr[],
            ARRAY['00000000-0000-0000-0000-0000000000c1']::uuid[])
$$, '23505');

-- A run belongs to a job in its own tenant. The composite key is what makes guessing the
-- uuid useless rather than merely unlikely.
SELECT pg_temp.must_fail($$
    INSERT INTO discovery_run (tenant_id, job_id, ranges, trigger)
    VALUES ('00000000-0000-0000-0000-00000000000b',
            '00000000-0000-0000-0000-0000000000e1',
            ARRAY['192.168.1.0/24']::cidr[], 'schedule')
$$, '23503');

-- A run that is over has an end, and one that is not does not. This pair is what makes
-- "which runs are stuck" answerable without a heuristic about age.
SELECT pg_temp.must_fail($$
    INSERT INTO discovery_run (tenant_id, ranges, trigger, status)
    VALUES ('00000000-0000-0000-0000-00000000000a', ARRAY['192.168.1.0/24']::cidr[],
            'manual', 'succeeded')
$$, '23514');

SELECT pg_temp.must_fail($$
    INSERT INTO discovery_run (tenant_id, ranges, trigger, status, finished_at)
    VALUES ('00000000-0000-0000-0000-00000000000a', ARRAY['192.168.1.0/24']::cidr[],
            'manual', 'failed', now())
$$, '23514');

-- 300 devices answering a sweep of 254 addresses is a counter incremented on the wrong
-- path, and it is the kind of number that makes an operator distrust the whole screen.
SELECT pg_temp.must_fail($$
    INSERT INTO discovery_run (tenant_id, ranges, trigger, probed, answered)
    VALUES ('00000000-0000-0000-0000-00000000000a', ARRAY['192.168.1.0/24']::cidr[],
            'manual', 254, 300)
$$, '23514');

INSERT INTO discovery_run (id, tenant_id, job_id, ranges, trigger, probed, answered) VALUES
    ('00000000-0000-0000-0000-0000000000e2', '00000000-0000-0000-0000-00000000000a',
     '00000000-0000-0000-0000-0000000000e1', ARRAY['192.168.1.0/24']::cidr[],
     'schedule', 254, 9);

-- A candidate is a thing that exists. One with neither an address nor a chassis ID is an
-- empty row, and its fingerprint would silently collide with every other empty row.
SELECT pg_temp.must_fail($$
    INSERT INTO discovery_candidate (tenant_id, source)
    VALUES ('00000000-0000-0000-0000-00000000000a', 'sweep')
$$, '23514');

SELECT pg_temp.must_fail($$
    INSERT INTO discovery_candidate (tenant_id, source, address, state)
    VALUES ('00000000-0000-0000-0000-00000000000a', 'sweep', '192.168.1.9', 'promoted')
$$, '23514');

-- §2.5: a port and a neighbour are things a neighbour table reports. A sweep has neither,
-- and a row claiming otherwise is a bug in whichever writer produced it.
SELECT pg_temp.must_fail($$
    INSERT INTO discovery_candidate (tenant_id, source, address, port_id)
    VALUES ('00000000-0000-0000-0000-00000000000a', 'sweep', '192.168.1.9', 'Gi0/1')
$$, '23514');

INSERT INTO discovery_candidate
    (tenant_id, last_run_id, source, address, sys_descr, reason)
VALUES
    ('00000000-0000-0000-0000-00000000000a', '00000000-0000-0000-0000-0000000000e2',
     'sweep', '192.168.1.9', 'HP LaserJet', 'no monitoring profile matches this device');

-- One row per thing, not per sighting. The second nightly sweep updates rather than
-- adding a second printer.
SELECT pg_temp.must_fail($$
    INSERT INTO discovery_candidate (tenant_id, source, address)
    VALUES ('00000000-0000-0000-0000-00000000000a', 'sweep', '192.168.1.9')
$$, '23505');

-- The same address seen by a neighbour table is a different sighting of possibly the same
-- device, and deciding that is identity resolution's job rather than a unique index's.
INSERT INTO discovery_candidate (tenant_id, source, address, chassis_id, port_id)
VALUES ('00000000-0000-0000-0000-00000000000a', 'lldp', '192.168.1.9',
        '00:1b:21:3c:4d:5e', 'GigabitEthernet0/1');
SELECT pg_temp.check(
    (SELECT count(*) FROM discovery_candidate WHERE address = '192.168.1.9') = 2,
    'a sweep sighting and an LLDP sighting are two candidates, not one');

-- Deleting a job must not delete the record that it once scanned somebody's network.
-- §2.7 is the whole reason `discovery_run` exists separately.
DELETE FROM discovery_job WHERE id = '00000000-0000-0000-0000-0000000000e1';
SELECT pg_temp.check(
    (SELECT job_id IS NULL FROM discovery_run
      WHERE id = '00000000-0000-0000-0000-0000000000e2'),
    'deleting a discovery job keeps the runs that recorded what it scanned');

-- Every foreign key has an index on its referencing side.
--
-- PostgreSQL indexes the referenced side automatically and the referencing side never,
-- so an unindexed one makes every parent DELETE or key UPDATE scan the whole child
-- table, once per row. It stays invisible until somebody deletes in bulk — a
-- decommissioned site, a removed customer, a retention job — and then the cost is the
-- product of two table sizes. Migration 0010 has the story; this is the guard that stops
-- the next foreign key being added without one.
--
-- Coverage, not exact shape: an index on (a, b, c) serves a key of (a, b), which is why
-- this compares a prefix of indkey rather than equality.
SELECT pg_temp.check(
    NOT EXISTS (
        SELECT 1
          FROM pg_constraint c
         WHERE c.contype = 'f'
           AND NOT EXISTS (
               SELECT 1
                 FROM pg_index i
                WHERE i.indrelid = c.conrelid
                  AND (i.indkey::smallint[])[0:array_length(c.conkey, 1) - 1] @> c.conkey
           )
    ),
    'every foreign key needs an index on the referencing side');

-- No ON DELETE SET NULL may try to null a NOT NULL column.
--
-- A composite foreign key nulls *every* column in the key, not the one that pointed at
-- the deleted row. Since migration 0002 nearly every intra-tenant reference here is
-- `(thing_id, tenant_id)`, so the default behaviour sets `tenant_id` to NULL too — and
-- `tenant_id` is NOT NULL everywhere by design. The delete then fails with a not-null
-- violation on a table the operator was not looking at, and only when somebody finally
-- deletes a parent row in production.
--
-- Migration 0017 was written this way and this test is what found it. The fix is the
-- column list — `ON DELETE SET NULL (job_id)` — which PostgreSQL 15 added and which
-- PLAN's floor of 16 therefore allows. This guard is the reason the next one cannot be
-- written the old way.
SELECT pg_temp.check(
    NOT EXISTS (
        SELECT 1
          FROM pg_constraint c
          JOIN pg_attribute a
            ON a.attrelid = c.conrelid
           AND a.attnum = ANY (coalesce(c.confdelsetcols, c.conkey))
         WHERE c.contype = 'f'
           AND c.confdeltype = 'n'
           AND a.attnotnull
    ),
    'ON DELETE SET NULL must name its columns rather than nulling a NOT NULL one');

ROLLBACK;

\echo 'schema invariants: OK'
