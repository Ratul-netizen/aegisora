/**
 * The tail's buffer, which is the part that can be quietly wrong.
 *
 * The window arithmetic is the server's and is tested against real `ClickHouse`. What is
 * left in the browser is this: rows arriving in pages, oldest leaving, and a table that
 * must not blank itself between them. Every failure here looks like a working tail —
 * lines appear, the page scrolls — while showing an operator something that is not what
 * arrived.
 */

import { describe, expect, it } from "vitest";

import { BUFFER_ROWS, merge } from "./tail";
import type { TailPage } from "./tail";
import type { ResultSet } from "./query";

const COLUMNS = [
  { name: "observed_at", type: "DateTime64(3)" },
  { name: "body", type: "String" },
];

function page(bodies: string[], extra: Partial<TailPage> = {}): TailPage {
  return {
    columns: bodies.length > 0 ? COLUMNS : [],
    rows: bodies.map((body) => ["2026-09-17 10:00:00", body]),
    table: "logs",
    warnings: [],
    rows_read: bodies.length,
    bytes_read: bodies.length * 100,
    next_since: "2026-09-17T10:00:02.000Z",
    complete: true,
    skipped: false,
    ...extra,
  };
}

function bodies(result: ResultSet): string[] {
  return result.rows.map((row) => String(row[1]));
}

describe("merge", () => {
  it("puts what just arrived at the top", () => {
    const first = merge(null, page(["older", "oldest"]));
    const second = merge(first, page(["newest", "newer"]));

    // The tail delivers newest-first within a poll, and a later poll is newer than an
    // earlier one. Appending instead would put the line that just arrived at the bottom
    // of a thousand rows, which is not a tail.
    expect(bodies(second)).toEqual(["newest", "newer", "older", "oldest"]);
  });

  it("keeps the columns when a poll brings no rows", () => {
    // ClickHouse sends no `meta` for an empty result, and on a quiet estate most polls
    // are empty. Taking the page's columns unconditionally blanks the table every two
    // seconds — the rows are still there, and the header is gone, so nothing renders.
    const running = merge(null, page(["something"]));
    const quiet = merge(running, page([]));

    expect(quiet.columns).toEqual(COLUMNS);
    expect(bodies(quiet)).toEqual(["something"]);
    expect(quiet.table).toBe("logs");
  });

  it("drops the oldest rows once the buffer is full", () => {
    const full = merge(null, page(Array.from({ length: BUFFER_ROWS }, (_, i) => `row ${i}`)));
    const after = merge(full, page(["just arrived"]));

    expect(after.rows).toHaveLength(BUFFER_ROWS);
    expect(bodies(after)[0]).toBe("just arrived");
    // The row that fell off the end is the oldest one, not an arbitrary one.
    expect(bodies(after).at(-1)).toBe(`row ${BUFFER_ROWS - 2}`);
  });

  it("accumulates what the tail has cost", () => {
    // The number that explains a cluster's load at the end of an afternoon. The last
    // poll's few hundred rows explain nothing, and would tick back to nearly zero every
    // two seconds while the real cost kept climbing.
    const first = merge(null, page(["a"]));
    const second = merge(first, page(["b"]));

    expect(second.rows_read).toBe(2);
    expect(second.bytes_read).toBe(200);
  });

  it("keeps only the newest poll's warnings", () => {
    // A warning is a statement about a query, and every poll runs the same one. Keeping
    // them all prints the same sentence a thousand times.
    const slow = { warning: "not_index_accelerated", message: "substring search cannot use the index" };
    const first = merge(null, page(["a"], { warnings: [slow] }));
    const second = merge(first, page(["b"], { warnings: [slow] }));

    expect(second.warnings).toHaveLength(1);
  });
});
