#!/usr/bin/env bash
# Run the workspace suite on Linux, against the compose stack.
#
# This project is developed on Windows and ships on Linux, so every change is run on
# both. The container is the Linux half, and it exists because the differences are real:
# file permissions on the key ring, unprivileged ICMP, and UDP behaviour under load have
# each produced a Linux-only failure that Windows could not have found.
#
#   bash scripts/linux-test.sh                 # the whole suite
#   bash scripts/linux-test.sh -p uops-otlp    # anything after the script is cargo's
#
# # Two things this handles that a hand-typed docker run does not
#
# **The container IPs.** Docker reassigns them on every restart of the stack, so a
# hard-coded address is stale the moment anything restarts — and a stale address looks
# exactly like a test failure. They are read from the daemon here instead.
#
# **The incremental cache.** `target-linux/debug/incremental` reached 5.5 GB in one
# session, and together with its Windows twin it has filled this machine's disk three
# times — twice taking Docker's engine down with it, because its virtual disk could not
# grow. Incremental compilation buys very little here: the container is fresh each run
# and most invocations touch a different crate. So it is off, which is the difference
# between a few hundred megabytes and several gigabytes per run.
set -euo pipefail

cd "$(dirname "$0")/.."

# MSYS on Windows rewrites /w into a drive path unless told not to.
export MSYS_NO_PATHCONV=1

ip_of() {
    docker inspect -f '{{range .NetworkSettings.Networks}}{{.IPAddress}}{{end}}' "$1" 2>/dev/null || true
}

PG=$(ip_of uops-dev-pg)
CH=$(ip_of uops-dev-ch)
SNMP=$(ip_of uops-snmp-agent)

if [ -z "$PG" ] || [ -z "$CH" ]; then
    echo "the compose stack is not running. Start it with:" >&2
    echo "  docker compose -f deploy/docker-compose.yml --profile test up -d postgres clickhouse snmp-agent" >&2
    exit 1
fi

echo "postgres $PG, clickhouse $CH, snmp-agent ${SNMP:-<absent>}"

# The network the stack is on. Named after the compose project directory, which is
# `deploy` here — read rather than assumed, because a rename would otherwise fail with an
# error about a network nobody has heard of.
NETWORK=$(docker inspect -f '{{range $k, $v := .NetworkSettings.Networks}}{{$k}}{{end}}' uops-dev-pg)

exec docker run --rm \
    --network "$NETWORK" \
    -v "$(pwd -W 2>/dev/null || pwd)":/w \
    -v uops-cargo-registry:/usr/local/cargo/registry \
    -w /w \
    -e DATABASE_URL="postgres://uops:uops@${PG}:5432/uops" \
    -e CLICKHOUSE_URL="http://${CH}:8123" \
    -e CLICKHOUSE_DB=uops \
    -e CLICKHOUSE_USER=uops \
    -e CLICKHOUSE_PASSWORD=uops \
    ${SNMP:+-e UOPS_SNMP_AGENT="${SNMP}:161"} \
    -e CARGO_TARGET_DIR=/w/target-linux \
    -e CARGO_INCREMENTAL=0 \
    rust:1-slim \
    cargo test --workspace --all-targets --quiet "$@"
