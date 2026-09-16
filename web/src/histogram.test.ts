/**
 * The histogram's arithmetic, which is the part that can be quietly wrong.
 *
 * A bar is a claim about a span of time. If the bucketing is off, the chart still draws —
 * bars of plausible heights in plausible places — and the drag that zooms into them asks
 * for the wrong window. Nothing errors, and an operator concludes the incident happened
 * at a time it did not.
 */

import { describe, expect, it } from "vitest";

import { describeSeconds, toBuckets } from "./histogram";
import {
  MAX_BUCKET_SECONDS,
  bucketSeconds,
  toFieldCounts,
  toHistogram,
  type Query,
  type ResultSet,
} from "./query";

function result(rows: [string, number][]): ResultSet {
  return {
    columns: [
      { name: "bucket", type: "DateTime" },
      { name: "n", type: "UInt64" },
    ],
    rows: rows.map(([at, n]) => [at, n]),
    table: "logs",
    warnings: [],
    rows_read: 0,
    bytes_read: 0,
  };
}

const QUERY: Query = {
  signal: "log",
  time: { start: "2026-09-17T10:00:00.000Z", end: "2026-09-17T11:00:00.000Z" },
  resources: { type: "all" },
  filter: { op: "text", field: { field: "body" }, mode: "any_token", terms: ["timeout"] },
  limit: 100,
};

describe("bucketSeconds", () => {
  it("snaps to a width a person can say out loud", () => {
    // "Each bar is five minutes" is a sentence. 37 seconds is an artefact of whatever
    // window somebody happened to drag.
    const hour = 3_600_000;
    expect(bucketSeconds(0, hour)).toBe(60);
    expect(bucketSeconds(0, 24 * hour)).toBe(1800);
    expect(bucketSeconds(0, 15 * 60_000)).toBe(15);
  });

  it("never exceeds the bucket the server will accept", () => {
    // `uops_query`'s compiler rejects a bucket outside 1 second to 1 day outright, so a
    // window long enough to want two-day bars would have produced a 400 rather than a
    // chart. Found by reading the compiler after the TypeScript was already green —
    // typechecking a request says nothing about whether the server takes it.
    const day = 86_400_000;
    for (const days of [1, 7, 30, 90, 365, 3650]) {
      const seconds = bucketSeconds(0, days * day);
      expect(seconds).toBeGreaterThanOrEqual(1);
      expect(seconds).toBeLessThanOrEqual(MAX_BUCKET_SECONDS);
    }
  });

  it("never returns zero, however short the window", () => {
    // A zero would divide by zero in `toBuckets` and draw nothing; a sub-second bucket
    // would ask for more bars than there are pixels.
    expect(bucketSeconds(0, 0)).toBeGreaterThan(0);
    expect(bucketSeconds(0, 100)).toBeGreaterThan(0);
  });
});

describe("toBuckets", () => {
  it("fills the buckets ClickHouse did not return", () => {
    // ClickHouse sends a row only for a bucket that has data, so a gap in the result is
    // a gap in *time*. A chart that packed the returned buckets side by side would draw
    // a quiet hour and a busy hour at the same width.
    const from = new Date("2026-09-17T10:00:00Z");
    const to = new Date("2026-09-17T10:05:00Z");
    const buckets = toBuckets(
      result([
        ["2026-09-17 10:00:00", 3],
        ["2026-09-17 10:04:00", 7],
      ]),
      from,
      to,
      60,
    );

    expect(buckets).toHaveLength(6); // 10:00 through 10:05 inclusive
    expect(buckets.map((b) => b.count)).toEqual([3, 0, 0, 0, 7, 0]);
    expect(buckets[0]?.at.toISOString()).toBe("2026-09-17T10:00:00.000Z");
  });

  it("puts a bucket where the server put it, not a millisecond away", () => {
    // The server floors to the interval and so does this. If they disagreed, one bucket
    // would become two half-height bars and the total would still look right.
    const buckets = toBuckets(
      result([["2026-09-17 10:02:00", 5]]),
      new Date("2026-09-17T10:00:00.123Z"),
      new Date("2026-09-17T10:04:00Z"),
      60,
    );
    const at = buckets.find((b) => b.count === 5);
    expect(at?.at.toISOString()).toBe("2026-09-17T10:02:00.000Z");
    expect(buckets.filter((b) => b.count > 0)).toHaveLength(1);
  });

  it("returns nothing rather than looping when the width is impossible", () => {
    expect(toBuckets(result([]), new Date(0), new Date(1000), 0)).toEqual([]);
  });

  it("is bounded, so a mis-derived width cannot hang the page", () => {
    // A one-second bucket across a year is 31 million bars. The guard is not a limit
    // anybody should hit; it is the difference between a wrong chart and a dead tab.
    const buckets = toBuckets(
      result([]),
      new Date("2026-01-01T00:00:00Z"),
      new Date("2027-01-01T00:00:00Z"),
      1,
    );
    expect(buckets.length).toBeLessThanOrEqual(5000);
  });
});

describe("toHistogram", () => {
  it("keeps the filter the rows were found with", () => {
    // A histogram that filtered differently from the rows beneath it would be a chart of
    // something else, and nobody would notice until they counted the bars.
    const histogram = toHistogram(QUERY, 60);
    expect(histogram.filter).toEqual(QUERY.filter);
    expect(histogram.signal).toBe(QUERY.signal);
    expect(histogram.time).toEqual(QUERY.time);
    expect(histogram.resources).toEqual(QUERY.resources);
  });

  it("groups and orders by the bucket, so the bars come back in time order", () => {
    const histogram = toHistogram(QUERY, 300);
    expect(histogram.group_by).toEqual([{ field: "time_bucket", seconds: 300 }]);
    expect(histogram.aggregations).toEqual([{ func: "count", alias: "n" }]);
    expect(histogram.order_by).toEqual([
      { key: { by: "field", field: { field: "time_bucket", seconds: 300 } } },
    ]);
  });

  it("does not inherit the table's row limit", () => {
    // 100 rows is a page of logs; it is also 100 bars, which would silently truncate the
    // right-hand end of any window with more buckets than that.
    expect(toHistogram({ ...QUERY, limit: 50 }, 60).limit).toBeGreaterThan(100);
  });
});

describe("toFieldCounts", () => {
  it("orders by the count, descending, so the sidebar shows the top values", () => {
    const counts = toFieldCounts(QUERY, { field: "severity" });
    expect(counts.group_by).toEqual([{ field: "severity" }]);
    expect(counts.order_by).toEqual([{ key: { by: "alias", alias: "n" }, desc: true }]);
    expect(counts.filter).toEqual(QUERY.filter);
  });

  it("asks for a handful, not a page", () => {
    // A field with ten thousand distinct values would otherwise pull all of them across
    // the wire to display eight.
    expect(toFieldCounts(QUERY, { field: "severity" }).limit).toBeLessThanOrEqual(20);
  });
});

describe("describeSeconds", () => {
  it("says what a bar is in words", () => {
    expect(describeSeconds(1)).toBe("1 second");
    expect(describeSeconds(30)).toBe("30 seconds");
    expect(describeSeconds(60)).toBe("1 minute");
    expect(describeSeconds(300)).toBe("5 minutes");
    expect(describeSeconds(3600)).toBe("1 hour");
    expect(describeSeconds(86_400)).toBe("1 day");
  });
});
