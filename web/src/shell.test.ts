/**
 * The time range is parsed from a string that a person can edit in the address bar, and
 * it decides what every query in the product asks for. It is the one piece of logic in
 * this app that can be quietly wrong — a range that silently resolves backwards returns
 * no rows, and "there is no data" is indistinguishable from "the system is broken".
 */

import { describe, expect, it } from "vitest";

import {
  DEFAULT_RANGE,
  describeRange,
  resolveInstant,
  resolveRange,
  validateShellSearch,
} from "./shell";

const NOW = Date.parse("2026-09-15T12:00:00.000Z");

describe("resolveInstant", () => {
  it("reads every relative unit", () => {
    const cases: [string, string][] = [
      ["now", "2026-09-15T12:00:00.000Z"],
      ["now-30s", "2026-09-15T11:59:30.000Z"],
      ["now-15m", "2026-09-15T11:45:00.000Z"],
      ["now-6h", "2026-09-15T06:00:00.000Z"],
      ["now-7d", "2026-09-08T12:00:00.000Z"],
      ["now-2w", "2026-09-01T12:00:00.000Z"],
    ];
    for (const [input, expected] of cases) {
      expect(resolveInstant(input, NOW)?.toISOString(), input).toBe(expected);
    }
  });

  it("reads an absolute instant", () => {
    expect(resolveInstant("2026-01-02T03:04:05Z", NOW)?.toISOString()).toBe(
      "2026-01-02T03:04:05.000Z",
    );
  });

  it("returns null rather than a guess", () => {
    // These arrive by someone editing the URL. Guessing produces a window nobody asked
    // for and no indication that it happened.
    for (const junk of ["", "yesterday", "now-1", "now-1y", "now+1h", "1h"]) {
      expect(resolveInstant(junk, NOW), junk).toBeNull();
    }
  });
});

describe("resolveRange", () => {
  it("resolves both ends against one clock", () => {
    // The reason resolveInstant takes `now` instead of reading it. Two calls to
    // Date.now() can straddle a millisecond and produce from > to, which returns
    // nothing and cannot be reproduced.
    const resolved = resolveRange({ from: "now", to: "now" }, NOW);
    expect(resolved?.from.getTime()).toBe(resolved?.to.getTime());
  });

  it("is null when either end is unreadable", () => {
    expect(resolveRange({ from: "now-1h", to: "nonsense" }, NOW)).toBeNull();
    expect(resolveRange({ from: "nonsense", to: "now" }, NOW)).toBeNull();
  });

  it("does not reorder a backwards range", () => {
    // Deliberate: a range the user wrote backwards is a mistake they should see, not
    // one this function should quietly repair into a different question.
    const resolved = resolveRange({ from: "now", to: "now-1h" }, NOW);
    expect(resolved).not.toBeNull();
    expect(resolved!.from.getTime()).toBeGreaterThan(resolved!.to.getTime());
  });

  it("keeps a relative range relative", () => {
    // The property that matters for a dashboard left open overnight: the same range
    // string resolves to a later window as the clock moves.
    const early = resolveRange(DEFAULT_RANGE, NOW)!;
    const later = resolveRange(DEFAULT_RANGE, NOW + 3_600_000)!;
    expect(later.to.getTime() - early.to.getTime()).toBe(3_600_000);
  });
});

describe("describeRange", () => {
  it("names a preset rather than spelling out its instants", () => {
    expect(describeRange({ from: "now-1h", to: "now" })).toBe("Last 1h");
    expect(describeRange({ from: "now-7d", to: "now" })).toBe("Last 7d");
  });

  it("falls back to instants for anything else", () => {
    expect(describeRange({ from: "now-90m", to: "now" })).toContain("→");
  });

  it("says so when the range cannot be read", () => {
    expect(describeRange({ from: "nonsense", to: "now" })).toBe("Invalid range");
  });
});

describe("validateShellSearch", () => {
  it("keeps the three parameters it knows", () => {
    expect(
      validateShellSearch({ tenant: "t-1", from: "now-6h", to: "now", other: "x" }),
    ).toEqual({ tenant: "t-1", from: "now-6h", to: "now" });
  });

  it("drops rather than rejects", () => {
    // A URL is something people edit and paste. A whole page that refuses to render
    // because one parameter is the wrong type is worse than one that uses a default.
    expect(validateShellSearch({ tenant: 42, from: null, to: undefined })).toEqual({});
    expect(validateShellSearch({})).toEqual({});
  });

  it("treats an empty string as absent", () => {
    // `?tenant=` is what a form submits when nothing is chosen, and it must not select
    // a tenant whose id is the empty string — there isn't one.
    expect(validateShellSearch({ tenant: "", from: "" })).toEqual({});
  });
});
