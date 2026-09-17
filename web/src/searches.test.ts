/**
 * Reading a saved search back into the Explorer's controls.
 *
 * `buildFilter` turns controls into an AST and `toControls` turns one back. The property
 * that matters is that the pair is a round trip for every filter the form can build —
 * because the day it is not, opening a saved search shows an operator a form that says
 * one thing while the rows beneath it are the answer to another.
 *
 * The other half is honesty about what the form cannot express. A filter with an `or` in
 * it must come back marked inexact rather than silently truncated to the parts that
 * happened to fit.
 */

import { describe, expect, it } from "vitest";

import { buildFilter, type Query, type Severity, type TextMode } from "./query";
import { overWindow, toControls } from "./searches";

function query(filter: Query["filter"], limit = 100): Query {
  return {
    signal: "log",
    time: { start: "2026-09-17T10:00:00.000Z", end: "2026-09-17T10:15:00.000Z" },
    resources: { type: "all" },
    ...(filter ? { filter } : {}),
    limit,
  };
}

describe("toControls", () => {
  it("round-trips every filter the form can build", () => {
    const cases: { search: string; mode: TextMode; severity: Severity | "" }[] = [
      { search: "", mode: "any_token", severity: "" },
      { search: "connection refused", mode: "any_token", severity: "" },
      { search: "connection refused", mode: "all_token", severity: "error" },
      { search: "connection refused", mode: "phrase", severity: "" },
      { search: "interface reset", mode: "substring", severity: "warn" },
      { search: "", mode: "any_token", severity: "critical" },
    ];

    for (const c of cases) {
      const filter = buildFilter({ signal: "log", ...c });
      const back = toControls(query(filter));

      expect(back.exact, JSON.stringify(c)).toBe(true);
      expect(back.search, JSON.stringify(c)).toBe(c.search);
      expect(back.severity, JSON.stringify(c)).toBe(c.severity);
      // The match mode is only meaningful when there is something to match, and an empty
      // box carries no mode into the AST to read back.
      if (c.search) expect(back.mode, JSON.stringify(c)).toBe(c.mode);
    }
  });

  it("reads the sidebar's chips back as chips", () => {
    const back = toControls(
      query({
        op: "and",
        of: [
          { op: "text", field: { field: "body" }, mode: "any_token", terms: ["bgp"] },
          { op: "compare", field: { field: "source_kind" }, cmp: "eq", value: "syslog" },
          {
            op: "compare",
            field: { field: "attr", key: "host.name" },
            cmp: "eq",
            value: "rtr-01",
          },
        ],
      }),
    );

    expect(back.exact).toBe(true);
    expect(back.picked).toEqual([
      { field: { field: "source_kind" }, value: "syslog" },
      { field: { field: "attr", key: "host.name" }, value: "rtr-01" },
    ]);
  });

  it("says so when the form cannot show the whole filter", () => {
    // An `or` is expressible in the AST, will be expressible in the M6 text language,
    // and has no representation in this form. Marked rather than dropped: the rows are
    // still the saved search's, and the operator needs to know that pressing Run would
    // replace this filter with the weaker one the form describes.
    const back = toControls(
      query({
        op: "or",
        of: [
          { op: "text", field: { field: "body" }, mode: "any_token", terms: ["a"] },
          { op: "text", field: { field: "body" }, mode: "any_token", terms: ["b"] },
        ],
      }),
    );

    expect(back.exact).toBe(false);
    expect(back.search).toBe("");
  });

  it("does not read a severity set that is not a floor", () => {
    // "warn or emergency, but not error" is a real filter and is not what the dropdown
    // means. Showing `warn` for it would be readable, plausible, and not what runs.
    const back = toControls(
      query({
        op: "compare",
        field: { field: "severity" },
        cmp: "in",
        value: ["warn", "emergency"],
      }),
    );

    expect(back.severity).toBe("");
    expect(back.exact).toBe(false);
  });

  it("keeps the signal and the limit", () => {
    const back = toControls({ ...query(undefined, 500), signal: "metric" });
    expect(back.signal).toBe("metric");
    expect(back.limit).toBe(500);
  });
});

describe("overWindow", () => {
  it("replaces the stored window and nothing else", () => {
    // The stored window is provenance — the AST has no relative windows, so a saved
    // search carries whatever it was saved over. Everything else is the question itself
    // and must survive untouched.
    const saved = query({
      op: "text",
      field: { field: "body" },
      mode: "phrase",
      terms: ["link down"],
    });
    const opened = overWindow(
      saved,
      new Date("2026-09-17T12:00:00.000Z"),
      new Date("2026-09-17T13:00:00.000Z"),
    );

    expect(opened.time).toEqual({
      start: "2026-09-17T12:00:00.000Z",
      end: "2026-09-17T13:00:00.000Z",
    });
    expect({ ...opened, time: saved.time }).toEqual(saved);
  });
});
