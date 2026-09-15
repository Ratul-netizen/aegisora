/**
 * The Explorer: build a query, run it, read the rows.
 *
 * It posts the AST from SPEC §M0.5 directly. There is no query DSL in this app and no
 * translation layer — the controls below assemble the same structure a saved alert is an
 * instance of, and the same one the M6 text language will parse onto. If something can
 * be asked here it can be alerted on, because they are the same object.
 *
 * # It does not run on its own
 *
 * Changing a control does not fire a query. A telemetry query reads a lot of data, and
 * a UI that runs one on every keystroke bills the customer for the user's typing. The
 * time range is the exception in the other direction: it is shared state, so changing it
 * re-runs whatever was last run, because that is what changing a time range means.
 */

import { useMutation } from "@tanstack/react-query";
import { useEffect, useRef, useState } from "react";

import {
  SEVERITIES,
  SIGNALS,
  type Query,
  type ResultSet,
  type Severity,
  type Signal,
  type TextMode,
  buildFilter,
  message,
  runQuery,
} from "./query";
import { resolveRange, useShell } from "./shell";

const TEXT_MODES: { value: TextMode; label: string; hint: string }[] = [
  { value: "any_token", label: "any word", hint: "Uses the text index." },
  { value: "all_token", label: "all words", hint: "Uses the text index." },
  {
    value: "phrase",
    label: "phrase",
    hint: "Narrows by token, then verifies by scanning. Slower.",
  },
  {
    value: "substring",
    label: "substring",
    hint: "Cannot use the text index at all. Slowest, and it reads the whole window.",
  },
];

/** How a ClickHouse value is best shown, from the column type the server reported. */
function render(value: unknown, type: string): string {
  if (value === null || value === undefined) return "—";
  if (type.startsWith("DateTime")) return String(value).replace("T", " ").replace("Z", "");
  if (type.startsWith("Map") || type.startsWith("Array")) {
    // An empty map is the common case and `{}` is noise in a table of a thousand rows.
    const text = JSON.stringify(value);
    return text === "{}" || text === "[]" ? "—" : text;
  }
  return typeof value === "string" ? value : JSON.stringify(value);
}

function Results({ result }: { result: ResultSet }) {
  if (result.rows.length === 0) {
    return (
      <>
        <Warnings result={result} />
        <p className="dim">
          No rows. The query read {result.rows_read.toLocaleString()} rows from{" "}
          <span className="mono">{result.table}</span>.
        </p>
      </>
    );
  }

  return (
    <>
      <Warnings result={result} />
      <p className="dim">
        {result.rows.length.toLocaleString()} rows, read{" "}
        {result.rows_read.toLocaleString()} from <span className="mono">{result.table}</span>{" "}
        ({(result.bytes_read / 1_048_576).toFixed(1)} MiB).
      </p>
      <div className="scroll-x">
        <table>
          <thead>
            <tr>
              {result.columns.map((c) => (
                <th key={c.name} title={c.type}>
                  {c.name}
                </th>
              ))}
            </tr>
          </thead>
          <tbody>
            {/* The index is the key: a telemetry row has no identity of its own, and
                two identical rows in a result set are two real occurrences rather than
                a duplicate to be collapsed. */}
            {result.rows.map((row, i) => (
              <tr key={i}>
                {row.map((cell, j) => (
                  <td key={result.columns[j]?.name ?? j} className="mono">
                    {render(cell, result.columns[j]?.type ?? "")}
                  </td>
                ))}
              </tr>
            ))}
          </tbody>
        </table>
      </div>
    </>
  );
}

/**
 * The server's own wording, never this app's.
 *
 * SPEC's position is that a query which is correct but slow should say so rather than
 * be silently rewritten. That only works if the explanation reaches the person who can
 * change the query, which is here.
 */
function Warnings({ result }: { result: ResultSet }) {
  if (result.warnings.length === 0) return null;
  return (
    <ul className="warnings">
      {result.warnings.map((w) => (
        <li key={w.warning}>{w.message}</li>
      ))}
    </ul>
  );
}

export function ExplorePage() {
  const { tenant, range } = useShell();

  const [signal, setSignal] = useState<Signal>("log");
  const [search, setSearch] = useState("");
  const [mode, setMode] = useState<TextMode>("any_token");
  const [severity, setSeverity] = useState<Severity | "">("");
  const [limit, setLimit] = useState(100);

  const run = useMutation({
    mutationFn: (q: Query) => runQuery(tenant.tenant_id, q),
  });

  const build = (): Query | null => {
    const resolved = resolveRange(range);
    if (!resolved) return null;
    const filter = buildFilter({ signal, search, mode, severity });
    return {
      signal,
      time: { start: resolved.from.toISOString(), end: resolved.to.toISOString() },
      resources: { type: "all" },
      ...(filter ? { filter } : {}),
      limit,
    };
  };

  // Re-run on a tenant or time-range change, but only if something has been run. The
  // first visit to this page shows an empty state and an untouched form, not a query
  // nobody asked for.
  const hasRun = useRef(false);
  const { mutate } = run;
  useEffect(() => {
    if (!hasRun.current) return;
    const q = build();
    if (q) mutate(q);
    // build() closes over every control, and re-running on a control change is exactly
    // what this page must not do. Only these two are shared state.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [tenant.tenant_id, range.from, range.to, mutate]);

  const textSearchable = signal === "log" || signal === "event";

  return (
    <>
      <h1>Explore</h1>

      <form
        className="explore-form"
        onSubmit={(e) => {
          e.preventDefault();
          const q = build();
          if (!q) return;
          hasRun.current = true;
          run.mutate(q);
        }}
      >
        <label>
          Signal
          <select value={signal} onChange={(e) => setSignal(e.target.value as Signal)}>
            {SIGNALS.map((s) => (
              <option key={s.value} value={s.value}>
                {s.label}
              </option>
            ))}
          </select>
        </label>

        {textSearchable && (
          <>
            <label className="grow">
              {signal === "log" ? "Message contains" : "Event type contains"}
              <input
                type="text"
                value={search}
                onChange={(e) => setSearch(e.target.value)}
                placeholder="connection refused"
              />
            </label>

            <label>
              Match
              <select value={mode} onChange={(e) => setMode(e.target.value as TextMode)}>
                {TEXT_MODES.map((m) => (
                  <option key={m.value} value={m.value}>
                    {m.label}
                  </option>
                ))}
              </select>
            </label>

            <label>
              Severity at least
              <select
                value={severity}
                onChange={(e) => setSeverity(e.target.value as Severity | "")}
              >
                <option value="">any</option>
                {SEVERITIES.map((s) => (
                  <option key={s} value={s}>
                    {s}
                  </option>
                ))}
              </select>
            </label>
          </>
        )}

        <label>
          Limit
          <select value={limit} onChange={(e) => setLimit(Number(e.target.value))}>
            {[50, 100, 500, 1000].map((n) => (
              <option key={n} value={n}>
                {n}
              </option>
            ))}
          </select>
        </label>

        <button type="submit" className="primary" disabled={run.isPending}>
          {run.isPending ? "Running…" : "Run"}
        </button>
      </form>

      {/* The cost of the chosen match mode, before it is paid rather than after. */}
      {textSearchable && search.trim() !== "" && (
        <p className="dim">{TEXT_MODES.find((m) => m.value === mode)?.hint}</p>
      )}

      {run.isError && (
        <div className="problem" role="alert">
          {message(run.error)}
        </div>
      )}

      {run.data && <Results result={run.data} />}

      {!run.data && !run.isPending && !run.isError && (
        <p className="dim">Choose a signal and run. The window is the one in the header.</p>
      )}
    </>
  );
}
