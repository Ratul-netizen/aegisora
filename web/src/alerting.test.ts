/**
 * The sentence a rule reads as, and the order alerts appear in.
 *
 * Both are the kind of thing that is wrong without being broken: a list renders, the
 * words are plausible, and the rule that gets silenced at 3am is not the one somebody
 * meant to silence.
 */

import { describe as group, expect, it } from "vitest";

import { ago, describe, describeSeconds, order, type Alert, type Rule } from "./alerting";
import type { Query } from "./query";

function rule(over: Partial<Rule> = {}): Rule {
  const query: Query = {
    signal: "metric",
    time: { start: "2026-09-17T10:00:00.000Z", end: "2026-09-17T10:05:00.000Z" },
    resources: { type: "all" },
    aggregations: [{ func: "avg", field: { field: "value" }, alias: "v" }],
    limit: 100,
  };
  return {
    id: "r1",
    name: "CPU hot",
    description: "",
    kind: "threshold",
    query,
    condition: { kind: "threshold", op: "gt", value: 90, hold_seconds: 300 },
    severity: "critical",
    enabled: true,
    eval_interval_seconds: 60,
    notify: [],
    created_at: "2026-09-17T10:00:00.000Z",
    updated_at: "2026-09-17T10:00:00.000Z",
    ...over,
  };
}

group("describe", () => {
  it("reads a threshold rule as the sentence somebody would say", () => {
    expect(describe(rule())).toBe("avg(value) greater than 90 for 5 minutes");
  });

  it("says nothing about a dwell a rule does not have", () => {
    // A rule that fires on the first breach is a different rule, and "for immediately"
    // is not a sentence.
    const eager = rule({
      condition: { kind: "threshold", op: "gt", value: 90, hold_seconds: 0 },
    });
    expect(describe(eager)).toBe("avg(value) greater than 90");
  });

  it("describes a saved search's rule as the row count it alerts on", () => {
    // The rule a saved Log Explorer search converts into has no aggregation: it alerts on
    // how many rows the search returns. The list has to read that way or it will not match
    // what the rule does.
    const fromSearch = rule({
      kind: "threshold",
      query: {
        signal: "log",
        time: { start: "2026-09-17T10:00:00.000Z", end: "2026-09-17T10:15:00.000Z" },
        resources: { type: "all" },
        filter: { op: "text", field: { field: "body" }, mode: "any_token", terms: ["crc"] },
        limit: 100,
      },
      condition: { kind: "threshold", op: "gt", value: 0, hold_seconds: 0 },
    });
    expect(describe(fromSearch)).toBe("log rows greater than 0");
  });

  it("reads an absence rule as the silence it is about", () => {
    const silent = rule({
      kind: "absence",
      condition: { kind: "absence", after_seconds: 300 },
    });
    expect(describe(silent)).toBe("no metric telemetry for 5 minutes");
  });

  it("names an attribute the rule groups on rather than the word attr", () => {
    const perInterface = rule({
      query: {
        ...rule().query,
        aggregations: [{ func: "max", field: { field: "attr", key: "if.errors" }, alias: "v" }],
      },
    });
    expect(describe(perInterface)).toContain("max(if.errors)");
  });
});

group("describeSeconds", () => {
  it("uses whole units, because every one of these was typed by a person", () => {
    expect(describeSeconds(60)).toBe("1 minute");
    expect(describeSeconds(300)).toBe("5 minutes");
    expect(describeSeconds(3_600)).toBe("1 hour");
    expect(describeSeconds(86_400)).toBe("1 day");
    expect(describeSeconds(90)).toBe("90 seconds");
    expect(describeSeconds(0)).toBe("immediately");
  });
});

group("order", () => {
  it("puts what is firing above what is only pending", () => {
    // Nobody has been told about a pending alert. It belongs on the screen — it is one
    // evaluation from paging — but under the things that already have.
    const alerts: Alert[] = [
      alert("a", "pending", "2026-09-17T10:00:00.000Z"),
      alert("b", "firing", "2026-09-17T10:04:00.000Z"),
      alert("c", "firing", "2026-09-17T10:01:00.000Z"),
    ];

    expect(order(alerts).map((a) => a.id)).toEqual(["c", "b", "a"]);
  });

  it("does not reorder the array it was given", () => {
    const alerts = [alert("a", "pending", "2026-09-17T10:00:00.000Z")];
    order(alerts);
    expect(alerts[0]?.id).toBe("a");
  });
});

group("ago", () => {
  it("rounds down, so a screen refreshed twice does not report two durations", () => {
    const now = Date.parse("2026-09-17T10:02:00.000Z");
    expect(ago("2026-09-17T10:01:59.000Z", now)).toBe("1s");
    expect(ago("2026-09-17T10:00:01.000Z", now)).toBe("1m");
    expect(ago("2026-09-17T08:03:00.000Z", now)).toBe("1h");
    expect(ago("2026-09-15T10:01:00.000Z", now)).toBe("2d");
    // And a minute short of two days is one day, which is the rounding working.
    expect(ago("2026-09-15T10:03:00.000Z", now)).toBe("1d");
  });

  it("never reports a negative age for a clock that is ahead", () => {
    const now = Date.parse("2026-09-17T10:00:00.000Z");
    expect(ago("2026-09-17T10:05:00.000Z", now)).toBe("0s");
  });
});

function alert(id: string, state: Alert["state"], since: string): Alert {
  return {
    id,
    rule_id: "r1",
    rule: "CPU hot",
    severity: "critical",
    resource_id: "res",
    resource: "rtr-01",
    dedup_key: `r1/${id}`,
    state,
    since,
    last_eval: since,
    last_value: 95,
  };
}
