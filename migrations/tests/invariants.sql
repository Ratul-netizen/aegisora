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

ROLLBACK;

\echo 'schema invariants: OK'
