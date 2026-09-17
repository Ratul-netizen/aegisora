-- A composite ON DELETE SET NULL nulls the whole key, including `tenant_id`.
--
-- Migration 0004 wrote this:
--
--     FOREIGN KEY (resource_id, tenant_id) REFERENCES resource (id, tenant_id)
--         ON DELETE SET NULL
--
-- intending "when the resource goes, forget which resource it was". What PostgreSQL does
-- is set *every column of the key* to NULL — `tenant_id` as well — and `tenant_id` is
-- NOT NULL. So deleting any resource that had ever been through identity resolution
-- failed with:
--
--     null value in column "tenant_id" of relation "identity_decision"
--     violates not-null constraint
--
-- on a table the operator was not looking at, from a statement about a different one.
-- Nothing caught it because deleting a resource is rare and the decision log is written
-- by a code path most tests do not reach. It was found on 2026-09-17 by a schema
-- invariant added while writing migration 0017, which had the same mistake in three
-- places at once — which is the argument for the guard rather than for being careful.
--
-- The fix is the column list, PostgreSQL 15 and inside PLAN's floor of 16. It says what
-- 0004 meant: null the reference, keep the tenant.
--
-- Rewriting 0004 was the other option and is not available: the runner checksums applied
-- migrations and refuses to run against a database whose history has been edited, which
-- is the property that makes a deployed schema knowable at all.

ALTER TABLE identity_decision
    DROP CONSTRAINT identity_decision_resource_id_tenant_id_fkey;

ALTER TABLE identity_decision
    ADD CONSTRAINT identity_decision_resource_id_tenant_id_fkey
    FOREIGN KEY (resource_id, tenant_id) REFERENCES resource (id, tenant_id)
        ON DELETE SET NULL (resource_id);
