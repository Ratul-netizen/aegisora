#!/usr/bin/env bash
# PostgreSQL control-plane schema: apply migrations, assert the invariants.
#
#   bash scripts/db.sh up        # start the dev database
#   bash scripts/db.sh migrate   # apply migrations/ in order
#   bash scripts/db.sh test      # assert the schema invariants
#   bash scripts/db.sh reset     # drop everything and re-apply
#   bash scripts/db.sh sweep     # drop scratch databases and test tenants left behind
#   bash scripts/db.sh psql      # interactive shell
#   bash scripts/db.sh down      # stop (keeps the volume)
#
# Migrations are applied with sqlx-cli when it is installed, which is what CI and
# deployments use, and with psql otherwise so that a clone with no cargo tooling can
# still bring a database up. The two paths differ in one way that matters: only sqlx
# records what it applied in _sqlx_migrations, so the psql path is for throwaway
# databases, and it says so before it runs.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
COMPOSE="$ROOT/deploy/docker-compose.yml"
: "${DATABASE_URL:=postgres://uops:uops@localhost:5432/uops}"
export DATABASE_URL

have() { command -v "$1" >/dev/null 2>&1; }

# psql through the container, so a local client install is not required.
psql_run() {
  docker compose -f "$COMPOSE" exec -T -e PGPASSWORD=uops postgres \
    psql -U uops -d uops -v ON_ERROR_STOP=1 "$@"
}

wait_ready() {
  echo "waiting for postgres..."
  for _ in $(seq 1 40); do
    if docker compose -f "$COMPOSE" exec -T postgres pg_isready -U uops -d uops \
         >/dev/null 2>&1; then
      echo "ready"
      return 0
    fi
    sleep 1
  done
  echo "postgres did not become ready" >&2
  exit 1
}

cmd_up() {
  docker compose -f "$COMPOSE" up -d postgres clickhouse
  wait_ready
}

cmd_migrate() {
  if have sqlx; then
    echo "applying migrations with sqlx-cli"
    (cd "$ROOT" && sqlx migrate run)
    return
  fi

  echo "sqlx-cli not found; applying with psql."
  echo "NOTE: nothing is recorded in _sqlx_migrations, so this database cannot be"
  echo "      migrated incrementally afterwards. Use 'reset' to re-apply."
  for f in "$ROOT"/migrations/[0-9]*.sql; do
    echo "  -> $(basename "$f")"
    psql_run -f - < "$f"
  done
}

cmd_test() {
  psql_run -f - < "$ROOT/migrations/tests/invariants.sql"
}

# Scratch databases from the bootstrap tests, which need a genuinely empty
# installation and so cannot share this one. A passing test drops its own; a panicking
# test cannot, because Drop is not async. They are empty and harmless, and they
# accumulate, so reset sweeps them.
cmd_sweep() {
  local stray
  stray=$(psql_run -tAc "SELECT datname FROM pg_database
                          WHERE datname LIKE 'uops_boot_%'
                             OR datname LIKE 'uops_poll_%'")
  for db in $stray; do
    psql_run -c "DROP DATABASE IF EXISTS \"$db\" WITH (FORCE)" > /dev/null
    echo "dropped scratch database $db"
  done

  # Every tenant but the one first-run creates.
  #
  # Nearly every integration test in this workspace creates a tenant, and a test that
  # panics before its clean-up leaves it behind. They accumulate: this was found at 2 608
  # of them, which is not a tidiness problem — the poller's reload reads *every* tenant
  # and issues two queries for each, so a development database nobody swept turns a
  # reload into five thousand round trips. It is also the same shape as the scale test's
  # ten thousand orphaned resources, which changed what the planner chose for every other
  # suite and took a day to find.
  #
  # This is a scratch database — `reset` drops the whole schema — so "every tenant but
  # `default`" is the right rule here and would be the wrong one anywhere else.
  local removed
  removed=$(psql_run -tAc "
    WITH doomed AS (SELECT id FROM tenant WHERE slug <> 'default'),
         a AS (DELETE FROM access_log            WHERE tenant_id IN (SELECT id FROM doomed)),
         b AS (DELETE FROM audit_log             WHERE tenant_id IN (SELECT id FROM doomed)),
         c AS (DELETE FROM credential_access_log WHERE tenant_id IN (SELECT id FROM doomed)),
         d AS (DELETE FROM identity_decision     WHERE tenant_id IN (SELECT id FROM doomed)),
         e AS (DELETE FROM resource_relationship WHERE tenant_id IN (SELECT id FROM doomed)),
         f AS (DELETE FROM resource_alias        WHERE tenant_id IN (SELECT id FROM doomed)),
         g AS (DELETE FROM resource_identifier   WHERE tenant_id IN (SELECT id FROM doomed)),
         h AS (DELETE FROM resource              WHERE tenant_id IN (SELECT id FROM doomed)),
         i AS (DELETE FROM credential            WHERE tenant_id IN (SELECT id FROM doomed)),
         j AS (DELETE FROM monitoring_profile    WHERE tenant_id IN (SELECT id FROM doomed)),
         k AS (DELETE FROM user_tenant_role      WHERE tenant_id IN (SELECT id FROM doomed)),
         l AS (DELETE FROM site                  WHERE tenant_id IN (SELECT id FROM doomed)),
         m AS (DELETE FROM tenant WHERE id IN (SELECT id FROM doomed) RETURNING 1)
    SELECT count(*) FROM m")
  if [ "${removed:-0}" -gt 0 ]; then
    echo "removed $removed test tenant(s) and everything under them"
  fi

  # Organizations are not tenant-scoped, so they are orphaned rather than cascaded — and
  # `app_user` hangs off the organization rather than the tenant, so the users go with
  # them. Sessions reference the user and are deleted first for the same reason.
  local orgs
  orgs=$(psql_run -tAc "
    WITH doomed AS (
      SELECT id FROM organization o
       WHERE NOT EXISTS (SELECT 1 FROM tenant t WHERE t.org_id = o.id)
    ),
    users AS (SELECT id FROM app_user WHERE org_id IN (SELECT id FROM doomed)),
    a AS (DELETE FROM session  WHERE user_id IN (SELECT id FROM users)),
    b AS (DELETE FROM app_user WHERE id      IN (SELECT id FROM users)),
    c AS (DELETE FROM organization WHERE id  IN (SELECT id FROM doomed) RETURNING 1)
    SELECT count(*) FROM c")
  if [ "${orgs:-0}" -gt 0 ]; then
    echo "removed $orgs organization(s) with no tenants left"
  fi
}

cmd_reset() {
  psql_run -c 'DROP SCHEMA public CASCADE; CREATE SCHEMA public;'
  cmd_sweep
  cmd_migrate
}

case "${1:-}" in
  up)      cmd_up ;;
  migrate) cmd_migrate ;;
  test)    cmd_test ;;
  reset)   cmd_reset ;;
  sweep)   cmd_sweep ;;
  psql)    docker compose -f "$COMPOSE" exec -e PGPASSWORD=uops postgres \
             psql -U uops -d uops ;;
  down)    docker compose -f "$COMPOSE" down ;;
  *)       sed -n '2,20p' "${BASH_SOURCE[0]}"; exit 1 ;;
esac
