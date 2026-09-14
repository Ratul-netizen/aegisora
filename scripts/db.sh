#!/usr/bin/env bash
# PostgreSQL control-plane schema: apply migrations, assert the invariants.
#
#   bash scripts/db.sh up        # start the dev database
#   bash scripts/db.sh migrate   # apply migrations/ in order
#   bash scripts/db.sh test      # assert the schema invariants
#   bash scripts/db.sh reset     # drop everything and re-apply
#   bash scripts/db.sh sweep     # drop scratch databases left by bootstrap tests
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
  docker compose -f "$COMPOSE" up -d
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
  stray=$(psql_run -tAc "SELECT datname FROM pg_database WHERE datname LIKE 'uops_boot_%'")
  for db in $stray; do
    psql_run -c "DROP DATABASE IF EXISTS \"$db\" WITH (FORCE)" > /dev/null
    echo "dropped scratch database $db"
  done
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
