/**
 * "Show all signals for this resource around this timestamp."
 *
 * SPEC §M3 calls this *"the seed of the Investigation Workspace, and the one interaction
 * that demonstrates the product thesis in ten seconds"*, and that is exactly what it is:
 * a log line, one click, and every other thing this product knows about that device at
 * that moment.
 *
 * It is only possible because of a decision made in M0 and held since. A syslog message,
 * an SNMP sample, an OTLP record and a state transition all resolve to **one**
 * `resource_id`, and every telemetry table sorts by `(tenant_id, resource_id,
 * observed_at)`. So this is four range reads on the same key rather than four searches —
 * which is why it can run on a click instead of on a button somebody has to mean.
 *
 * # The window
 *
 * Centred on the row's own timestamp, not on the page's range. An operator who has found
 * a line at 14:32:07 is asking about 14:32, and re-using a six-hour page window would
 * bury the answer in six hours of everything else.
 */

import { useQueries } from "@tanstack/react-query";

import {
  type Query,
  type ResultSet,
  type Signal,
  SIGNALS,
  message,
  runQuery,
} from "./query";

/** How far either side of the row to look. */
export const AROUND_MS = 5 * 60 * 1000;

/** The four signals, and how to summarise a row of each in one line. */
const SHOWN: { signal: Signal; describe: (columns: string[], row: unknown[]) => string }[] = [
  {
    signal: "log",
    describe: (columns, row) => pick(columns, row, ["severity", "body"]),
  },
  {
    signal: "event",
    describe: (columns, row) => pick(columns, row, ["event_category", "event_type", "summary"]),
  },
  {
    signal: "state",
    describe: (columns, row) => pick(columns, row, ["from_state", "to_state", "reason"]),
  },
  {
    signal: "metric",
    describe: (columns, row) => pick(columns, row, ["metric", "value", "unit"]),
  },
];

/** Some named columns of a row, in order, skipping the ones that are absent or empty. */
function pick(columns: string[], row: unknown[], names: string[]): string {
  return names
    .map((name) => {
      const at = columns.indexOf(name);
      if (at < 0) return null;
      const value = row[at];
      if (value === null || value === undefined || value === "") return null;
      return String(value);
    })
    .filter((v): v is string => v !== null)
    .join("  ");
}

function when(columns: string[], row: unknown[]): string {
  const at = columns.indexOf("observed_at");
  if (at < 0) return "";
  return String(row[at]).replace("T", " ").replace("Z", "").slice(0, 23);
}

function around(resourceId: string, at: Date, signal: Signal): Query {
  return {
    signal,
    time: {
      start: new Date(at.getTime() - AROUND_MS).toISOString(),
      end: new Date(at.getTime() + AROUND_MS).toISOString(),
    },
    // The whole point: one id, four tables, one sort key.
    resources: { type: "ids", ids: [resourceId] },
    // Small on purpose. This is a summary that says "go and look", not a second Explorer
    // — and a hundred rows per signal in a side panel is a wall nobody reads.
    limit: 25,
  };
}

export function AllSignals({
  tenant,
  resourceId,
  at,
}: {
  tenant: string;
  resourceId: string;
  at: Date;
}) {
  const results = useQueries({
    queries: SHOWN.map(({ signal }) => ({
      queryKey: ["signals", tenant, resourceId, at.toISOString(), signal],
      queryFn: () => runQuery(tenant, around(resourceId, at, signal)),
      // A panel that re-fetched four queries every time it re-rendered would make a
      // click expensive; the window and the resource are both fixed once it is open.
      staleTime: 60_000,
      retry: false,
    })),
  });

  return (
    <section className="all-signals">
      <h3>
        Everything around {at.toLocaleTimeString()}{" "}
        <span className="dim">± {AROUND_MS / 60_000} minutes</span>
      </h3>

      {SHOWN.map(({ signal, describe }, i) => {
        const result = results[i];
        const label = SIGNALS.find((s) => s.value === signal)?.label ?? signal;
        // `useQueries` returns one result per query, so this cannot be missing — but the
        // types do not say so, and an assertion would be a claim the compiler cannot
        // check.
        if (!result) return null;

        if (result.isPending) {
          return (
            <div key={signal} className="signal-group">
              <h4>{label}</h4>
              <p className="dim">Looking…</p>
            </div>
          );
        }
        if (result.isError) {
          return (
            <div key={signal} className="signal-group">
              <h4>{label}</h4>
              {/* Named rather than swallowed. One signal failing while three answer is
                  a partial picture, and an operator drawing a conclusion from it should
                  know which quarter is missing. */}
              <p className="problem">{message(result.error)}</p>
            </div>
          );
        }

        const data = result.data as ResultSet;
        const columns = data.columns.map((c) => c.name);
        if (data.rows.length === 0) {
          return (
            <div key={signal} className="signal-group">
              <h4>{label}</h4>
              <p className="dim">Nothing.</p>
            </div>
          );
        }

        return (
          <div key={signal} className="signal-group">
            <h4>
              {label} <span className="dim">{data.rows.length}</span>
            </h4>
            <ol className="signal-rows">
              {data.rows.map((row, n) => (
                // The index is the key: a telemetry row has no identity of its own, and
                // two identical rows are two real occurrences.
                <li key={n}>
                  <span className="mono dim">{when(columns, row)}</span>{" "}
                  <span className="mono">{describe(columns, row)}</span>
                </li>
              ))}
            </ol>
          </div>
        );
      })}
    </section>
  );
}
