/**
 * The query AST, as TypeScript.
 *
 * SPEC §M0.5: the UI builds this, saved alerts are instances of it, and the M6 text
 * language parses onto it. There is one path to telemetry and this is its front door —
 * so these types mirror `uops_query::ast` exactly, including the serde tags (`type` on
 * a resource selector, `op` on an expression, `field` on a field), because a mismatch
 * here is a 422 with a serde message rather than anything a person can act on.
 *
 * Only the parts the Explorer builds so far are modelled. Aggregations, grouping and
 * ordering exist in the Rust AST and will be added here alongside the UI that produces
 * them; declaring them now would be declaring an interface nothing implements.
 */

import { ApiError, request } from "./api";

export type Signal = "metric" | "log" | "event" | "state";

/** The signals the Explorer offers. `trace` and `flow` are deferred — SPEC §M0.5. */
export const SIGNALS: { value: Signal; label: string }[] = [
  { value: "log", label: "Logs" },
  { value: "event", label: "Events" },
  { value: "state", label: "State changes" },
  { value: "metric", label: "Metrics" },
];

export type Field =
  | { field: "body" }
  | { field: "severity" }
  | { field: "source_kind" }
  | { field: "source_vendor" }
  | { field: "event_category" }
  | { field: "event_type" }
  | { field: "metric" }
  | { field: "resource_id" }
  | { field: "observed_at" }
  | { field: "attr"; key: string }
  | { field: "time_bucket"; seconds: number };

export type TextMode = "any_token" | "all_token" | "substring" | "phrase";

export type Expr =
  | { op: "and"; of: Expr[] }
  | { op: "or"; of: Expr[] }
  | { op: "not"; of: Expr }
  | { op: "compare"; field: Field; cmp: "eq" | "ne" | "in" | "not_in"; value: unknown }
  | { op: "text"; field: Field; mode: TextMode; terms: string[] }
  | { op: "exists"; field: Field };

export interface Query {
  signal: Signal;
  time: { start: string; end: string };
  resources: { type: "all" } | { type: "ids"; ids: string[] };
  filter?: Expr;
  limit: number;
  offset?: number;
}

export interface Column {
  name: string;
  type: string;
}

/**
 * A warning the server attached to a result.
 *
 * `message` is written by `QueryWarning::message()` in Rust and sent on the wire. This
 * app deliberately does not keep its own copy of the wording: a switch statement here
 * would not fail the day a variant is added, it would fall through to a default and
 * print `not_index_accelerated` — losing the half of the warning that says what to do.
 */
export interface QueryWarning {
  warning: string;
  message: string;
  [key: string]: unknown;
}

export interface ResultSet {
  columns: Column[];
  /** Values in column order — the server sends JSONCompact, not row objects. */
  rows: unknown[][];
  /** Which physical table answered. The first question asked of any slow query. */
  table: string;
  warnings: QueryWarning[];
  rows_read: number;
  bytes_read: number;
}

export function runQuery(tenant: string, query: Query, signal?: AbortSignal) {
  return request<ResultSet>("/api/v1/query", {
    method: "POST",
    body: query,
    tenant,
    ...(signal ? { signal } : {}),
  });
}

/**
 * Severity, in the order the server's enum declares it.
 *
 * Not alphabetical, and not a set: "at least this severe" is the filter people actually
 * want, and it needs an order. `emergency` sorts last because it is the most severe,
 * which is the opposite of how the word reads in a list.
 */
export const SEVERITIES = [
  "trace",
  "debug",
  "info",
  "notice",
  "warn",
  "error",
  "critical",
  "alert",
  "emergency",
] as const;

export type Severity = (typeof SEVERITIES)[number];

/** The severities at or above `floor`, for an `in` comparison. */
export function atLeast(floor: Severity): Severity[] {
  return SEVERITIES.slice(SEVERITIES.indexOf(floor));
}

/**
 * Build the filter expression from what the Explorer's controls hold.
 *
 * Returns undefined rather than an empty `and`, because `filter: null` is "no filter"
 * and `{"op":"and","of":[]}` is a predicate the compiler has to decide about. The AST
 * accepts both; only one of them is obviously what was meant.
 */
export function buildFilter(opts: {
  signal: Signal;
  search: string;
  mode: TextMode;
  severity: Severity | "";
}): Expr | undefined {
  const terms: Expr[] = [];

  const search = opts.search.trim();
  if (search) {
    // Logs search the body; events search the event type. Metrics have no free text at
    // all, so the box is hidden for them rather than silently ignored.
    const field: Field | null =
      opts.signal === "log"
        ? { field: "body" }
        : opts.signal === "event"
          ? { field: "event_type" }
          : null;

    if (field) {
      terms.push({
        op: "text",
        field,
        mode: opts.mode,
        // Whitespace-separated for the token modes; one term for substring and phrase,
        // where splitting would change the question being asked.
        terms:
          opts.mode === "substring" || opts.mode === "phrase"
            ? [search]
            : search.split(/\s+/).filter(Boolean),
      });
    }
  }

  if (opts.severity && (opts.signal === "log" || opts.signal === "event")) {
    terms.push({
      op: "compare",
      field: { field: "severity" },
      cmp: "in",
      value: atLeast(opts.severity),
    });
  }

  if (terms.length === 0) return undefined;
  if (terms.length === 1) return terms[0];
  return { op: "and", of: terms };
}

export function isAbort(error: unknown): boolean {
  return error instanceof DOMException && error.name === "AbortError";
}

export function message(error: unknown): string {
  if (error instanceof ApiError) return error.message;
  return String(error);
}
