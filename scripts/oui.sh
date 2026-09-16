#!/usr/bin/env bash
#
# Regenerate the bundled IEEE MAC-assignment table.
#
#   bash scripts/oui.sh
#
# Downloads the four public IEEE registries and writes a compact table to
# crates/uops-oui/data/assignments.tsv. Commit the result; the crate embeds it, so
# nothing downloads anything at build time or at run time.
#
# # Why all four
#
# A 24-bit prefix is the OUI everybody knows, and it has not been the whole story since
# IEEE began issuing smaller blocks. An MA-M assignment is 28 bits and an MA-S is 36, and
# several organisations share the 24-bit prefix above them — so a lookup that only ever
# took three bytes reports whichever of them happens to hold the parent block. That is
# not a missing answer, it is a wrong one with a plausible company name attached.
#
# # Why a derived file rather than the CSVs
#
# The registries carry a postal address per entry, which is most of their 5.2 MB and is
# of no use here. What is left is the assignment and the organisation, and the assignment's
# own length says how many bits it covers: six hex characters is 24 bits, seven is 28,
# nine is 36.
#
# # Provenance
#
# Public registries published by IEEE at standards-oui.ieee.org. See the README beside
# the generated file.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OUT="$ROOT/crates/uops-oui/data/assignments.tsv"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

have() { command -v "$1" >/dev/null 2>&1; }
have curl || { echo "curl is required" >&2; exit 1; }
have python3 || have python || { echo "python is required" >&2; exit 1; }
PY=$(command -v python3 || command -v python)

echo "downloading the IEEE registries..."
curl -fsS --max-time 180 -o "$TMP/mal.csv" https://standards-oui.ieee.org/oui/oui.csv
curl -fsS --max-time 180 -o "$TMP/mam.csv" https://standards-oui.ieee.org/oui28/mam.csv
curl -fsS --max-time 180 -o "$TMP/mas.csv" https://standards-oui.ieee.org/oui36/oui36.csv
curl -fsS --max-time 180 -o "$TMP/cid.csv" https://standards-oui.ieee.org/cid/cid.csv

mkdir -p "$(dirname "$OUT")"
"$PY" "$ROOT/scripts/oui.py" "$TMP" "$OUT"

echo "wrote $OUT: $(wc -l < "$OUT") assignments, $(wc -c < "$OUT") bytes"
echo "commit it, and update the date in $(dirname "$OUT")/README.md"
