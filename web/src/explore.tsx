/**
 * The Log Explorer: build a query, run it, read the rows — and then follow them.
 *
 * It posts the AST from SPEC §M0.5 directly. There is no query DSL in this app and no
 * translation layer — the controls below assemble the same structure a saved alert is an
 * instance of, and the same one the M6 text language will parse onto. If something can
 * be asked here it can be alerted on, because they are the same object.
 *
 * # Three queries, one filter
 *
 * A run issues the row query, a histogram over the same filter, and a count per value for
 * each sidebar field. All three are *derived* from one `Query` rather than assembled
 * separately — see `toHistogram` and `toFieldCounts` — because a histogram that filtered
 * differently from the rows beneath it would be a chart of something else, and nobody
 * would notice until they counted the bars.
 *
 * # It does not run on its own
 *
 * Changing a control does not fire a query. A telemetry query reads a lot of data, and a
 * UI that runs one on every keystroke bills the customer for the user's typing. The time
 * range is the exception in the other direction: it is shared state, so changing it
 * re-runs whatever was last run, because that is what changing a time range means — and
 * it is what makes drag-to-zoom on the histogram a zoom rather than a redraw.
 */

import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useEffect, useMemo, useRef, useState } from "react";

import { ApiError } from "./api";
import { Histogram, describeSeconds, toBuckets } from "./histogram";
import {
  SEVERITIES,
  SIGNALS,
  type Expr,
  type Field,
  type Query,
  type ResultSet,
  type Severity,
  type Signal,
  type TextMode,
  bucketSeconds,
  buildFilter,
  displayOrder,
  isAbort,
  message,
  runQuery,
  toFieldCounts,
  toHistogram,
} from "./query";
import { resolveRange, useShell } from "./shell";
import { AllSignals } from "./signals";
import { SavedSearches } from "./saved";
import { overWindow, toControls, type SavedSearch } from "./searches";
import { POLL_MS, merge, pollTail } from "./tail";

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

/**
 * The fields the sidebar counts, per signal.
 *
 * Deliberately a short list of *low-cardinality* columns. A sidebar over `resource_id`
 * would ask ClickHouse to group ten thousand values to show eight, which is the
 * high-cardinality grouping cost the W1 benchmark measured and warned about.
 *
 * `host.name` and `service.name` are the exception and are safe: both are materialised
 * columns in `logs` and `events`, so grouping on them reads a column rather than parsing
 * a map.
 */
const SIDEBAR: Record<Signal, { label: string; field: Field }[]> = {
  log: [
    { label: "Severity", field: { field: "severity" } },
    { label: "Source", field: { field: "source_kind" } },
    { label: "Vendor", field: { field: "source_vendor" } },
    { label: "Host", field: { field: "attr", key: "host.name" } },
    { label: "Service", field: { field: "attr", key: "service.name" } },
  ],
  event: [
    { label: "Severity", field: { field: "severity" } },
    { label: "Category", field: { field: "event_category" } },
    { label: "Type", field: { field: "event_type" } },
    { label: "Host", field: { field: "attr", key: "host.name" } },
  ],
  state: [{ label: "Source", field: { field: "source_kind" } }],
  metric: [{ label: "Metric", field: { field: "metric" } }],
};

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

/**
 * The server's own wording, never this app's.
 *
 * SPEC's position is that a query which is correct but slow should say so rather than be
 * silently rewritten. That only works if the explanation reaches the person who can
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

/** One field's top values, with counts, as a clickable filter. */
function FieldCounts({
  tenant,
  base,
  label,
  field,
  onPick,
}: {
  tenant: string;
  base: Query;
  label: string;
  field: Field;
  onPick: (field: Field, value: string) => void;
}) {
  const counts = useQuery({
    queryKey: ["counts", tenant, label, JSON.stringify(base)],
    queryFn: () => runQuery(tenant, toFieldCounts(base, field)),
    retry: false,
    staleTime: 60_000,
  });

  if (counts.isPending) return null;
  if (counts.isError) {
    // A sidebar is an aid, not the answer. One field failing must not take the rows with
    // it, so this says so quietly and the table above carries on.
    return (
      <div className="field-counts">
        <h4>{label}</h4>
        <p className="dim">unavailable</p>
      </div>
    );
  }

  const rows = counts.data.rows.filter((r) => String(r[0] ?? "") !== "");
  if (rows.length === 0) return null;
  const top = Math.max(...rows.map((r) => Number(r[1]) || 0), 1);

  return (
    <div className="field-counts">
      <h4>{label}</h4>
      <ul>
        {rows.map((row) => {
          const value = String(row[0]);
          const n = Number(row[1]) || 0;
          return (
            <li key={value}>
              <button type="button" onClick={() => onPick(field, value)} title={`Filter to ${value}`}>
                <span className="value">{value}</span>
                <span className="count">{n.toLocaleString()}</span>
              </button>
              {/* A bar rather than a number alone: "4 200 and 3 900" is two numbers,
                  and two bars of almost the same length is a fact. */}
              <span className="meter" style={{ width: `${(n / top) * 100}%` }} />
            </li>
          );
        })}
      </ul>
    </div>
  );
}

/** One row, expanded: every column, and everything else about that resource just then. */
function RowDetail({
  tenant,
  result,
  row,
  onClose,
}: {
  tenant: string;
  result: ResultSet;
  row: unknown[];
  onClose: () => void;
}) {
  const columns = result.columns.map((c) => c.name);
  const resourceAt = columns.indexOf("resource_id");
  const timeAt = columns.indexOf("observed_at");

  const resourceId = resourceAt >= 0 ? String(row[resourceAt]) : null;
  const observedAt =
    timeAt >= 0 ? new Date(String(row[timeAt]).replace(" ", "T") + "Z") : null;

  return (
    <aside className="row-detail" aria-label="Row detail">
      <header>
        <h2>Row</h2>
        <button type="button" onClick={onClose} aria-label="Close">
          ✕
        </button>
      </header>

      <dl>
        {result.columns.map((column, i) => (
          <div key={column.name}>
            <dt>{column.name}</dt>
            <dd className="mono">{render(row[i], column.type)}</dd>
          </div>
        ))}
      </dl>

      {resourceId && observedAt && !Number.isNaN(observedAt.getTime()) ? (
        <AllSignals tenant={tenant} resourceId={resourceId} at={observedAt} />
      ) : (
        // Metrics rows carry both, so this is reachable only for a projection that
        // dropped one. Saying why beats an empty panel.
        <p className="dim">
          This row carries no resource and timestamp, so there is nothing to correlate it
          with.
        </p>
      )}
    </aside>
  );
}

/** How many consecutive failed polls end a tail. */
const MAX_FAILURES = 5;

export function ExplorePage() {
  const { tenant, range, setRange } = useShell();
  const client = useQueryClient();

  const [signal, setSignal] = useState<Signal>("log");
  const [search, setSearch] = useState("");
  const [mode, setMode] = useState<TextMode>("any_token");
  const [severity, setSeverity] = useState<Severity | "">("");
  const [limit, setLimit] = useState(100);
  /** Filters added by clicking a value in the sidebar. */
  const [picked, setPicked] = useState<{ field: Field; value: string }[]>([]);
  const [open, setOpen] = useState<number | null>(null);

  /**
   * The search being followed, or null.
   *
   * A separate query object rather than a flag over `ran`, because a tail is not the
   * search re-run: the operator starts following *this* filter, and then changes the
   * controls to build the next search while the tail keeps delivering the old one. That
   * is what somebody watching an incident actually does.
   */
  const [followed, setFollowed] = useState<Query | null>(null);
  const [tail, setTail] = useState<ResultSet | null>(null);
  const [lag, setLag] = useState({ behind: false, skipped: false });
  const [tailError, setTailError] = useState<string | null>(null);
  /** Whether the form is showing less than the query that ran — see `toControls`. */
  const [partial, setPartial] = useState(false);

  const run = useMutation({
    mutationFn: (q: Query) => runQuery(tenant.tenant_id, q),
  });

  /** What was actually run, so the histogram and the sidebar match the rows. */
  const [ran, setRan] = useState<Query | null>(null);

  const build = (): Query | null => {
    const resolved = resolveRange(range);
    if (!resolved) return null;

    const base = buildFilter({ signal, search, mode, severity });
    const clauses: Expr[] = [
      ...(base ? [base] : []),
      ...picked.map(
        (p): Expr => ({ op: "compare", field: p.field, cmp: "eq", value: p.value }),
      ),
    ];
    const filter =
      clauses.length === 0 ? undefined : clauses.length === 1 ? clauses[0] : { op: "and" as const, of: clauses };

    return {
      signal,
      time: { start: resolved.from.toISOString(), end: resolved.to.toISOString() },
      resources: { type: "all" },
      ...(filter ? { filter } : {}),
      limit,
    };
  };

  /**
   * Open a saved search.
   *
   * Runs the AST that was saved, with only the window replaced. The controls are set
   * from it as well so it can be read and edited — but that direction is lossy, and the
   * rows deliberately do not depend on it: what is on screen is the saved question, even
   * when the form below cannot draw all of it.
   */
  const openSaved = (saved: SavedSearch) => {
    const resolved = resolveRange(range);
    if (!resolved) return;

    const controls = toControls(saved.query);
    setSignal(controls.signal);
    setSearch(controls.search);
    setMode(controls.mode);
    setSeverity(controls.severity);
    setLimit(controls.limit);
    // Setting `picked` fires the effect below, which would rebuild the query from the
    // controls and run *that* — the approximation this function exists to avoid.
    applying.current = true;
    setPicked(controls.picked);
    setPartial(!controls.exact);

    const q = overWindow(saved.query, resolved.from, resolved.to);
    hasRun.current = true;
    setFollowed(null);
    setOpen(null);
    setRan(q);
    run.mutate(q);
  };

  const go = () => {
    // Typing in the form and pressing Run means the form is the question again.
    setPartial(false);
    const q = build();
    if (!q) return;
    hasRun.current = true;
    // A search and a tail are two different answers to two different questions, and
    // showing both at once would mean two tables disagreeing about which rows exist.
    // Running one ends the other; the tail is one click away again.
    setFollowed(null);
    setOpen(null);
    setRan(q);
    run.mutate(q);
  };

  // Re-run on a tenant or time-range change, but only if something has been run. The
  // first visit shows an empty state and an untouched form, not a query nobody asked for.
  //
  // What re-runs is *what was last run*, with the window replaced — not the controls
  // rebuilt. For a search typed into this form the two are the same thing. For a saved
  // search they are not: its filter may be one the form cannot express, and rebuilding
  // would answer a different question every time somebody dragged the histogram.
  const hasRun = useRef(false);
  const { mutate } = run;
  useEffect(() => {
    if (!hasRun.current) return;
    const resolved = resolveRange(range);
    const last = ranRef.current;
    if (!resolved || !last) return;

    const q = overWindow(last, resolved.from, resolved.to);
    setOpen(null);
    setRan(q);
    mutate(q);
    // `ran` is read through a ref so that re-running does not itself re-trigger this.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [tenant.tenant_id, range.from, range.to, mutate]);

  /** The last query run, for the effect above. */
  const ranRef = useRef<Query | null>(null);
  ranRef.current = ran;

  // A picked filter is a click, and a click should act. It is the one control that runs
  // on change, because nobody clicks a value in a sidebar and then looks for a button.
  const firstRender = useRef(true);
  const applying = useRef(false);
  useEffect(() => {
    if (firstRender.current) {
      firstRender.current = false;
      return;
    }
    if (applying.current) {
      // The chips were set by opening a saved search, which has already run the query
      // that was saved. Running again here would replace it with the form's rebuild.
      applying.current = false;
      return;
    }
    if (hasRun.current) go();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [picked]);

  /**
   * The watermark, outside React's state.
   *
   * It survives the effect below being torn down and set up again — which happens every
   * time a row is opened and closed — so resuming continues from where the tail stopped
   * rather than from a fresh minute. A `useState` here would re-run the effect on every
   * poll, and a fresh `null` on resume would re-deliver the last minute of rows as
   * though they had just arrived.
   */
  const since = useRef<string | null>(null);

  /**
   * The poll loop.
   *
   * Paused while a row is open, because prepending rows renumbers the list and the
   * detail panel is addressed by index: without this, a row opened during a busy minute
   * silently becomes a different row under the operator's eyes. Closing it resumes from
   * the same watermark, so the rows that arrived while it was open are delivered rather
   * than skipped.
   */
  useEffect(() => {
    if (!followed || open !== null) return;

    const controller = new AbortController();
    let timer: number | undefined;
    let stopped = false;

    let failures = 0;

    const poll = async () => {
      try {
        const page = await pollTail(
          tenant.tenant_id,
          followed,
          since.current,
          controller.signal,
        );
        // Only ever the server's own value. A window computed here would be wrong by
        // whatever this browser's clock is wrong by, and silently — an empty tail on a
        // busy estate, with nothing on screen to explain it.
        since.current = page.next_since;
        failures = 0;
        setTail((was) => merge(was, page));
        setLag({ behind: !page.complete, skipped: page.skipped });
        setTailError(null);
      } catch (error) {
        if (isAbort(error)) return;

        // The session ended. Polling on is 1 800 rejected requests an hour and an error
        // message where the login page should be — `me` is cached for thirty seconds and
        // does not refetch on focus, so nothing else here would notice a tab left open
        // overnight. Asking for it again is what sends the app to /login.
        if (error instanceof ApiError && error.isUnauthenticated) {
          stopped = true;
          setFollowed(null);
          setTailError("the session ended");
          void client.invalidateQueries({ queryKey: ["me"] });
          return;
        }

        // The watermark is deliberately not advanced. A ClickHouse restart or a proxy
        // hiccup is a gap of a few seconds, and the next successful poll asks for the
        // window that failed — so the rows that arrived during the outage arrive late
        // rather than never.
        failures += 1;
        setTailError(message(error));

        // A tail that cannot succeed must not keep asking. Five consecutive failures is
        // ten seconds of trying, which outlasts a restart and does not outlast a broken
        // query — and the rows are not lost, because the search that this was following
        // still finds them.
        if (failures >= MAX_FAILURES) {
          stopped = true;
          setFollowed(null);
          return;
        }
      }
      if (!stopped) timer = window.setTimeout(() => void poll(), POLL_MS);
    };

    void poll();
    return () => {
      stopped = true;
      controller.abort();
      window.clearTimeout(timer);
    };
  }, [followed, open, tenant.tenant_id, client]);

  /** Start following what the controls currently describe. */
  const follow = () => {
    const q = build();
    if (!q) return;
    since.current = null;
    setTail(null);
    setLag({ behind: false, skipped: false });
    setTailError(null);
    setOpen(null);
    setFollowed(q);
  };

  const unfollow = () => {
    setFollowed(null);
    setLag({ behind: false, skipped: false });
  };

  const resolved = resolveRange(range);
  // Extracted so the dependency arrays below are two numbers rather than two expressions
  // the linter cannot check — and so that a re-render with an identical window does not
  // recompute a chart, which is the whole reason these are memoised.
  const fromMs = resolved?.from.getTime() ?? 0;
  const toMs = resolved?.to.getTime() ?? 0;

  const seconds = useMemo(() => (fromMs && toMs ? bucketSeconds(fromMs, toMs) : 60), [fromMs, toMs]);

  const histogram = useQuery({
    queryKey: ["histogram", tenant.tenant_id, JSON.stringify(ran), seconds],
    queryFn: () => runQuery(tenant.tenant_id, toHistogram(ran as Query, seconds)),
    enabled: ran !== null,
    retry: false,
    staleTime: 60_000,
  });

  const buckets = useMemo(
    () =>
      histogram.data && fromMs && toMs
        ? toBuckets(histogram.data, new Date(fromMs), new Date(toMs), seconds)
        : [],
    [histogram.data, fromMs, toMs, seconds],
  );

  const textSearchable = signal === "log" || signal === "event";
  const fields = SIDEBAR[signal] ?? [];

  /**
   * The rows on screen: the tail's while following, the search's otherwise.
   *
   * One table either way, rather than two that would disagree about which rows exist —
   * and one `open` index into it, which is why following pauses while a row is open.
   */
  const shown: ResultSet | null = followed ? tail : (run.data ?? null);
  /** Which columns to draw, and in what order — see `displayOrder`. */
  const order = shown ? displayOrder(shown.columns) : [];

  return (
    <>
      <h1>Explore</h1>

      <SavedSearches
        tenant={tenant.tenant_id}
        role={tenant.role}
        current={build}
        describesWhatRan={!partial}
        onOpen={openSaved}
      />

      {partial && (
        <p className="warn" role="status">
          This saved search has a filter the form below cannot show, so the form is
          describing less than what ran. The rows are the saved search; pressing Run
          would replace it with what the form says.
        </p>
      )}

      <form
        className="explore-form"
        onSubmit={(e) => {
          e.preventDefault();
          go();
        }}
      >
        <label>
          Signal
          <select
            value={signal}
            onChange={(e) => {
              setSignal(e.target.value as Signal);
              // The picked filters name columns of the old signal. Keeping them would
              // send a filter on `event_category` to the `logs` table and get a 422.
              setPicked([]);
            }}
          >
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

        {/* Not a submit: following is a different verb from searching, and a form whose
            Enter key sometimes starts a tail is one nobody can predict. */}
        <button
          type="button"
          className={followed ? "following" : undefined}
          onClick={() => (followed ? unfollow() : follow())}
          aria-pressed={followed !== null}
        >
          {followed ? "Stop" : "Follow"}
        </button>
      </form>

      {picked.length > 0 && (
        <div className="picked">
          {picked.map((p) => (
            <button
              key={`${JSON.stringify(p.field)}=${p.value}`}
              type="button"
              className="chip"
              onClick={() => setPicked((was) => was.filter((q) => q !== p))}
              title="Remove this filter"
            >
              {"key" in p.field ? p.field.key : p.field.field} = {p.value} ✕
            </button>
          ))}
        </div>
      )}

      {/* The cost of the chosen match mode, before it is paid rather than after. */}
      {textSearchable && search.trim() !== "" && (
        <p className="dim">{TEXT_MODES.find((m) => m.value === mode)?.hint}</p>
      )}

      {run.isError && (
        <div className="problem" role="alert">
          {message(run.error)}
        </div>
      )}

      {followed && (
        <div className="tailing" role="status">
          <p>
            <span className="live" aria-hidden="true" />
            {open !== null
              ? "Paused while a row is open — nothing is being missed; the watermark is held."
              : "Following. New rows arrive at the top."}
            {" "}
            {tail ? `${tail.rows.length.toLocaleString()} rows in view.` : "Waiting…"}
          </p>

          {/* Both of these are the server's own answers about its own window, not this
              app's guesses. A tail that quietly drops rows is worse than one that says
              it is behind. */}
          {lag.behind && (
            <p className="warn">
              Ingest is arriving faster than the tail can show it, so some rows in the
              last window were left behind. The search still finds all of them.
            </p>
          )}
          {lag.skipped && (
            <p className="warn">
              This tail was away longer than the server looks back, so the rows in
              between were skipped. They are still in storage — run the search to see
              them.
            </p>
          )}
          {tailError && (
            <p className="warn">
              The last poll failed: {tailError}. Still trying, and the watermark is held,
              so whatever arrives meanwhile is delivered on the next one.
            </p>
          )}
        </div>
      )}

      {/* The chart and the sidebar describe the window that was searched, and a tail has
          no window. Hiding them while following is what keeps them from being read as a
          live chart of something they are not counting. */}
      {!followed && ran && buckets.length > 0 && (
        <Histogram
          buckets={buckets}
          seconds={seconds}
          onZoom={(from, to) =>
            // Absolute, not relative. A drag says "this window", and leaving it relative
            // would have it slide out from under the operator on the next re-read.
            setRange({ from: from.toISOString(), to: to.toISOString() })
          }
        />
      )}

      {shown && (
        <div className="explorer">
          {/* Counted over the searched window, so they belong to the search and not to
              the tail — see the note above the histogram. */}
          {!followed && fields.length > 0 && ran && (
            <nav className="fields" aria-label="Fields">
              {fields.map((f) => (
                <FieldCounts
                  key={f.label}
                  tenant={tenant.tenant_id}
                  base={ran}
                  label={f.label}
                  field={f.field}
                  onPick={(field, value) =>
                    setPicked((was) =>
                      was.some(
                        (p) => JSON.stringify(p.field) === JSON.stringify(field) && p.value === value,
                      )
                        ? was
                        : [...was, { field, value }],
                    )
                  }
                />
              ))}
            </nav>
          )}

          <div className="rows">
            <Warnings result={shown} />
            <p className="dim">
              {shown.rows.length.toLocaleString()} rows, read{" "}
              {shown.rows_read.toLocaleString()} from{" "}
              <span className="mono">{shown.table}</span> (
              {(shown.bytes_read / 1_048_576).toFixed(1)} MiB)
              {!followed && buckets.length > 0 && (
                <> · bars are {describeSeconds(seconds)}</>
              )}
              {/* The tail's totals are what it has cost since it was switched on, which
                  is the number that explains an afternoon's load. */}
              {followed && <> · since this tail started</>}.
            </p>

            {shown.rows.length === 0 ? (
              <p className="dim">
                {followed ? "Nothing has arrived yet." : "No rows in this window."}
              </p>
            ) : (
              <div className="scroll-x">
                <table>
                  <thead>
                    <tr>
                      {/* When, how bad, from where, what it said — then the rest. See
                          `displayOrder`: the compiler's column order is the table's, and
                          it puts three UUIDs before the message. */}
                      {order.map((at) => (
                        <th key={shown.columns[at]?.name ?? at} title={shown.columns[at]?.type}>
                          {shown.columns[at]?.name}
                        </th>
                      ))}
                    </tr>
                  </thead>
                  <tbody>
                    {/* The index is the key: a telemetry row has no identity of its own,
                        and two identical rows in a result set are two real occurrences
                        rather than a duplicate to be collapsed. While following, the
                        list is renumbered by every poll — which is exactly why opening a
                        row pauses it. */}
                    {shown.rows.map((row, i) => (
                      <tr
                        key={i}
                        onClick={() => setOpen(open === i ? null : i)}
                        className={open === i ? "open" : undefined}
                        aria-expanded={open === i}
                      >
                        {order.map((at) => (
                          <td key={shown.columns[at]?.name ?? at} className="mono">
                            {render(row[at], shown.columns[at]?.type ?? "")}
                          </td>
                        ))}
                      </tr>
                    ))}
                  </tbody>
                </table>
              </div>
            )}
          </div>

          {open !== null && shown.rows[open] && (
            <RowDetail
              tenant={tenant.tenant_id}
              result={shown}
              row={shown.rows[open]}
              onClose={() => setOpen(null)}
            />
          )}
        </div>
      )}

      {!shown && !followed && !run.isPending && !run.isError && (
        <p className="dim">Choose a signal and run. The window is the one in the header.</p>
      )}
    </>
  );
}
