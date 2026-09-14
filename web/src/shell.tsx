/**
 * The two pieces of state every view shares: which tenant, and what time range.
 *
 * Both live in the URL, not in React state and not in storage. That is the decision this
 * file exists to make, and it has consequences everywhere:
 *
 * * A link reproduces a view. "Look at this" in a chat window is the most common thing
 *   an operator does during an incident, and a URL that does not carry the time range is
 *   a URL that shows the recipient something else.
 * * Back and forward work. Widening a window and going back narrows it again, because
 *   the browser already knows how to do that and we did not reimplement it.
 * * Reloading during an incident does not reset you to "last 15 minutes".
 * * Two tabs can be two tenants. An MSP engineer comparing two customers is not an edge
 *   case, and a tenant kept in module state or storage makes it impossible.
 *
 * The cost is that every navigation must carry the search params forward, which
 * TanStack Router does for us, and that the range is parsed from strings on every read.
 */

import { createContext, useCallback, useContext, useMemo } from "react";
import { useNavigate, useSearch } from "@tanstack/react-router";

import type { Me, TenantMembership } from "./api";

/**
 * A time range, as it appears in the URL.
 *
 * Relative ranges stay relative — `from=now-1h` re-evaluates on every read, so a
 * dashboard left open overnight still shows the last hour rather than the hour it was
 * opened in. An absolute range is two ISO instants and never moves, which is what you
 * want once you are looking at a specific incident.
 */
export interface TimeRange {
  from: string;
  to: string;
}

export const DEFAULT_RANGE: TimeRange = { from: "now-1h", to: "now" };

export const PRESETS: { label: string; range: TimeRange }[] = [
  { label: "15m", range: { from: "now-15m", to: "now" } },
  { label: "1h", range: { from: "now-1h", to: "now" } },
  { label: "6h", range: { from: "now-6h", to: "now" } },
  { label: "24h", range: { from: "now-24h", to: "now" } },
  { label: "7d", range: { from: "now-7d", to: "now" } },
];

const RELATIVE = /^now(?:-(\d+)([smhdw]))?$/;

/**
 * A date, or a date and time, with an optional offset. Deliberately narrower than what
 * `Date.parse` will take — see `resolveInstant`.
 */
const ISO_8601 =
  /^\d{4}-\d{2}-\d{2}(?:[T ]\d{2}:\d{2}(?::\d{2}(?:\.\d{1,9})?)?(?:Z|[+-]\d{2}:?\d{2})?)?$/;
const UNIT_MS: Record<string, number> = {
  s: 1000,
  m: 60_000,
  h: 3_600_000,
  d: 86_400_000,
  w: 604_800_000,
};

/**
 * Resolve one endpoint to an instant, against a clock passed in.
 *
 * `now` is a parameter rather than a call to `Date.now()` so that both ends of a range
 * resolve against the same instant. Reading the clock twice can produce `from` after
 * `to` at a millisecond boundary, which is a query that returns nothing and a bug report
 * nobody can reproduce.
 */
export function resolveInstant(value: string, now: number): Date | null {
  const relative = RELATIVE.exec(value);
  if (relative) {
    const [, amount, unit] = relative;
    if (!amount || !unit) return new Date(now);
    const ms = UNIT_MS[unit];
    if (ms === undefined) return null;
    return new Date(now - Number(amount) * ms);
  }

  // Checked against ISO-8601 before Date.parse, because Date.parse is allowed to fall
  // back to implementation-defined parsing and V8's fallback accepts almost anything:
  // `Date.parse("now-1")` is 2000-12-31, not NaN. A typo in the address bar would
  // otherwise query the year 2000, return nothing, and look exactly like an outage.
  if (!ISO_8601.test(value)) return null;

  const parsed = Date.parse(value);
  return Number.isNaN(parsed) ? null : new Date(parsed);
}

/** A range as two absolute instants, or null if either end is unreadable. */
export function resolveRange(range: TimeRange, now = Date.now()): { from: Date; to: Date } | null {
  const from = resolveInstant(range.from, now);
  const to = resolveInstant(range.to, now);
  if (!from || !to) return null;
  return { from, to };
}

/** For the picker's label. `now-1h` reads better than the two instants it means. */
export function describeRange(range: TimeRange): string {
  const preset = PRESETS.find((p) => p.range.from === range.from && p.range.to === range.to);
  if (preset) return `Last ${preset.label}`;

  const resolved = resolveRange(range);
  if (!resolved) return "Invalid range";

  const fmt = (d: Date) => d.toISOString().slice(0, 16).replace("T", " ");
  return `${fmt(resolved.from)} → ${fmt(resolved.to)}`;
}

/** The search params every route carries. */
export interface ShellSearch {
  tenant?: string;
  from?: string;
  to?: string;
}

/**
 * Validate the shell's search params.
 *
 * Unknown values are dropped rather than rejected: a URL is something people edit and
 * paste, and a whole page that refuses to render because one parameter is misspelled is
 * worse than a page that renders with a default. An unreadable range falls back to the
 * default and the picker shows what it actually used.
 */
export function validateShellSearch(search: Record<string, unknown>): ShellSearch {
  const out: ShellSearch = {};
  if (typeof search.tenant === "string" && search.tenant) out.tenant = search.tenant;
  if (typeof search.from === "string" && search.from) out.from = search.from;
  if (typeof search.to === "string" && search.to) out.to = search.to;
  return out;
}

interface ShellValue {
  me: Me;
  tenant: TenantMembership;
  setTenant: (tenantId: string) => void;
  range: TimeRange;
  setRange: (range: TimeRange) => void;
}

const ShellContext = createContext<ShellValue | null>(null);

export function ShellProvider({ me, children }: { me: Me; children: React.ReactNode }) {
  const search = useSearch({ strict: false }) as ShellSearch;
  const navigate = useNavigate();

  // A tenant in the URL that this user cannot reach resolves to their first one rather
  // than to an error. The server would answer 404 for it anyway — a tenant you cannot
  // see does not exist — so the honest UI is the one that shows you what you *can* see.
  const tenant = useMemo(() => {
    const named = me.tenants.find((t) => t.tenant_id === search.tenant);
    return named ?? me.tenants[0];
  }, [me.tenants, search.tenant]);

  const range = useMemo<TimeRange>(() => {
    const candidate = { from: search.from ?? DEFAULT_RANGE.from, to: search.to ?? DEFAULT_RANGE.to };
    return resolveRange(candidate) ? candidate : DEFAULT_RANGE;
  }, [search.from, search.to]);

  const setTenant = useCallback(
    (tenantId: string) => {
      // The range is deliberately kept across a tenant switch. An MSP engineer
      // comparing the same incident window across two customers is the reason the
      // switcher exists at all.
      void navigate({ to: ".", search: (old: ShellSearch) => ({ ...old, tenant: tenantId }) });
    },
    [navigate],
  );

  const setRange = useCallback(
    (next: TimeRange) => {
      void navigate({ to: ".", search: (old: ShellSearch) => ({ ...old, ...next }) });
    },
    [navigate],
  );

  if (!tenant) {
    // An authenticated account with no role on any tenant. Rare, and a real state: a
    // user whose last role was revoked. Saying so beats rendering an empty shell.
    return (
      <div className="empty-state">
        <h1>No tenants</h1>
        <p>
          {me.email} has no role on any tenant. An administrator has to grant one before
          there is anything here to see.
        </p>
      </div>
    );
  }

  const value: ShellValue = { me, tenant, setTenant, range, setRange };
  return <ShellContext.Provider value={value}>{children}</ShellContext.Provider>;
}

export function useShell(): ShellValue {
  const value = useContext(ShellContext);
  if (!value) throw new Error("useShell outside ShellProvider");
  return value;
}
