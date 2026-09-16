-- Indexes on the referencing side of every foreign key that did not have one.
--
-- PostgreSQL indexes the *referenced* side automatically — it has to, the target is a
-- primary key or a unique constraint — and indexes the referencing side never. So every
-- DELETE or key UPDATE on the parent runs a sequential scan of each child table to check
-- the constraint, once per row.
--
-- That is invisible until it is not. Deleting one resource scans `resource_alias` and
-- `identity_decision` once, which is nothing; deleting ten thousand scans them ten
-- thousand times, and the cost of the whole operation is the product of two table sizes.
--
-- # How this was found
--
-- The API scale test seeds 10 000 resources, measures pagination, and removes them. It
-- began failing its own clean-up with `57014 canceling statement due to statement
-- timeout`, inside:
--
--   SELECT 1 FROM ONLY "resource_alias" x
--    WHERE $1 = "current_id" AND $2 = "tenant_id" FOR KEY SHARE OF x
--
-- which is PostgreSQL checking `resource_alias_current_id_tenant_id_fkey`. The test
-- passes when run alone and fails under a loaded server, which is the signature of
-- something quadratic rather than something broken: it is always doing the scans, and it
-- only runs out of time when the server is busy.
--
-- # Why this is a product fix and not a test fix
--
-- The test does what an operator does. Decommissioning a site, removing a customer, or
-- an M12 retention job pruning resources that have not been seen in a year are all bulk
-- deletes against these same constraints, and every one of them was quadratic.
--
-- `user_tenant_role_tenant_id_fkey` is the sharpest of them: it is `ON DELETE CASCADE`,
-- so removing a tenant scans every role grant in the installation. `db.sh sweep` does
-- exactly that, in a loop.
--
-- # What is deliberately not here
--
-- No index is added for a foreign key whose leading columns are already covered — the
-- audit below is the query that decided, and it checks coverage rather than exact shape,
-- because an index on (a, b, c) serves a key of (a, b).
--
--   SELECT c.conrelid::regclass, c.conname
--     FROM pg_constraint c
--    WHERE c.contype = 'f'
--      AND NOT EXISTS (
--          SELECT 1 FROM pg_index i
--           WHERE i.indrelid = c.conrelid
--             AND (i.indkey::smallint[])[0:array_length(c.conkey, 1) - 1] @> c.conkey
--      );
--
-- It returns nothing after this migration, and `migrations/tests/` asserts that.

-- Merges: the child of resource. Also the one that was actually timing out.
CREATE INDEX resource_alias_current_idx
    ON resource_alias (current_id, tenant_id);

-- Merges: the child of identity_decision.
CREATE INDEX resource_alias_decision_idx
    ON resource_alias (decision_id);

-- Every resolution decision points at the resource it landed on. ON DELETE SET NULL, so
-- a resource delete rewrites these rows rather than refusing — and had to find them by
-- scanning first.
CREATE INDEX identity_decision_resource_idx
    ON identity_decision (resource_id, tenant_id);

-- A resource's site, credential and profile. All three are parents that get deleted:
-- decommissioning a site, revoking a credential, removing a profile.
CREATE INDEX resource_site_idx
    ON resource (site_id, tenant_id);

CREATE INDEX resource_credential_idx
    ON resource (credential_ref, tenant_id);

CREATE INDEX resource_profile_idx
    ON resource (profile_id);

-- Role grants. ON DELETE CASCADE from tenant, so removing a tenant scanned every grant
-- in the installation; `granted_by` points at the user who made the grant and is scanned
-- when a user is removed.
CREATE INDEX user_tenant_role_tenant_idx
    ON user_tenant_role (tenant_id);

CREATE INDEX user_tenant_role_granted_by_idx
    ON user_tenant_role (granted_by);
