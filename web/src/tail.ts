/**
 * The live tail: a search that keeps arriving.
 *
 * The server does the hard half. `POST /api/v1/query/tail` takes the same AST the
 * Explorer already builds plus a watermark, and answers with the rows that reached
 * storage in `[since, now)` — half-open, on `ingested_at` — and the watermark to send
 * next time. Consecutive polls therefore partition the rows: every row appears once,
 * none appears twice, and a row whose ingest lagged its own timestamp still arrives.
 *
 * So there is no window arithmetic in this file, deliberately. A browser clock three
 * minutes fast would otherwise ask for a window that has not happened yet and show an
 * empty tail on a busy estate, with nothing on screen to explain it. The client's only
 * job is to send back what the server last told it.
 *
 * What is left here is the buffer, and it is the part that can be quietly wrong — see
 * `merge`.
 */

import { request } from "./api";
import type { Query, QueryWarning, ResultSet } from "./query";

/** One poll's worth of rows, and what to ask for next. */
export interface TailPage extends ResultSet {
  /** The server's own `now`, echoed back on the next poll. Never a client clock. */
  next_since: string;
  /**
   * False when the poll filled its row limit: ingest is arriving faster than the tail
   * can show it, and part of that window was left behind. Shown rather than swallowed —
   * a tail that silently drops rows while claiming to be live is worse than one that
   * says it is behind.
   */
  complete: boolean;
  /**
   * True when the watermark was older than the server will look back — a laptop that
   * slept, a tab left open overnight. The rows in between are still in storage and a
   * search finds them; the tail cannot, and says so instead of resuming as though
   * nothing were missing.
   */
  skipped: boolean;
}

/**
 * How often to poll.
 *
 * Two seconds is a compromise between looking live and the cost of the query underneath
 * it. Every poll is a real `ClickHouse` read — the projection makes it a cheap one, but
 * cheap is not free, and a 250 ms tail on a hundred open tabs is a self-inflicted load
 * test against the customer's own cluster.
 */
export const POLL_MS = 2_000;

/**
 * How many rows the view keeps.
 *
 * A tail left running overnight would otherwise hold every row it ever saw in the DOM,
 * and the tab that was meant to show an incident is the one that has stopped responding
 * by the time somebody looks at it. Older rows leave the view; they are still in storage,
 * and the search this tail was started from still finds them.
 */
export const BUFFER_ROWS = 1_000;

export function pollTail(
  tenant: string,
  query: Query,
  since: string | null,
  signal?: AbortSignal,
) {
  return request<TailPage>("/api/v1/query/tail", {
    method: "POST",
    body: since === null ? { query } : { query, since },
    tenant,
    ...(signal ? { signal } : {}),
  });
}

/**
 * Add a poll's rows to what is already on screen.
 *
 * Newest first, because that is the order the tail delivers in and the order a tail is
 * read in: the line that just arrived belongs at the top, where the eye already is.
 *
 * The counters accumulate rather than being replaced. "This tail has read 4.2M rows" is
 * the number that explains a cluster's load at the end of an afternoon; the last poll's
 * few hundred explains nothing, and would tick back to almost zero every two seconds
 * while the cost kept climbing.
 *
 * Warnings are the newest poll's and are not accumulated — a warning is a statement
 * about a query, and every poll runs the same one, so keeping them all would print the
 * same sentence a thousand times.
 */
export function merge(
  previous: ResultSet | null,
  page: TailPage,
  cap = BUFFER_ROWS,
): ResultSet {
  // An empty poll carries no columns — ClickHouse sends no `meta` for an empty result —
  // so the columns already on screen are kept. Taking the page's would blank the table
  // the first time nothing arrived for two seconds, which on a quiet estate is most of
  // the time.
  const columns = page.columns.length > 0 ? page.columns : (previous?.columns ?? []);

  return {
    columns,
    rows: [...page.rows, ...(previous?.rows ?? [])].slice(0, cap),
    table: page.table || (previous?.table ?? ""),
    warnings: page.warnings as QueryWarning[],
    rows_read: (previous?.rows_read ?? 0) + page.rows_read,
    bytes_read: (previous?.bytes_read ?? 0) + page.bytes_read,
  };
}
