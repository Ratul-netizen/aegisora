#!/usr/bin/env python3
"""Turn the four IEEE registry CSVs into one compact table.

Called by scripts/oui.sh, which is where the reasoning lives.

Each output line is `<assignment hex>\t<registry>\t<organisation>`. The assignment's
length says how wide it is: 6 hex characters is a 24-bit MA-L, 7 is a 28-bit MA-M, 9 is
a 36-bit MA-S.

The registry is one letter. `C` is a CID, which is explicitly *not* a guarantee of
uniqueness — it exists for protocols that need a company identifier and not a unique
address, so two devices may legitimately carry the same one. Keeping the letter lets a
caller decide what to do about that instead of being told a lie by omission.
"""

import csv
import io
import os
import sys

FILES = {
    "mal.csv": "L",
    "mam.csv": "M",
    "mas.csv": "S",
    "cid.csv": "C",
}

HEX = set("0123456789ABCDEF")


def clean(name: str) -> str:
    """Collapse whitespace and drop the placeholders these files carry."""
    name = " ".join(name.split())
    # A few thousand entries are literally "Private". A vendor column reading Private is
    # worse than an empty one: it looks like a company of that name.
    if name.lower() in {"private", "ieee registration authority"}:
        return ""
    return name


def main() -> int:
    src, out = sys.argv[1], sys.argv[2]
    rows: dict[str, tuple[str, str]] = {}

    for filename, registry in FILES.items():
        path = os.path.join(src, filename)
        if not os.path.exists(path):
            print("missing " + path, file=sys.stderr)
            return 1
        with io.open(path, encoding="utf-8", newline="") as handle:
            for row in csv.DictReader(handle):
                assignment = (row.get("Assignment") or "").strip().upper()
                org = clean(row.get("Organization Name") or "")
                if not org or not assignment:
                    continue
                if len(assignment) not in (6, 7, 9):
                    continue
                if not set(assignment) <= HEX:
                    continue
                rows[assignment] = (registry, org)

    # Sorted, so a diff between two regenerations reads as the handful of assignments
    # that were added rather than as a reshuffle of fifty thousand lines.
    with io.open(out, "w", encoding="utf-8", newline="\n") as handle:
        for assignment in sorted(rows):
            registry, org = rows[assignment]
            handle.write(assignment + "\t" + registry + "\t" + org + "\n")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
