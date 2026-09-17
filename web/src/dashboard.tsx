/**
 * A dashboard: the list of them, and one of them on screen.
 *
 * # Every panel is its own query
 *
 * Twenty panels are twenty requests, in parallel, each cached by react-query under its own
 * key. The alternative — one batched request that returns twenty result sets — would be
 * faster on paper and worse in every other way: one slow panel would hold the other
 * nineteen, a failing panel would fail the page, and the server would grow a second query
 * endpoint whose semantics are "these twenty, atomically" for no caller that wants that.
 *
 * SPEC's criterion is p95 under three seconds for twenty panels over thirty days, and it
 * is met by the panels being independent rather than by them being bundled.
 *
 * # Panels are added from saved searches
 *
 * The same argument the rule editor makes: the query half of a panel already has a place
 * where it is built by somebody who can see the rows it returns. A second query builder
 * here would produce panels whose authors never saw what they matched.
 */

import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Link } from "@tanstack/react-router";
import { useState } from "react";

import { listAlerts } from "./alerting";
import {
  ROW_HEIGHT,
  blank,
  createDashboard,
  deleteDashboard,
  forWindow,
  getDashboard,
  latest,
  listDashboards,
  replaceDashboard,
  type Dashboard,
  type Panel,
  type Viz,
} from "./dashboards";
import { Alerts, Gauge, Stat, Table, TimeSeries, nameOf } from "./panels";
import { message, runQuery, type ResultSet } from "./query";
import { listSearches } from "./searches";
import { resolveRange, useShell } from "./shell";

const KINDS: Viz["kind"][] = ["time_series", "stat", "table", "gauge", "alerts"];

function mayWrite(role: string): boolean {
  return role === "operator" || role === "admin";
}

export function DashboardsPage() {
  const { tenant } = useShell();
  const client = useQueryClient();
  const [name, setName] = useState("");
  const [problem, setProblem] = useState<string | null>(null);

  const dashboards = useQuery({
    queryKey: ["dashboards", tenant.tenant_id],
    queryFn: () => listDashboards(tenant.tenant_id),
    retry: false,
  });

  const create = useMutation({
    mutationFn: () =>
      createDashboard(tenant.tenant_id, { name: name.trim(), panels: [] }),
    onSuccess: async () => {
      setName("");
      setProblem(null);
      await client.invalidateQueries({ queryKey: ["dashboards", tenant.tenant_id] });
    },
    onError: (error) => setProblem(message(error)),
  });

  const all = dashboards.data ?? [];

  return (
    <>
      <h1>Dashboards</h1>

      {mayWrite(tenant.role) && (
        <form
          className="explore-form"
          onSubmit={(e) => {
            e.preventDefault();
            create.mutate();
          }}
        >
          <label className="grow">
            New dashboard
            <input
              type="text"
              value={name}
              placeholder="Core routers"
              onChange={(e) => setName(e.target.value)}
            />
          </label>
          <button
            type="submit"
            className="primary"
            disabled={name.trim() === "" || create.isPending}
          >
            {create.isPending ? "Creating…" : "Create"}
          </button>
          {problem && (
            <span className="problem-inline" role="alert">
              {problem}
            </span>
          )}
        </form>
      )}

      {dashboards.isError && (
        <div className="problem" role="alert">
          {message(dashboards.error)}
        </div>
      )}

      {all.length === 0 ? (
        <p className="dim">None yet. A dashboard is a name and a list of panels.</p>
      ) : (
        <ul className="cards">
          {all.map((dashboard: Dashboard) => (
            <li key={dashboard.id}>
              <Link to="/dashboards/$id" params={{ id: dashboard.id }}>
                <strong>{dashboard.name}</strong>
              </Link>
              <p className="dim">
                {dashboard.panels.length} panel
                {dashboard.panels.length === 1 ? "" : "s"}
                {dashboard.description && ` · ${dashboard.description}`}
              </p>
            </li>
          ))}
        </ul>
      )}
    </>
  );
}

export function DashboardPage({ id }: { id: string }) {
  const { tenant, range } = useShell();
  const client = useQueryClient();
  const [problem, setProblem] = useState<string | null>(null);

  const dashboard = useQuery({
    queryKey: ["dashboard", tenant.tenant_id, id],
    queryFn: () => getDashboard(tenant.tenant_id, id),
    retry: false,
  });

  const save = useMutation({
    mutationFn: (panels: Panel[]) =>
      replaceDashboard(tenant.tenant_id, id, {
        name: dashboard.data?.name ?? "",
        description: dashboard.data?.description ?? "",
        panels,
      }),
    onSuccess: async () => {
      setProblem(null);
      await client.invalidateQueries({ queryKey: ["dashboard", tenant.tenant_id, id] });
    },
    onError: (error) => setProblem(message(error)),
  });

  const remove = useMutation({
    mutationFn: () => deleteDashboard(tenant.tenant_id, id),
    onError: (error) => setProblem(message(error)),
  });

  const resolved = resolveRange(range);
  const panels = dashboard.data?.panels ?? [];

  if (dashboard.isError) {
    return (
      <div className="problem" role="alert">
        {message(dashboard.error)}
      </div>
    );
  }

  return (
    <>
      <h1>{dashboard.data?.name ?? "Dashboard"}</h1>
      <p className="dim">
        <Link to="/dashboards">All dashboards</Link>
        {dashboard.data?.description && ` · ${dashboard.data.description}`}
        {" · the window is the one in the header"}
      </p>

      {problem && (
        <div className="problem" role="alert">
          {problem}
        </div>
      )}

      {mayWrite(tenant.role) && dashboard.data && (
        <AddPanel
          onAdd={(panel) => save.mutate([...panels, panel])}
          pending={save.isPending}
        />
      )}

      {panels.length === 0 ? (
        <p className="dim">No panels yet.</p>
      ) : (
        <div className="grid">
          {panels.map((panel) => (
            <section
              key={panel.id}
              className="panel"
              style={{
                gridColumn: `span ${Math.min(12, Math.max(1, panel.width))}`,
                minHeight: `${Math.max(1, panel.height) * ROW_HEIGHT}px`,
              }}
            >
              <header>
                <h3>{panel.title || nameOf(panel.viz.kind)}</h3>
                {mayWrite(tenant.role) && (
                  <button
                    type="button"
                    className="danger"
                    title="Remove this panel"
                    disabled={save.isPending}
                    onClick={() =>
                      save.mutate(panels.filter((other) => other.id !== panel.id))
                    }
                  >
                    ✕
                  </button>
                )}
              </header>
              <PanelBody
                panel={panel}
                tenant={tenant.tenant_id}
                from={resolved?.from}
                to={resolved?.to}
              />
            </section>
          ))}
        </div>
      )}

      {mayWrite(tenant.role) && dashboard.data && (
        <p className="dim">
          <button
            type="button"
            className="danger"
            disabled={remove.isPending}
            onClick={() => remove.mutate()}
          >
            Delete this dashboard
          </button>
        </p>
      )}
    </>
  );
}

/**
 * One panel's data.
 *
 * Its own `useQuery`, so twenty panels are twenty independent requests: one slow panel
 * spins on its own, and one failing panel says so in its own corner rather than taking
 * the page with it.
 */
function PanelBody({
  panel,
  tenant,
  from,
  to,
}: {
  panel: Panel;
  tenant: string;
  // Absent while the range is unresolvable, which is a state the header can be in.
  from?: Date | undefined;
  to?: Date | undefined;
}) {
  const alerts = useQuery({
    queryKey: ["alerts", tenant],
    queryFn: () => listAlerts(tenant),
    enabled: panel.viz.kind === "alerts",
    retry: false,
  });

  const asked =
    panel.query && from && to ? forWindow(panel.query, from, to) : null;

  const data = useQuery({
    queryKey: ["panel", tenant, panel.id, JSON.stringify(asked)],
    queryFn: () => runQuery(tenant, asked as NonNullable<typeof asked>),
    enabled: asked !== null,
    retry: false,
    // A dashboard left open on a wall is the case this is for: the panels are worth
    // keeping for a minute, and the time range in the header is what changes them.
    staleTime: 60_000,
  });

  if (panel.viz.kind === "alerts") {
    if (alerts.isPending) return <p className="dim">…</p>;
    if (alerts.isError) return <p className="warn">{message(alerts.error)}</p>;
    return <Alerts alerts={alerts.data} />;
  }

  if (!asked) return <p className="dim">No window.</p>;
  if (data.isPending) return <p className="dim">…</p>;
  if (data.isError) {
    // The server's own sentence, in the panel's own corner. A dashboard where one panel
    // is broken is still nineteen panels of information.
    return <p className="warn">{message(data.error)}</p>;
  }

  return <Drawn panel={panel} result={data.data} />;
}

function Drawn({ panel, result }: { panel: Panel; result: ResultSet }) {
  switch (panel.viz.kind) {
    case "time_series":
      return <TimeSeries result={result} unit={panel.viz.unit} />;
    case "stat":
      return (
        <Stat
          value={latest(result)}
          unit={panel.viz.unit}
          decimals={panel.viz.decimals}
        />
      );
    case "gauge":
      return (
        <Gauge
          value={latest(result)}
          min={panel.viz.min}
          max={panel.viz.max}
          unit={panel.viz.unit}
        />
      );
    case "table":
      return <Table result={result} />;
    case "alerts":
      // Unreachable: PanelBody answers the alerts kind before it gets here. Kept so the
      // match is exhaustive and adding a sixth kind is a compile error rather than a
      // blank corner of somebody's dashboard.
      return null;
  }
}

/** Add a panel: a saved search, a picture, and a title. */
function AddPanel({
  onAdd,
  pending,
}: {
  onAdd: (panel: Panel) => void;
  pending: boolean;
}) {
  const { tenant } = useShell();
  const [kind, setKind] = useState<Viz["kind"]>("time_series");
  const [search, setSearch] = useState("");
  const [title, setTitle] = useState("");

  const searches = useQuery({
    queryKey: ["searches", tenant.tenant_id],
    queryFn: () => listSearches(tenant.tenant_id),
    retry: false,
  });

  const chosen = (searches.data ?? []).find((s) => s.id === search);
  const needsQuery = kind !== "alerts";

  return (
    <form
      className="explore-form"
      onSubmit={(e) => {
        e.preventDefault();
        if (needsQuery && !chosen) return;

        const panel = blank(kind, crypto.randomUUID(), chosen?.query);
        onAdd({ ...panel, title: title.trim() || chosen?.name || nameOf(kind) });
        setTitle("");
        setSearch("");
      }}
    >
      <label>
        Panel
        <select value={kind} onChange={(e) => setKind(e.target.value as Viz["kind"])}>
          {KINDS.map((k) => (
            <option key={k} value={k}>
              {nameOf(k)}
            </option>
          ))}
        </select>
      </label>

      {needsQuery && (
        <label>
          From saved search
          <select value={search} onChange={(e) => setSearch(e.target.value)}>
            <option value="">choose one…</option>
            {(searches.data ?? []).map((s) => (
              <option key={s.id} value={s.id}>
                {s.name} · {s.signal}
              </option>
            ))}
          </select>
        </label>
      )}

      <label className="grow">
        Title
        <input
          type="text"
          value={title}
          placeholder={chosen?.name ?? nameOf(kind)}
          onChange={(e) => setTitle(e.target.value)}
        />
      </label>

      <button
        type="submit"
        className="primary"
        disabled={pending || (needsQuery && !chosen)}
      >
        Add panel
      </button>

      {needsQuery && (searches.data ?? []).length === 0 && (
        <span className="dim">
          Save a search in the Explorer first — a panel draws one.
        </span>
      )}
    </form>
  );
}
