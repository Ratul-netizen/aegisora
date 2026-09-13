#!/usr/bin/env bash
# W1 benchmark loader.
#
# Loads generated telemetry into ClickHouse over the HTTP interface, in bounded
# batches.
#
# TWO HARNESS BUGS THIS SCRIPT EXISTS TO AVOID — both measured, not theoretical:
#
# 1. `docker exec -i` stdin is unusable for bulk load on Windows.
#    1M log rows: 182s through `docker exec` stdin vs 9.9s over HTTP to the
#    published port. Docker Desktop proxies container stdin through a named pipe
#    which collapses under a high-rate stream. This is a HARNESS artifact, not a
#    ClickHouse ingest limit — reporting it as an ingest number would be wrong.
#
# 2. `curl --data-binary @-` buffers the ENTIRE stream in memory before sending.
#    Observed curl.exe at 6.7 GB RSS and still climbing on a 100M-row load, with
#    zero bytes delivered server-side. A 100M-row load is ~32 GB of TSV, so this
#    exhausts RAM before ClickHouse sees a single row.
#    Fix: fixed-size batches, one HTTP request each, bounded by BATCH.
#
# The generator's --skip/--total flags keep timestamps continuous and needle
# frequencies exact across batch boundaries, so a batched load is byte-for-byte
# equivalent in distribution to a single stream.
#
# Usage:
#   scripts/load.sh logs    100000000 [--truncate]
#   scripts/load.sh metrics 100000000 [--truncate]
#
# Env: BATCH (default 2000000) RESOURCES TENANTS DAYS SEED

set -euo pipefail

SIGNAL="${1:?usage: load.sh <logs|metrics> <rows> [--truncate]}"
ROWS="${2:?usage: load.sh <logs|metrics> <rows> [--truncate]}"
TRUNCATE="${3:-}"

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
GEN="$HERE/gen/target/release/uops-bench-gen.exe"
[ -x "$GEN" ] || GEN="$HERE/gen/target/release/uops-bench-gen"
CH="http://localhost:8123/?user=bench&password=bench"

BATCH="${BATCH:-2000000}"      # ~640 MB of TSV per request at current row width
RESOURCES="${RESOURCES:-5000}"
TENANTS="${TENANTS:-3}"
DAYS="${DAYS:-7}"
SEED="${SEED:-42}"

case "$SIGNAL" in
  logs)
    TABLE="bench.logs"
    COLS="tenant_id,resource_id,site_id,observed_at,ingested_at,source_kind,source_vendor,severity,facility,body,attributes,trace_id,span_id"
    ;;
  metrics)
    TABLE="bench.metrics"
    COLS="tenant_id,resource_id,site_id,metric,observed_at,ingested_at,value,unit,labels"
    ;;
  *) echo "unknown signal: $SIGNAL" >&2; exit 2 ;;
esac

q() { curl -sS --data-binary @- "$CH"; }

if [ "$TRUNCATE" = "--truncate" ]; then
  echo "truncating $TABLE"
  echo "TRUNCATE TABLE $TABLE" | q
fi

COLS_ENC="${COLS//,/%2C}"
INSERT_URL="${CH}&query=INSERT%20INTO%20${TABLE}%20(${COLS_ENC})%20FORMAT%20TabSeparated&max_insert_block_size=1000000&input_format_parallel_parsing=1"

echo "loading $ROWS rows into $TABLE"
echo "  batch=$BATCH resources=$RESOURCES tenants=$TENANTS days=$DAYS seed=$SEED"
START=$(date +%s)
DONE=0

while [ "$DONE" -lt "$ROWS" ]; do
  REMAIN=$((ROWS - DONE))
  N=$(( REMAIN < BATCH ? REMAIN : BATCH ))

  # -T - streams with chunked transfer-encoding instead of buffering stdin.
  # Kept as an explicit POST because ClickHouse's HTTP insert expects POST.
  "$GEN" "$SIGNAL" --rows "$N" --skip "$DONE" --total "$ROWS" \
         --resources "$RESOURCES" --tenants "$TENANTS" --days "$DAYS" --seed "$SEED" \
    | curl -sS -X POST -H 'Transfer-Encoding: chunked' --data-binary @- "$INSERT_URL"

  DONE=$((DONE + N))
  NOW=$(date +%s); EL=$((NOW - START)); [ "$EL" -eq 0 ] && EL=1
  printf '\r  %s / %s rows  (%s rows/s)   ' "$DONE" "$ROWS" "$((DONE / EL))"
done

END=$(date +%s); ELAPSED=$((END - START)); [ "$ELAPSED" -eq 0 ] && ELAPSED=1
echo
echo "loaded in ${ELAPSED}s  ($((ROWS / ELAPSED)) rows/s)"
echo "SELECT count() FROM $TABLE" | q
