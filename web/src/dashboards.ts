/**
 * Dashboards: a name, and an ordered list of panels.
 *
 * A panel is a `Query` AST and a picture to draw it as — the third thing in this app built
 * on that one type, after the Explorer and alert rules.
 *
 * The two functions with tests are the two that are quietly wrong when they are wrong:
 * `forWindow`, which decides what a panel actually asks for when the header's range
 * changes, and `toSeries`, which decides which column of a result set is a line.
 */

import { request } from "./api";
import { bucketSeconds, type Query, type ResultSet } from "./query";

export type Viz =
  | { kind: "time_series"; unit?: string }
  | { kind: "stat"; unit?: string; decimals?: number }
  | { kind: "table" }
  | { kind: "gauge"; min: number; max: number; unit?: string }
  | { kind: "alerts" };

export interface Panel {
  id: string;
  title: string;
  /**
   * Absent only for the alerts panel, which reads the control plane.
   *
   * `| undefined` spelled out because this project has `exactOptionalPropertyTypes` on,
   * and a panel built from a kind that takes no query genuinely carries the key with
   * nothing in it.
   */
  query?: Query | undefined;
  viz: Viz;
  /** Columns of twelve. */
  width: number;
  /** Rows of `ROW_HEIGHT`. */
  height: number;
}

export interface Dashboard {
  id: string;
  name: string;
  description: string;
  panels: Panel[];
  created_at: string;
  updated_at: string;
}

export interface DashboardRequest {
  name: string;
  description?: string;
  panels: Panel[];
}

/** How tall one grid row is, in pixels. */
export const ROW_HEIGHT = 110;

export function listDashboards(tenant: string) {
  return request<Dashboard[]>("/api/v1/dashboards", { tenant });
}

export function getDashboard(tenant: string, id: string) {
  return request<Dashboard>(`/api/v1/dashboards/${id}`, { tenant });
}

export function createDashboard(tenant: string, body: DashboardRequest) {
  return request<Dashboard>("/api/v1/dashboards", { method: "POST", body, tenant });
}

export function replaceDashboard(tenant: string, id: string, body: DashboardRequest) {
  return request<Dashboard>(`/api/v1/dashboards/${id}`, {
    method: "PUT",
    body,
    tenant,
  });
}

export function deleteDashboard(tenant: string, id: string) {
  return request<void>(`/api/v1/dashboards/${id}`, { method: "DELETE", tenant });
}

/**
 * What a panel actually asks for, over the range in the header.
 *
 * Two substitutions, and the second is the one that matters:
 *
 * 1. **The window.** Stored absolute, like every `Query` in this product; replaced on
 *    read, like a saved search's. A panel that kept asking about the afternoon it was
 *    created on would be a screenshot.
 *
 * 2. **The bucket.** A panel saved over an hour has five-minute buckets in its
 *    `group_by`. Viewed over thirty days that is 8 640 of them — more than the panel's
 *    own limit, so the chart would silently show the first few hundred and look like a
 *    complete picture of a shorter period. Rewriting the bucket to suit the window is
 *    what makes a dashboard's time picker mean anything, and it is why the thirty-day
 *    acceptance criterion exercises the rollups rather than the raw table: a wide window
 *    asks for wide buckets, and the compiler answers those from the pre-aggregate.
 */
export function forWindow(query: Query, from: Date, to: Date): Query {
  const seconds = bucketSeconds(from.getTime(), to.getTime());

  return {
    ...query,
    time: { start: from.toISOString(), end: to.toISOString() },
    ...(query.group_by
      ? {
          group_by: query.group_by.map((field) =>
            field.field === "time_bucket" ? { field: "time_bucket" as const, seconds } : field,
          ),
        }
      : {}),
  };
}

/** One line on a chart. */
export interface Series {
  /** The labels the query grouped by, joined — or the aggregate's name when there are none. */
  name: string;
  points: { at: number; value: number }[];
}

/**
 * Read a grouped result set as lines.
 *
 * The column layout is the compiler's: `group_by` columns in order, then the aggregations.
 * A time-series panel groups by `time_bucket` and possibly by other things, so column 0
 * is the instant, the last column is the value, and anything between names the line.
 *
 * Rows whose value is null are dropped rather than drawn as zero — `avg()` over a bucket
 * with no samples is null, and a chart that drew it as zero would show an outage as a
 * collapse to the floor, which is a different and much more alarming picture.
 */
export function toSeries(result: ResultSet): Series[] {
  const width = result.columns.length;
  if (width < 2) return [];

  const lines = new Map<string, Series>();

  for (const row of result.rows) {
    const at = Date.parse(String(row[0]).replace(" ", "T") + "Z");
    const raw = row[width - 1];
    const value = typeof raw === "number" ? raw : Number(raw);
    if (Number.isNaN(at) || raw === null || raw === undefined || Number.isNaN(value)) {
      continue;
    }

    const name =
      width === 2
        ? (result.columns[width - 1]?.name ?? "value")
        : row
            .slice(1, width - 1)
            .map((cell) => String(cell ?? ""))
            .join(" · ");

    const line = lines.get(name) ?? { name, points: [] };
    line.points.push({ at, value });
    lines.set(name, line);
  }

  // Sorted by time within each line: the compiler's ORDER BY is the panel's, and a panel
  // that did not ask for one would otherwise draw a polyline that doubles back on itself.
  for (const line of lines.values()) {
    line.points.sort((a, b) => a.at - b.at);
  }

  return [...lines.values()];
}

/**
 * The one number a stat or a gauge shows.
 *
 * The **last** row's aggregate, which for a query grouped by time is the most recent
 * bucket — "what is it now", which is what somebody reads a stat panel for. A mean across
 * the window would be a different question, and one the panel's title would not match.
 *
 * `null` when there is nothing to show, which the panel renders as "no data" rather than
 * as zero: those are different facts and only one of them is reassuring.
 */
export function latest(result: ResultSet): number | null {
  const width = result.columns.length;
  if (width === 0) return null;

  for (let i = result.rows.length - 1; i >= 0; i -= 1) {
    const raw = result.rows[i]?.[width - 1];
    if (raw === null || raw === undefined) continue;
    const value = typeof raw === "number" ? raw : Number(raw);
    if (!Number.isNaN(value)) return value;
  }
  return null;
}

/** A blank panel of a kind, for the editor. */
export function blank(kind: Viz["kind"], id: string, query?: Query): Panel {
  const viz: Viz =
    kind === "gauge"
      ? { kind, min: 0, max: 100 }
      : kind === "stat"
        ? { kind, decimals: 2 }
        : kind === "time_series"
          ? { kind }
          : kind === "table"
            ? { kind }
            : { kind: "alerts" };

  return {
    id,
    title: "",
    ...(kind === "alerts" ? {} : { query }),
    viz,
    width: kind === "alerts" ? 12 : 6,
    height: 2,
  };
}
