#!/usr/bin/env bash
# ClickHouse schema: apply migrations, then prove the schema does what the rest of the
# codebase assumes it does.
#
#   bash scripts/ch.sh apply     # cargo run -p uops-ch-migrate -- apply
#   bash scripts/ch.sh status
#   bash scripts/ch.sh verify    # run uops-query's golden SQL against the live schema
#   bash scripts/ch.sh smoke     # insert rows, assert the views and indexes behave
#   bash scripts/ch.sh reset     # drop the database and re-apply from empty
#
# `verify` is the one that earns its keep. uops-query's golden files record the exact
# SQL the compiler emits; ch-migrations records the exact schema the product creates.
# Nothing else checks that those two agree, and the way they drift is silent — a
# renamed column or a function that does not exist passes every unit test on both
# sides. It has already caught one: the compiler emitted searchAll()/searchAny(), which
# are not ClickHouse functions.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
: "${CLICKHOUSE_URL:=http://localhost:8123}"
: "${CLICKHOUSE_DB:=uops}"
: "${CLICKHOUSE_USER:=uops}"
: "${CLICKHOUSE_PASSWORD:=uops}"
export CLICKHOUSE_URL CLICKHOUSE_DB CLICKHOUSE_USER CLICKHOUSE_PASSWORD

BASE="${CLICKHOUSE_URL}/?user=${CLICKHOUSE_USER}&password=${CLICKHOUSE_PASSWORD}&database=${CLICKHOUSE_DB}"

ch() { curl -sS -f "${BASE}$*" --data-binary @-; }

# Same server, no database pinned. Required for DROP/CREATE DATABASE: a request that
# names the database it is about to drop fails with a 404 on the NEXT statement, which
# leaves the server with no uops database at all and every later command failing.
ch_nodb() {
  curl -sS -f "${CLICKHOUSE_URL}/?user=${CLICKHOUSE_USER}&password=${CLICKHOUSE_PASSWORD}"     --data-binary @-
}

FAILURES=0

# Assert a single-value query returns exactly what is expected.
expect() {
  local what="$1" sql="$2" want="$3" got
  got="$(printf '%s' "$sql" | ch | tr -d '\r\n')"
  if [ "$got" = "$want" ]; then
    echo "  ok   $what"
  else
    echo "  FAIL $what: expected '$want', got '$got'"
    FAILURES=$((FAILURES + 1))
  fi
}

cmd_verify() {
  local dir="$ROOT/crates/uops-query/tests/golden"
  local checked=0

  for f in "$dir"/*.sql; do
    local name query args
    name="$(basename "$f")"
    # Everything before the "-- params" block is the statement.
    query="$(sed '/^-- params$/,$d' "$f")"
    # "--   p3 String = bgpd" -> "&param_p3=bgpd"
    args=""
    while read -r n v; do
      [ -z "$n" ] && continue
      args="${args}&param_${n}=$(printf '%s' "$v" | sed 's/ /%20/g; s/:/%3A/g')"
    done < <(sed -n 's/^--   \([a-z0-9_]*\) [A-Za-z0-9()]* = \(.*\)$/\1 \2/p' "$f")

    checked=$((checked + 1))
    if printf '%s' "$query" | ch "$args" > /dev/null 2>/tmp/ch_err; then
      echo "  ok   $name"
    else
      echo "  FAIL $name"
      sed -n '1,3p' /tmp/ch_err | sed 's/^/       /'
      FAILURES=$((FAILURES + 1))
    fi
  done

  echo "$checked golden statement(s) checked, $FAILURES failure(s)"
  [ "$FAILURES" -eq 0 ]
}

cmd_smoke() {
  local T="018f0000-0000-7000-8000-000000000001"
  local R="018f0000-0000-7000-8000-0000000000aa"
  local S="018f0000-0000-7000-8000-000000000055"

  echo "TRUNCATE TABLE logs"        | ch > /dev/null
  echo "TRUNCATE TABLE logs_counts_5m" | ch > /dev/null
  echo "TRUNCATE TABLE metrics"     | ch > /dev/null
  echo "TRUNCATE TABLE metrics_5m"  | ch > /dev/null
  echo "TRUNCATE TABLE metrics_1h"  | ch > /dev/null

  printf '%s' "{\"tenant_id\":\"$T\",\"resource_id\":\"$R\",\"site_id\":\"$S\",\
\"observed_at\":\"2026-09-01 00:03:00.000\",\"ingested_at\":\"2026-09-01 00:03:01.000\",\
\"source_kind\":\"syslog\",\"source_vendor\":\"cisco\",\"severity\":\"error\",\"facility\":23,\
\"body\":\"%LINK-3-UPDOWN: Interface Gi0/1, changed state to down\",\
\"attributes\":{\"host.name\":\"rtr-01\",\"service.name\":\"bgpd\"},\
\"trace_id\":\"\",\"span_id\":\"\"}" \
    | ch "&query=INSERT%20INTO%20logs%20FORMAT%20JSONEachRow"

  # 24 points, values 1..24, two minutes apart: unequal counts per five-minute bucket,
  # which is what makes the rollup assertion below meaningful.
  {
    for i in $(seq 0 23); do
      printf '{"tenant_id":"%s","resource_id":"%s","site_id":"%s","metric":"system.cpu.utilization","observed_at":"2026-09-01 00:%02d:00.000","ingested_at":"2026-09-01 00:%02d:01.000","value":%d,"unit":"1","labels":{"core":"0"}}\n' \
        "$T" "$R" "$S" "$((i * 2))" "$((i * 2))" "$((i + 1))"
    done
  } | ch "&query=INSERT%20INTO%20metrics%20FORMAT%20JSONEachRow"

  echo "smoke:"

  # W1's expensive finding: these are real columns, not map lookups. uops-query rewrites
  # Attr fields onto them, so if the MATERIALIZED clause were dropped the compiler would
  # emit SQL naming a column that does not exist.
  expect "materialised semconv columns are populated" \
    "SELECT host_name || '/' || service_name FROM logs FORMAT TSV" "rtr-01/bgpd"

  # The text index, and the function names the compiler emits.
  expect "token search matches" \
    "SELECT count() FROM logs WHERE hasAllTokens(body, ['changed','down'])" "1"
  expect "token search rejects a word that is not there" \
    "SELECT count() FROM logs WHERE hasAllTokens(body, ['changed','zzqx'])" "0"

  # W1 FIX 2: the Explorer histogram is served from here, so the view has to fire.
  expect "logs_counts_5m is populated by its materialised view" \
    "SELECT countMerge(cnt) FROM logs_counts_5m WHERE bucket = '2026-09-01 00:00:00'" "1"

  expect "metrics_5m is populated" \
    "SELECT countMerge(cnt) FROM metrics_5m" "24"

  # The one worth spelling out. metrics_1h is chained off metrics_5m with -MergeState,
  # so an hourly average is built from twelve five-minute states rather than from raw
  # points. Because the buckets hold unequal counts (3,2,3,2,…), a naive average of
  # bucket averages gives 13.15 — this asserts the weighted answer, 12.5, which is the
  # true mean of 1..24.
  expect "metrics_1h re-aggregates states, not averages of averages" \
    "SELECT round(avgMerge(avg_v), 4) FROM metrics_1h" "12.5"
  expect "metrics_1h preserves the extremes" \
    "SELECT concat(toString(minMerge(min_v)), '/', toString(maxMerge(max_v))) FROM metrics_1h FORMAT TSV" \
    "1/24"
  expect "metrics_1h keeps the raw point count" \
    "SELECT countMerge(cnt) FROM metrics_1h" "24"

  echo "$FAILURES failure(s)"
  [ "$FAILURES" -eq 0 ]
}

case "${1:-}" in
  apply)  (cd "$ROOT" && cargo run -q -p uops-ch-migrate -- apply) ;;
  status) (cd "$ROOT" && cargo run -q -p uops-ch-migrate -- status) ;;
  verify) cmd_verify ;;
  smoke)  cmd_smoke ;;
  reset)
    echo "DROP DATABASE IF EXISTS ${CLICKHOUSE_DB}" | ch_nodb > /dev/null
    echo "CREATE DATABASE IF NOT EXISTS ${CLICKHOUSE_DB}" | ch_nodb > /dev/null
    (cd "$ROOT" && cargo run -q -p uops-ch-migrate -- apply)
    ;;
  *) sed -n '2,16p' "${BASH_SOURCE[0]}"; exit 1 ;;
esac
