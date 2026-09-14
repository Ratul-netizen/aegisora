#!/usr/bin/env bash
# Run the server against the dev containers.
#
#   bash scripts/serve.sh          # debug build, http://127.0.0.1:8080
#   bash scripts/serve.sh --release
#
# Assumes scripts/db.sh migrate and scripts/ch.sh apply have been run. The server does
# not migrate — see crates/uops-server/src/main.rs for why — so a fresh clone needs
# those two first, and will be told plainly if it skipped them.
#
# UOPS_INSECURE_COOKIES is set here and only here. Cookies without `Secure` are
# discarded by the browser on plain http, so a developer on localhost would otherwise
# see an app that silently fails to log in. Everything outside this script defaults to
# secure; a deployment that wants this off has to say so itself.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

export DATABASE_URL="${DATABASE_URL:-postgres://uops:uops@localhost:5432/uops}"
export CLICKHOUSE_URL="${CLICKHOUSE_URL:-http://localhost:8123}"
export CLICKHOUSE_USER="${CLICKHOUSE_USER:-uops}"
export CLICKHOUSE_PASSWORD="${CLICKHOUSE_PASSWORD:-uops}"
export UOPS_BIND="${UOPS_BIND:-127.0.0.1:8080}"
export UOPS_INSECURE_COOKIES=1

echo "database:   ${DATABASE_URL%%:*}://$(echo "${DATABASE_URL#*@}")"
echo "clickhouse: $CLICKHOUSE_URL"
echo

# On the very first run this prints an administrator password, once. It is not written
# anywhere else and there is no way to ask for it again.
exec cargo run "$@" --manifest-path "$ROOT/Cargo.toml" -p uops-server
