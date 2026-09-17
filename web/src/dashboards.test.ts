/**
 * What a panel asks for, and what it draws from the answer.
 *
 * Both are wrong-without-being-broken: a chart renders, the shape is plausible, and it is
 * a picture of a different window or a different column than the one on the title.
 */

import { describe, expect, it } from "vitest";

import { forWindow, latest, toSeries } from "./dashboards";
import type { Query, ResultSet } from "./query";

const PANEL: Query = {
  signal: "metric",
  // Saved over an hour, with the five-minute buckets that suits.
  time: { start: "2026-09-17T09:00:00.000Z", end: "2026-09-17T10:00:00.000Z" },
  resources: { type: "all" },
  aggregations: [{ func: "avg", field: { field: "value" }, alias: "v" }],
  group_by: [{ field: "time_bucket", seconds: 300 }],
  limit: 500,
};

function result(columns: string[], rows: unknown[][]): ResultSet {
  return {
    columns: columns.map((name) => ({ name, type: "Float64" })),
    rows,
    table: "metrics",
    warnings: [],
    rows_read: 0,
    bytes_read: 0,
  };
}

describe("forWindow", () => {
  it("moves the window to the one in the header", () => {
    const from = new Date("2026-09-20T00:00:00.000Z");
    const to = new Date("2026-09-20T06:00:00.000Z");
    const asked = forWindow(PANEL, from, to);

    expect(asked.time).toEqual({
      start: "2026-09-20T00:00:00.000Z",
      end: "2026-09-20T06:00:00.000Z",
    });
  });

  it("widens the bucket with the window, so a month is not 8 640 buckets", () => {
    // The failure this prevents: the panel's own limit truncates the result, and the
    // chart shows the first few hundred buckets as if they were the whole month.
    const month = forWindow(
      PANEL,
      new Date("2026-08-20T00:00:00.000Z"),
      new Date("2026-09-19T00:00:00.000Z"),
    );

    const bucket = month.group_by?.[0];
    expect(bucket?.field).toBe("time_bucket");
    const seconds = bucket && "seconds" in bucket ? bucket.seconds : 0;
    expect(seconds).toBeGreaterThan(300);
    // Thirty days at that width has to fit inside what the panel asks for.
    expect((30 * 86_400) / seconds).toBeLessThanOrEqual(PANEL.limit);
  });

  it("leaves a panel that does not group by time alone", () => {
    const table: Query = { ...PANEL, group_by: [{ field: "resource_id" }] };
    expect(forWindow(table, new Date(0), new Date(3_600_000)).group_by).toEqual(
      table.group_by,
    );
  });

  it("changes nothing else about the query", () => {
    const asked = forWindow(PANEL, new Date(0), new Date(3_600_000));
    expect({ ...asked, time: PANEL.time, group_by: PANEL.group_by }).toEqual(PANEL);
  });
});

describe("toSeries", () => {
  it("reads one line from a bucket and a value", () => {
    const lines = toSeries(
      result(
        ["bucket", "v"],
        [
          ["2026-09-17 09:00:00", 10],
          ["2026-09-17 09:05:00", 20],
        ],
      ),
    );

    expect(lines).toHaveLength(1);
    expect(lines[0]?.name).toBe("v");
    expect(lines[0]?.points.map((p) => p.value)).toEqual([10, 20]);
  });

  it("names a line after what the panel grouped by", () => {
    const lines = toSeries(
      result(
        ["bucket", "host.name", "v"],
        [
          ["2026-09-17 09:00:00", "rtr-01", 10],
          ["2026-09-17 09:00:00", "rtr-02", 30],
          ["2026-09-17 09:05:00", "rtr-01", 11],
        ],
      ),
    );

    expect(lines.map((l) => l.name).sort()).toEqual(["rtr-01", "rtr-02"]);
    expect(lines.find((l) => l.name === "rtr-01")?.points).toHaveLength(2);
  });

  it("drops a null rather than drawing an outage as a collapse to zero", () => {
    // `avg()` over a bucket with no samples is null. Zero is a different fact, and a much
    // more alarming picture than the one the data supports.
    const lines = toSeries(
      result(
        ["bucket", "v"],
        [
          ["2026-09-17 09:00:00", 10],
          ["2026-09-17 09:05:00", null],
          ["2026-09-17 09:10:00", 12],
        ],
      ),
    );

    expect(lines[0]?.points.map((p) => p.value)).toEqual([10, 12]);
  });

  it("puts a line's points in time order whatever order they arrived in", () => {
    const lines = toSeries(
      result(
        ["bucket", "v"],
        [
          ["2026-09-17 09:10:00", 3],
          ["2026-09-17 09:00:00", 1],
          ["2026-09-17 09:05:00", 2],
        ],
      ),
    );

    expect(lines[0]?.points.map((p) => p.value)).toEqual([1, 2, 3]);
  });

  it("reads a quoted number, because ClickHouse quotes 64-bit integers", () => {
    const lines = toSeries(result(["bucket", "n"], [["2026-09-17 09:00:00", "42"]]));
    expect(lines[0]?.points[0]?.value).toBe(42);
  });
});

describe("latest", () => {
  it("is the most recent bucket, which is what a stat panel is read for", () => {
    expect(
      latest(
        result(
          ["bucket", "v"],
          [
            ["2026-09-17 09:00:00", 10],
            ["2026-09-17 09:05:00", 20],
          ],
        ),
      ),
    ).toBe(20);
  });

  it("skips trailing nulls rather than reporting no data because of one empty bucket", () => {
    expect(
      latest(
        result(
          ["bucket", "v"],
          [
            ["2026-09-17 09:00:00", 7],
            ["2026-09-17 09:05:00", null],
          ],
        ),
      ),
    ).toBe(7);
  });

  it("is null when there is nothing, which is not the same as zero", () => {
    expect(latest(result(["bucket", "v"], []))).toBeNull();
  });
});
