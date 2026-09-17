/**
 * The query AST, as TypeScript.
 *
 * SPEC §M0.5: the UI builds this, saved alerts are instances of it, and the M6 text
 * language parses onto it. There is one path to telemetry and this is its front door —
 * so these types mirror `uops_query::ast` exactly, including the serde tags (`type` on
 * a resource selector, `op` on an expression, `field` on a field), because a mismatch
 * here is a 422 with a serde message rather than anything a person can act on.
 *
 * Only the parts the Explorer builds are modelled. Everything here has a control that
 * produces it — declaring an interface nothing implements would be declaring a promise.
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
  // Metrics only, and not built by any control in the Explorer: an alert rule aggregates
  // over `value`, and the rule list renders the aggregate it was given. Modelled here
  // because the AST is one type across both, not two that drift.
  | { field: "value" }
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

/** The aggregate functions the Explorer asks for. The Rust AST has more. */
export type AggFunc = "count" | "sum" | "avg" | "min" | "max";

export interface Aggregation {
  func: AggFunc;
  /** `null` only for `count`. */
  field?: Field | null;
  /**
   * The output column's name.
   *
   * Validated server-side as an identifier, because it is the one caller-supplied string
   * that reaches the statement text. Kept short and fixed in this app rather than typed
   * by a user.
   */
  alias: string;
}

export type SortKey = { by: "field"; field: Field } | { by: "alias"; alias: string };

export interface Sort {
  key: SortKey;
  desc?: boolean;
}

export interface Query {
  signal: Signal;
  time: { start: string; end: string };
  resources: { type: "all" } | { type: "ids"; ids: string[] };
  filter?: Expr;
  aggregations?: Aggregation[];
  group_by?: Field[];
  order_by?: Sort[];
  limit: number;
  offset?: number;
}

/**
 * A bucket width that puts roughly `target` bars in a window.
 *
 * Snapped to a round number of seconds rather than computed exactly, because a histogram
 * whose bars are 37 seconds wide is one nobody can reason about — "each bar is five
 * minutes" is a sentence, and 37 seconds is an artefact of the window somebody happened
 * to pick.
 *
 * The floor is one second: `time_bucket` is `toStartOfInterval` server-side and a
 * sub-second bucket would ask ClickHouse for more bars than there are pixels.
 *
 * **The ceiling is one day, and it is the server's.** `uops_query`'s compiler rejects a
 * bucket outside 1 second to 1 day outright — *"time bucket must be between 1 second and
 * 1 day"* — so a window long enough to want two-day bars would have produced a 400 rather
 * than a chart. A year of daily bars is 365 of them, which is more than 60 and perfectly
 * readable; asking for wider ones buys nothing and fails.
 */
export const MAX_BUCKET_SECONDS = 86_400;

export function bucketSeconds(fromMs: number, toMs: number, target = 60): number {
  const span = Math.max(1, Math.round((toMs - fromMs) / 1000));
  const ideal = span / target;
  const steps = [
    1, 5, 10, 15, 30, 60, 120, 300, 600, 900, 1800, 3600, 7200, 10800, 21600, 43200,
    MAX_BUCKET_SECONDS,
  ];
  // The fallback is named rather than `at(-1)`: the compiler cannot know the literal is
  // non-empty, and an assertion would be a claim it cannot check.
  return steps.find((s) => s >= ideal) ?? MAX_BUCKET_SECONDS;
}

/**
 * The same query, counted per time bucket instead of listed.
 *
 * Deliberately derived from the query the table ran rather than assembled separately: a
 * histogram that filtered differently from the rows beneath it would be a chart of
 * something else, and nobody would notice until they counted the bars.
 */
export function toHistogram(query: Query, seconds: number): Query {
  const bucket: Field = { field: "time_bucket", seconds };
  return {
    ...query,
    aggregations: [{ func: "count", alias: "n" }],
    group_by: [bucket],
    order_by: [{ key: { by: "field", field: bucket } }],
    // One row per bucket. The server caps this anyway; asking for the window's worth of
    // buckets and no more is what makes the cap irrelevant.
    limit: 1000,
    offset: 0,
  };
}

/**
 * The same query, counted per distinct value of one field.
 *
 * What the field sidebar shows. `limit` is small because the sidebar shows a handful and
 * a field with ten thousand distinct values — a resource id, say — would otherwise pull
 * all of them across the wire to display eight.
 */
export function toFieldCounts(query: Query, field: Field, limit = 8): Query {
  return {
    ...query,
    aggregations: [{ func: "count", alias: "n" }],
    group_by: [field],
    order_by: [{ key: { by: "alias", alias: "n" }, desc: true }],
    limit,
    offset: 0,
  };
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

/**
 * The order a person reads a telemetry row in, as indices into `columns`.
 *
 * The compiler's `SELECT` order is the table's: `tenant_id, resource_id, site_id,
 * observed_at, …, body, …`. That is the right order for a machine and the wrong one for a
 * screen — three UUIDs occupy the whole width before the message anybody opened the
 * Explorer to read. Seen the moment the page was pointed at real data.
 *
 * So: when, how bad, from where, and what it said — then everything else in the order it
 * arrived. `tenant_id` is dropped outright, because every row in a tenant-scoped view has
 * the same one and a column of identical UUIDs is not information.
 *
 * Indices rather than a rearranged array, so the caller still reads cells by their
 * original position and nothing has to stay in step.
 */
export function displayOrder(columns: Column[]): number[] {
  const preferred = [
    "observed_at",
    "severity",
    "source_kind",
    "source_vendor",
    "metric",
    "value",
    "unit",
    "event_category",
    "event_type",
    "previous_status",
    "current_status",
    "body",
  ];

  return columns
    .map((column, index) => ({ column, index }))
    .filter(({ column }) => column.name !== "tenant_id")
    .sort((a, b) => {
      const rank = (name: string) => {
        const at = preferred.indexOf(name);
        return at === -1 ? preferred.length : at;
      };
      // Ties keep the compiler's order, so the columns nobody named stay where they were
      // relative to each other.
      return rank(a.column.name) - rank(b.column.name) || a.index - b.index;
    })
    .map(({ index }) => index);
}
