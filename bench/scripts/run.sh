#!/usr/bin/env bash
# W1 query benchmark runner.
#
# Executes each query in queries/queries.tsv N times and reports p50/p95/p99 from
# ClickHouse's own system.query_log (server-side duration), plus rows and bytes
# actually read — which is how you tell a skip index worked from a full scan that
# happened to be fast.
#
# Cache policy: each query runs once as a warmup (discarded), then N timed runs.
# The reported numbers are therefore WARM. That is the right default: a monitoring
# platform's query path is continuously exercised, not cold. Pass --cold to drop
# caches before every run instead.
#
# Usage: scripts/run.sh [iterations] [--cold]

set -euo pipefail

ITER="${1:-10}"
COLD=""
[ "${2:-}" = "--cold" ] && COLD=1

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
CH="http://localhost:8123/?user=bench&password=bench"
RUN_TAG="w1_$(date +%s)"

q()  { curl -sS --data-binary @- "$CH"; }
qid(){ curl -sS --data-binary @- "${CH}&query_id=$1"; }

echo "=== discovering data parameters ==="
TENANT=$(echo "SELECT tenant_id FROM bench.logs LIMIT 1" | q | tr -d '\r\n')
# Pick a resource with a representative (median-ish) row count, not the busiest —
# benchmarking the top talker would flatter Q05.
RESOURCE=$(echo "SELECT resource_id FROM bench.logs WHERE tenant_id='$TENANT' GROUP BY resource_id ORDER BY count() DESC LIMIT 1 OFFSET 2500" | q | tr -d '\r\n')
D0=$(echo "SELECT toString(min(observed_at)) FROM bench.logs" | q | tr -d '\r\n')
D1=$(echo "SELECT toString(max(observed_at)) FROM bench.logs" | q | tr -d '\r\n')
# a one-hour window in the middle of the range
T0=$(echo "SELECT toString(min(observed_at) + INTERVAL 3 DAY) FROM bench.logs" | q | tr -d '\r\n')
T1=$(echo "SELECT toString(min(observed_at) + INTERVAL 3 DAY + INTERVAL 1 HOUR) FROM bench.logs" | q | tr -d '\r\n')
ROWS=$(echo "SELECT count() FROM bench.logs" | q | tr -d '\r\n')

echo "rows      : $ROWS"
echo "tenant    : $TENANT"
echo "resource  : $RESOURCE"
echo "range     : $D0 .. $D1"
echo "window    : $T0 .. $T1"
echo "iterations: $ITER  (warm$([ -n "$COLD" ] && echo ", COLD mode"))"
echo

RESULTS="$HERE/results"
mkdir -p "$RESULTS"
OUT="$RESULTS/w1-logs-${ROWS}rows.md"

{
  echo "# W1 results — bench.logs"
  echo
  echo "- rows: \`$ROWS\`"
  echo "- range: \`$D0\` .. \`$D1\`"
  echo "- window (Q03–Q06): \`$T0\` .. \`$T1\`"
  echo "- iterations: $ITER (warm)"
  echo "- ClickHouse: \`$(echo 'SELECT version()' | q | tr -d '\r\n')\`"
  echo "- host: $(nproc 2>/dev/null || echo '?') cores"
  echo
  echo "| id | query | p50 ms | p95 ms | p99 ms | rows read | bytes read | result |"
  echo "|----|-------|-------:|-------:|-------:|----------:|-----------:|--------|"
} > "$OUT"

while IFS=$'\t' read -r ID LABEL SQL; do
  case "$ID" in ''|\#*) continue ;; esac

  SQL="${SQL//\{TENANT\}/$TENANT}"
  SQL="${SQL//\{RESOURCE\}/$RESOURCE}"
  SQL="${SQL//\{T0\}/$T0}"
  SQL="${SQL//\{T1\}/$T1}"
  SQL="${SQL//\{D0\}/$D0}"

  printf '%-6s %-40s ' "$ID" "$LABEL"

  # warmup (discarded)
  echo "$SQL" | q > /dev/null 2>&1 || { echo "FAILED"; continue; }

  for i in $(seq 1 "$ITER"); do
    if [ -n "$COLD" ]; then
      echo "SYSTEM DROP MARK CACHE" | q > /dev/null
      echo "SYSTEM DROP UNCOMPRESSED CACHE" | q > /dev/null
    fi
    echo "$SQL" | qid "${RUN_TAG}_${ID}_${i}" > /dev/null 2>&1 || true
  done

  echo "SYSTEM FLUSH LOGS" | q > /dev/null

  STATS=$(echo "
    SELECT
      round(quantileExact(0.50)(query_duration_ms)) AS p50,
      round(quantileExact(0.95)(query_duration_ms)) AS p95,
      round(quantileExact(0.99)(query_duration_ms)) AS p99,
      formatReadableQuantity(avg(read_rows))        AS rr,
      formatReadableSize(avg(read_bytes))           AS rb
    FROM system.query_log
    WHERE query_id LIKE '${RUN_TAG}_${ID}_%' AND type = 'QueryFinish'
    FORMAT TSV" | q | tr -d '\r')

  P50=$(echo "$STATS" | cut -f1); P95=$(echo "$STATS" | cut -f2); P99=$(echo "$STATS" | cut -f3)
  RR=$(echo "$STATS" | cut -f4);  RB=$(echo "$STATS" | cut -f5)
  RES=$(echo "$SQL" | q | head -1 | cut -c1-40 | tr -d '\r\n')

  printf 'p50=%-7s p95=%-7s read=%s\n' "${P50}ms" "${P95}ms" "$RR"
  echo "| $ID | $LABEL | $P50 | $P95 | $P99 | $RR | $RB | \`$RES\` |" >> "$OUT"
done < "$HERE/queries/queries.tsv"

{
  echo
  echo "## Storage"
  echo
  echo '```'
  echo "SELECT table, formatReadableSize(sum(data_compressed_bytes)) compressed,
        formatReadableSize(sum(data_uncompressed_bytes)) uncompressed,
        round(sum(data_uncompressed_bytes)/sum(data_compressed_bytes),2) ratio,
        formatReadableSize(sum(secondary_indices_compressed_bytes)) indexes,
        sum(rows) rows
        FROM system.parts WHERE database='bench' AND active GROUP BY table ORDER BY table FORMAT PrettyCompactMonoBlock" | q
  echo '```'
} >> "$OUT"

echo
echo "=== written: $OUT ==="
