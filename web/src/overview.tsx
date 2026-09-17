/**
 * The Operations Overview — UI-SPEC §8, `UI.md` §3.
 *
 * The landing page, and the one screen somebody sees before they have configured
 * anything. `UI.md`'s argument for it: *"a product whose first screen is empty has to be
 * learned before it can be judged."*
 *
 * # Six panels, four telemetry queries and one control-plane read
 *
 * Every number on it is sourced in the spec's table, and each panel is its own request —
 * one slow panel spins alone, one failing panel says so in its own corner. Well inside
 * the twenty-panel budget measured at p95 0.42 s.
 *
 * # Three things it deliberately does not draw
 *
 * **A health percentage.** `UI.md` §3's mock shows a 97.4% ring. There is no defensible
 * formula for it yet — availability with maintenance windows excluded is M9 — and a
 * number nobody can explain is worse than an absent one on the screen an operator trusts
 * first.
 *
 * **A topology panel.** Until M6 it would be a box saying "coming soon", which is the
 * product telling the operator it is unfinished every time they open it.
 *
 * **A zero in a red tile.** Nothing firing is "Nothing is firing", not a `0` drawn in the
 * colour of danger.
 */

import { useQuery } from "@tanstack/react-query";
import { Link } from "@tanstack/react-router";
import { useMemo } from "react";

import { ago, listAlerts, order, type Alert } from "./alerting";
import { api } from "./api";
import { message, runQuery, type Query, type ResultSet } from "./query";
import { resolveRange, useShell } from "./shell";

/** How often the control-plane panels re-read — UI-SPEC §5. */
const REFRESH_MS = 10_000;

/** Severities that count as "an error somebody should look at". */
const BAD = ["error", "critical", "alert", "emergency"];

/**
 * The severities the log-volume chart stacks, in the order they stack.
 *
 * Quiet at the bottom, loud at the top, so the shape of the bar reads as "how much of
 * this is bad" without anybody consulting a legend.
 */
const STACK: { severity: string; colour: string }[] = [
  { severity: "debug", colour: "var(--unknown)" },
  { severity: "info", colour: "var(--series-1)" },
  { severity: "notice", colour: "var(--series-6)" },
  { severity: "warn", colour: "var(--warn)" },
  { severity: "error", colour: "var(--danger)" },
  { severity: "critical", colour: "var(--maintenance)" },
];

export function OverviewPage() {
  const { tenant, range } = useShell();

  // Memoised on the *descriptor* rather than computed inline, and this is not a tidying:
  // `resolveRange` turns "last 1 hour" into two instants ending at `now`, so calling it
  // during render produces a different window every time the component renders. Every
  // panel's query key is derived from that window, so a fresh window is a fresh key, a
  // fresh fetch, a re-render — and the page fetches in a loop until somebody navigates
  // away. It renders as three panels stuck on their loading state, forever, while the
  // server takes three queries a frame.
  //
  // Found by pointing a browser at it. No test would have: each query is correct, each
  // one returns 200, and the only symptom is the count of them.
  const { from, to } = useMemo(() => {
    const resolved = resolveRange(range);
    return { from: resolved?.from, to: resolved?.to };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [range.from, range.to]);

  // ---- what exists, and what is wrong with it -------------------------------
  const resources = useQuery({
    queryKey: ["overview-resources", tenant.tenant_id],
    queryFn: () => api.resources(tenant.tenant_id),
    retry: false,
  });

  const alerts = useQuery({
    queryKey: ["alerts", tenant.tenant_id],
    queryFn: () => listAlerts(tenant.tenant_id),
    refetchInterval: REFRESH_MS,
    retry: false,
  });

  // ---- what is still talking ------------------------------------------------
  //
  // One row per resource that produced a metric in the window. Counting the rows is the
  // honest "how many are reporting" this product can answer today — and it is not called
  // availability, which implies an SLA calculation with maintenance excluded.
  const reporting = usePanel(
    ["overview-reporting", tenant.tenant_id],
    from && to
      ? {
          signal: "metric",
          time: { start: from.toISOString(), end: to.toISOString() },
          resources: { type: "all" },
          aggregations: [{ func: "count", alias: "n" }],
          group_by: [{ field: "resource_id" }],
          limit: 10_000,
        }
      : null,
  );

  // ---- how much is arriving, and how much of it is bad ----------------------
  const volume = usePanel(
    ["overview-volume", tenant.tenant_id],
    from && to
      ? {
          signal: "log",
          time: { start: from.toISOString(), end: to.toISOString() },
          resources: { type: "all" },
          aggregations: [{ func: "count", alias: "n" }],
          group_by: [
            { field: "time_bucket", seconds: bucketFor(from, to) },
            { field: "severity" },
          ],
          limit: 1000,
        }
      : null,
  );

  // ---- who is producing the errors -----------------------------------------
  //
  // Grouped on `host.name`, which is a materialised column in `logs` — W1 measured
  // grouping on a Map key at 2 252 ms and it is the slowest thing in the whole suite.
  // It also means the name comes back with the count, rather than forty requests to
  // resolve forty ids.
  const busiest = usePanel(
    ["overview-busiest", tenant.tenant_id],
    from && to
      ? {
          signal: "log",
          time: { start: from.toISOString(), end: to.toISOString() },
          resources: { type: "all" },
          filter: {
            op: "compare",
            field: { field: "severity" },
            cmp: "in",
            value: BAD,
          },
          aggregations: [{ func: "count", alias: "n" }],
          group_by: [{ field: "attr", key: "host.name" }],
          order_by: [{ key: { by: "alias", alias: "n" }, desc: true }],
          limit: 8,
        }
      : null,
  );

  const rows = order(alerts.data ?? []);
  const firing = rows.filter((a) => a.state === "firing");
  const pending = rows.filter((a) => a.state === "pending");
  const total = resources.data?.items.length ?? null;
  const talking = reporting.data?.rows.length ?? null;

  return (
    <>
      <h1>Operations overview</h1>
      <p className="dim">
        {tenant.name} · all sites · the window is the one in the header.
      </p>

      <div className="tiles">
        <Tile label="Resources" value={total} href="/resources" />
        <Tile
          label="Firing"
          value={firing.length}
          tone={firing.length > 0 ? "danger" : undefined}
          href="/alerts"
        />
        <Tile
          label="Pending"
          value={pending.length}
          tone={pending.length > 0 ? "warn" : undefined}
          note="nobody notified"
          href="/alerts"
        />
        <Tile
          label="Reporting"
          value={talking}
          of={total}
          // Not "availability": that implies an SLA calculation with maintenance
          // windows excluded, which is M9.
          note="sent telemetry in this window"
        />
      </div>

      <section className="panel">
        <header>
          <h3>What is firing</h3>
          <Link to="/alerts">All alerts</Link>
        </header>
        {alerts.isError ? (
          <p className="warn">{message(alerts.error)}</p>
        ) : rows.length === 0 ? (
          <p className="dim">Nothing is firing.</p>
        ) : (
          <ul className="panel-alerts">
            {rows.slice(0, 6).map((alert: Alert) => (
              <li key={alert.id}>
                <span className={`severity ${alert.severity}`}>{alert.severity}</span>
                <Link to="/resources/$id" params={{ id: alert.resource_id }}>
                  {alert.resource}
                </Link>
                <span className="dim">{alert.rule}</span>
                <span className="dim">
                  {alert.state === "pending" ? "pending · " : ""}
                  {ago(alert.since)}
                </span>
              </li>
            ))}
          </ul>
        )}
      </section>

      <div className="grid">
        <section className="panel" style={{ gridColumn: "span 7", minHeight: "220px" }}>
          <header>
            <h3>Log volume by severity</h3>
            <Link to="/explore">Explore</Link>
          </header>
          <PanelState panel={volume}>
            {(result) => <Volume result={result} />}
          </PanelState>
        </section>

        <section className="panel" style={{ gridColumn: "span 5", minHeight: "220px" }}>
          <header>
            <h3>Busiest resources</h3>
            <span className="dim">errors and worse</span>
          </header>
          <PanelState panel={busiest}>
            {(result) => <Busiest result={result} />}
          </PanelState>
        </section>
      </div>
    </>
  );
}

/** One telemetry panel's query, with the five states the widget contract requires. */
function usePanel(key: unknown[], query: Query | null) {
  return useQuery({
    queryKey: [...key, JSON.stringify(query)],
    queryFn: () => runQuery(String(key[1]), query as Query),
    enabled: query !== null,
    retry: false,
    staleTime: 60_000,
  });
}

/**
 * Loading, error, empty and data — the four a panel can be in before it has drawn
 * anything. Written once so every panel on this page behaves the same way, which is the
 * whole point of the widget contract having states at all.
 */
function PanelState({
  panel,
  children,
}: {
  panel: ReturnType<typeof usePanel>;
  children: (result: ResultSet) => React.ReactNode;
}) {
  if (panel.isPending) return <p className="dim">…</p>;
  // The server's own sentence, in this panel's corner, with the rest of the page alive.
  if (panel.isError) return <p className="warn">{message(panel.error)}</p>;
  if (panel.data.rows.length === 0) return <p className="dim">No data in this window.</p>;
  return <>{children(panel.data)}</>;
}

/** A headline number. Never a zero in the colour of danger — see the module docs. */
function Tile({
  label,
  value,
  of,
  note,
  tone,
  href,
}: {
  label: string;
  value: number | null;
  of?: number | null | undefined;
  note?: string | undefined;
  // `| undefined` spelled out: this project has exactOptionalPropertyTypes on, and a
  // tile with no tone genuinely passes the key with nothing in it.
  tone?: "danger" | "warn" | undefined;
  href?: "/resources" | "/alerts" | undefined;
}) {
  const body = (
    <>
      <span className="tile-label">{label}</span>
      <span className={`tile-value${tone && value ? ` ${tone}` : ""}`}>
        {value === null ? "—" : value.toLocaleString()}
        {of != null && <span className="tile-of"> / {of.toLocaleString()}</span>}
      </span>
      {note && <span className="tile-note">{note}</span>}
    </>
  );

  return href ? (
    <Link className="tile" to={href}>
      {body}
    </Link>
  ) : (
    <div className="tile">{body}</div>
  );
}

/**
 * Log volume, stacked by severity.
 *
 * Bars rather than a line: a count over a bucket is a quantity in a period, and a line
 * between two counts implies values in between that were never measured. The same
 * argument the Explorer's histogram makes.
 */
function Volume({ result }: { result: ResultSet }) {
  // [bucket, severity, n] — group_by order, then the aggregate.
  const buckets = new Map<string, Map<string, number>>();
  for (const row of result.rows) {
    const at = String(row[0]);
    const severity = String(row[1]);
    const n = Number(row[2]) || 0;
    const bucket = buckets.get(at) ?? new Map<string, number>();
    bucket.set(severity, (bucket.get(severity) ?? 0) + n);
    buckets.set(at, bucket);
  }

  const ordered = [...buckets.entries()].sort((a, b) => a[0].localeCompare(b[0]));
  if (ordered.length === 0) return <p className="dim">No data in this window.</p>;

  const totals = ordered.map(([, bucket]) => [...bucket.values()].reduce((a, b) => a + b, 0));
  const tallest = Math.max(...totals, 1);
  const width = 600;
  const height = 150;
  const gap = 1;
  const barWidth = Math.max(1, width / ordered.length - gap);

  return (
    <figure className="chart">
      <svg viewBox={`0 0 ${width} ${height}`} role="img" preserveAspectRatio="none">
        {ordered.map(([at, bucket], i) => {
          let y = height;
          return (
            <g key={at}>
              {STACK.map(({ severity, colour }) => {
                const n = bucket.get(severity) ?? 0;
                if (n === 0) return null;
                const h = (n / tallest) * (height - 4);
                y -= h;
                return (
                  <rect
                    key={severity}
                    x={i * (barWidth + gap)}
                    y={y}
                    width={barWidth}
                    height={h}
                    fill={colour}
                  >
                    <title>{`${severity}: ${n.toLocaleString()}`}</title>
                  </rect>
                );
              })}
            </g>
          );
        })}
      </svg>
      <figcaption className="legend">
        {STACK.map(({ severity, colour }) => (
          <span key={severity}>
            <i style={{ background: colour }} />
            {severity}
          </span>
        ))}
      </figcaption>
    </figure>
  );
}

/** Who is producing the errors, by host name. */
function Busiest({ result }: { result: ResultSet }) {
  const rows = result.rows
    .map((row) => ({ host: String(row[0] ?? ""), n: Number(row[1]) || 0 }))
    .filter((row) => row.host !== "");
  if (rows.length === 0) return <p className="dim">No errors in this window.</p>;

  const worst = Math.max(...rows.map((r) => r.n), 1);

  return (
    <ul className="ranked">
      {rows.map((row) => (
        // A bar as well as a number: "4 200 and 3 900" is two numbers, and two bars of
        // almost the same length is a fact. Drawn as the row's own background — see the
        // note in styles.css on why a child element cannot do it.
        <li
          key={row.host}
          style={{ ["--fill" as string]: `${(row.n / worst) * 100}%` }}
        >
          <span className="ranked-name mono">{row.host}</span>
          <span className="ranked-count">{row.n.toLocaleString()}</span>
        </li>
      ))}
    </ul>
  );
}

/**
 * A bucket width that puts roughly sixty bars in the window.
 *
 * The same arithmetic the Explorer uses, and for the same reason: a bar has to be a span
 * of time somebody can name.
 */
function bucketFor(from: Date, to: Date): number {
  const span = Math.max(1, Math.round((to.getTime() - from.getTime()) / 1000));
  const steps = [60, 300, 900, 1800, 3600, 10800, 21600, 43200, 86400];
  return steps.find((s) => s >= span / 60) ?? 86400;
}
