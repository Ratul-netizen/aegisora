/**
 * Saved searches: a question somebody wants to ask again.
 *
 * The server stores a `Query` AST and hands it back unchanged — SPEC §M3, and the reason
 * an M4 alert rule built from one is a copy rather than a translation. So this file does
 * not define a "saved search format". There is the AST, and there is a name on it.
 *
 * Two things happen when one is opened, and they are deliberately separate:
 *
 * 1. **It runs the AST that was saved**, with the time window replaced by the one in the
 *    header. Running a rebuilt approximation of it would mean the rows on screen are not
 *    the search the operator asked for.
 * 2. **The controls are set from the AST**, so the search can be read and edited. That
 *    direction is lossy — the AST can express filters this form cannot — which is why
 *    `toControls` says when it could not show everything, and why point 1 does not
 *    depend on point 2.
 */

import { request } from "./api";
import {
  SEVERITIES,
  type Expr,
  type Field,
  type Query,
  type Severity,
  type Signal,
  type TextMode,
} from "./query";

export interface SavedSearch {
  id: string;
  name: string;
  description: string;
  signal: Signal;
  /** The AST, exactly as `POST /api/v1/query` takes it. */
  query: Query;
  created_at: string;
  updated_at: string;
}

export interface SearchRequest {
  name: string;
  description?: string;
  query: Query;
}

export function listSearches(tenant: string) {
  return request<SavedSearch[]>("/api/v1/searches", { tenant });
}

export function saveSearch(tenant: string, body: SearchRequest) {
  return request<SavedSearch>("/api/v1/searches", {
    method: "POST",
    body,
    tenant,
  });
}

export function replaceSearch(tenant: string, id: string, body: SearchRequest) {
  return request<SavedSearch>(`/api/v1/searches/${id}`, {
    method: "PUT",
    body,
    tenant,
  });
}

export function removeSearch(tenant: string, id: string) {
  return request<void>(`/api/v1/searches/${id}`, {
    method: "DELETE",
    tenant,
  });
}

/**
 * The saved query, over the window the operator is looking at now.
 *
 * The stored window is the one it happened to be saved over — the AST has no relative
 * windows, by design, so that a compiled query is reproducible after the fact. It is
 * provenance, not a parameter: a saved search that reopened showing the same forty rows
 * forever would be a screenshot rather than a question.
 */
export function overWindow(query: Query, from: Date, to: Date): Query {
  return {
    ...query,
    time: { start: from.toISOString(), end: to.toISOString() },
  };
}

/** What the Explorer's controls hold. */
export interface Controls {
  signal: Signal;
  search: string;
  mode: TextMode;
  severity: Severity | "";
  limit: number;
  picked: { field: Field; value: string }[];
  /**
   * Whether the controls can show the whole filter.
   *
   * False when the AST contains something this form cannot express — an `or`, a `not`, a
   * comparison the sidebar never produces. The rows on screen are still exactly what was
   * saved, because opening runs the AST; what this flag changes is whether pressing Run
   * afterwards would quietly replace that filter with a weaker one. The UI says so
   * instead of letting it happen silently.
   */
  exact: boolean;
}

/**
 * Read the Explorer's controls back out of a query.
 *
 * The inverse of `buildFilter` plus the picked chips, and only of those: this recognises
 * the shapes the form produces and gives up honestly on anything else. Written as a
 * recognition pass rather than a general interpreter because a general one would have to
 * decide what `body contains X OR severity >= error` looks like as a text box and a
 * dropdown, and every answer to that is a lie about what will run.
 */
export function toControls(query: Query): Controls {
  const controls: Controls = {
    signal: query.signal,
    search: "",
    mode: "any_token",
    severity: "",
    limit: query.limit,
    picked: [],
    exact: true,
  };

  // A single clause is an `and` of one. Flattening here keeps the recognition below from
  // being written twice.
  const clauses: Expr[] =
    query.filter === undefined
      ? []
      : query.filter.op === "and"
        ? query.filter.of
        : [query.filter];

  for (const clause of clauses) {
    if (clause.op === "text" && !controls.search) {
      controls.mode = clause.mode;
      // The token modes split on whitespace; substring and phrase keep one term, so
      // joining is the exact inverse in both cases.
      controls.search = clause.terms.join(" ");
      continue;
    }

    if (clause.op === "compare" && clause.field.field === "severity" && clause.cmp === "in") {
      const floor = severityFloor(clause.value);
      if (floor) {
        controls.severity = floor;
        continue;
      }
    }

    if (clause.op === "compare" && clause.cmp === "eq" && typeof clause.value === "string") {
      controls.picked.push({ field: clause.field, value: clause.value });
      continue;
    }

    controls.exact = false;
  }

  return controls;
}

/**
 * The floor of an "at least this severe" list, if that is what it is.
 *
 * `atLeast` produces a suffix of `SEVERITIES`, so anything else — a hand-built set, or a
 * list from a future UI that allows picking severities individually — is deliberately
 * not recognised. Showing `warn` for a filter that actually means "warn or emergency but
 * not error" would be the worst kind of wrong: readable, plausible, and not what runs.
 */
function severityFloor(value: unknown): Severity | null {
  if (!Array.isArray(value) || value.length === 0) return null;

  const first = SEVERITIES.indexOf(value[0] as Severity);
  if (first < 0) return null;

  const expected = SEVERITIES.slice(first);
  if (expected.length !== value.length) return null;
  if (expected.some((s, i) => s !== value[i])) return null;

  // `first` came from `indexOf` on the same array, so this is a real element — but the
  // compiler cannot know that under noUncheckedIndexedAccess, and an assertion would be
  // a claim it cannot check.
  return expected[0] ?? null;
}
