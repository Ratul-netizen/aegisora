/**
 * Alerts, rules and channels, as the API hands them over.
 *
 * The one piece of logic in here is `describe`, and it is the reason this file has tests:
 * a rule is a `Query` AST plus a condition, and the sentence a person reads off a rule
 * list is the only place those two are turned back into English. Getting it wrong means a
 * list where two different rules read identically — and the moment that matters is when
 * somebody is deciding which one to silence at 3am.
 */

import { request } from "./api";
import type { Query } from "./query";

export type Phase = "ok" | "pending" | "firing" | "resolved";
export type Severity = "info" | "warning" | "critical";

export type Condition =
  | {
      kind: "threshold";
      op: "gt" | "gte" | "lt" | "lte" | "eq" | "ne";
      value: number;
      hold_seconds: number;
    }
  | { kind: "absence"; after_seconds: number };

export interface Alert {
  id: string;
  rule_id: string;
  /** Joined server-side, so a list of forty does not make forty requests. */
  rule: string;
  severity: Severity;
  resource_id: string;
  resource: string;
  dedup_key: string;
  state: Phase;
  since: string;
  last_eval: string;
  last_value: number | null;
  acked_at?: string;
}

export interface Rule {
  id: string;
  name: string;
  description: string;
  kind: "threshold" | "absence";
  query: Query;
  condition: Condition;
  severity: Severity;
  enabled: boolean;
  eval_interval_seconds: number;
  notify: string[];
  created_at: string;
  updated_at: string;
}

export interface RuleRequest {
  name: string;
  description?: string;
  query: Query;
  condition: Condition;
  severity: Severity;
  enabled?: boolean;
  eval_interval_seconds?: number;
  notify?: string[];
}

export interface Channel {
  id: string;
  name: string;
  kind: string;
  config: Record<string, unknown>;
  enabled: boolean;
  max_per_minute: number;
  created_at: string;
  updated_at: string;
}

export interface ChannelRequest {
  name: string;
  kind: string;
  config: Record<string, unknown>;
  enabled?: boolean;
  max_per_minute?: number;
}

export interface Sent {
  id: string;
  channel_id: string;
  rule_id: string;
  dedup_key: string;
  phase: string;
  /** `sent` | `failed` | `rate_limited` | `over_budget`. */
  outcome: string;
  detail: string;
  sent_at: string;
}

export const SEVERITIES: Severity[] = ["info", "warning", "critical"];

/** The comparisons a threshold rule can make, in the words a person types. */
export const COMPARISONS: { value: string; label: string }[] = [
  { value: "gt", label: "greater than" },
  { value: "gte", label: "at least" },
  { value: "lt", label: "less than" },
  { value: "lte", label: "at most" },
  { value: "eq", label: "equal to" },
  { value: "ne", label: "not equal to" },
];

export function listAlerts(tenant: string) {
  return request<Alert[]>("/api/v1/alerts", { tenant });
}

export function acknowledge(tenant: string, id: string) {
  return request<Alert>(`/api/v1/alerts/${id}/ack`, { method: "POST", tenant });
}

export function listRules(tenant: string) {
  return request<Rule[]>("/api/v1/alerts/rules", { tenant });
}

export function createRule(tenant: string, body: RuleRequest) {
  return request<Rule>("/api/v1/alerts/rules", { method: "POST", body, tenant });
}

export function replaceRule(tenant: string, id: string, body: RuleRequest) {
  return request<Rule>(`/api/v1/alerts/rules/${id}`, { method: "PUT", body, tenant });
}

export function setRuleEnabled(tenant: string, id: string, enabled: boolean) {
  return request<Rule>(`/api/v1/alerts/rules/${id}/enabled`, {
    method: "PATCH",
    body: { enabled },
    tenant,
  });
}

export function deleteRule(tenant: string, id: string) {
  return request<void>(`/api/v1/alerts/rules/${id}`, { method: "DELETE", tenant });
}

export function listChannels(tenant: string) {
  return request<Channel[]>("/api/v1/channels", { tenant });
}

export function createChannel(tenant: string, body: ChannelRequest) {
  return request<Channel>("/api/v1/channels", { method: "POST", body, tenant });
}

export function deleteChannel(tenant: string, id: string) {
  return request<void>(`/api/v1/channels/${id}`, { method: "DELETE", tenant });
}

export function listSent(tenant: string) {
  return request<Sent[]>("/api/v1/notifications", { tenant });
}

/**
 * A duration in the words somebody would say it in.
 *
 * Whole units only, because every duration this shows is one a person typed: an
 * evaluation interval, a dwell, an absence window. "1h 0m 0s" is a machine describing 3600
 * to another machine.
 */
export function describeSeconds(seconds: number): string {
  if (seconds <= 0) return "immediately";
  if (seconds % 86_400 === 0) return plural(seconds / 86_400, "day");
  if (seconds % 3_600 === 0) return plural(seconds / 3_600, "hour");
  if (seconds % 60 === 0) return plural(seconds / 60, "minute");
  return plural(seconds, "second");
}

function plural(n: number, unit: string): string {
  return `${n} ${unit}${n === 1 ? "" : "s"}`;
}

/**
 * What a rule says, as a sentence.
 *
 * The only place the AST and the condition are turned back into English, and the thing a
 * person reads when they are choosing which rule to silence. It deliberately names the
 * *signal* and the aggregate rather than paraphrasing the filter: a filter can be an
 * arbitrary tree, and a summary that flattened one would be a sentence that is sometimes
 * a lie. "matching a saved search" is true of every one of them.
 */
export function describe(rule: Rule): string {
  // Bound to a local so the narrowing survives: reading `rule.condition` again after the
  // early return gives the compiler back the whole union.
  const condition = rule.condition;
  if (condition.kind === "absence") {
    return `no ${rule.query.signal} telemetry for ${describeSeconds(
      condition.after_seconds,
    )}`;
  }

  const comparison =
    COMPARISONS.find((c) => c.value === condition.op)?.label ?? condition.op;
  const subject = aggregateOf(rule);
  const held =
    condition.hold_seconds > 0 ? ` for ${describeSeconds(condition.hold_seconds)}` : "";

  return `${subject} ${comparison} ${condition.value}${held}`;
}

/**
 * The left-hand side of a threshold: the rule's own aggregate, or the row count.
 *
 * A rule with no aggregation alerts on how many rows its search returns — that is what
 * makes a saved Log Explorer search convertible without edits, and it has to read that way
 * here too or the list will not match what the rule does.
 */
function aggregateOf(rule: Rule): string {
  const aggregation = rule.query.aggregations?.[0];
  if (!aggregation) return `${rule.query.signal} rows`;

  const field = aggregation.field;
  if (!field) return `count of ${rule.query.signal}`;

  const name = "key" in field ? field.key : field.field;
  return `${aggregation.func}(${name})`;
}

/** Firing first, then pending; within each, the longest-standing first. */
export function order(alerts: Alert[]): Alert[] {
  const rank = (a: Alert) => (a.state === "firing" ? 0 : 1);
  return [...alerts].sort(
    (a, b) => rank(a) - rank(b) || Date.parse(a.since) - Date.parse(b.since),
  );
}

/**
 * How long an alert has been in its current state, as a person would say it.
 *
 * Rounded down to the unit shown: an alert that fired 119 seconds ago has been firing for
 * a minute, not two. Rounding up would have a screen refreshed twice in a row report
 * different durations for an alert that did not change.
 */
export function ago(since: string, now = Date.now()): string {
  const seconds = Math.max(0, Math.floor((now - Date.parse(since)) / 1000));
  if (seconds < 60) return `${seconds}s`;
  if (seconds < 3_600) return `${Math.floor(seconds / 60)}m`;
  if (seconds < 86_400) return `${Math.floor(seconds / 3_600)}h`;
  return `${Math.floor(seconds / 86_400)}d`;
}
