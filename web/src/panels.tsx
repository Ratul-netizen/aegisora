/**
 * The five panel types SPEC names for v0.1, and no more.
 *
 * *"Five types built well beats twelve built badly"* — and of the four it excludes,
 * heatmap and pie are choices this product does not need, while geo map and topology
 * depend on M6 and M7.
 *
 * # Still no chart library
 *
 * SVG by hand, the same call `histogram.tsx` and `map.tsx` made, for the same reason: a
 * line has to be the series it claims to be, and the moment a library is between the data
 * and the pixels, "why does this dip at 14:05" becomes a question about the library. The
 * whole of a line chart is a scale, a path and two axes.
 */

import { Link } from "@tanstack/react-router";

import { ago, order, type Alert } from "./alerting";
import { toSeries, type Series, type Viz } from "./dashboards";
import type { ResultSet } from "./query";

/**
 * The colours a chart assigns to lines, in order — UI-SPEC §1.4.
 *
 * The *series* palette, deliberately not the semantic one: a second line drawn in the
 * warning amber reads as a warning about something. Six, because a panel with more than
 * six lines is one whose grouping is too fine to read — and after six they repeat, which
 * is a visible signal that this has happened rather than a silent slide into twelve
 * indistinguishable blues.
 */
const LINE_COLOURS = [
  "var(--series-1)",
  "var(--series-2)",
  "var(--series-3)",
  "var(--series-4)",
  "var(--series-5)",
  "var(--series-6)",
];

/** A number, in the way a person reads one at a glance. */
function compact(value: number, decimals = 2): string {
  const abs = Math.abs(value);
  if (abs >= 1_000_000_000) return `${(value / 1_000_000_000).toFixed(1)}G`;
  if (abs >= 1_000_000) return `${(value / 1_000_000).toFixed(1)}M`;
  if (abs >= 10_000) return `${(value / 1_000).toFixed(1)}k`;
  return value.toFixed(decimals);
}

/**
 * A line per series over the panel's window.
 *
 * The y-axis starts at zero unless the data goes below it. A chart whose baseline floats
 * makes a 2% variation look like a cliff, which is the most common way a dashboard lies
 * to somebody who is not reading the axis.
 */
export function TimeSeries({
  result,
  unit,
}: {
  result: ResultSet;
  // `| undefined` spelled out because this project has exactOptionalPropertyTypes on:
  // "absent" and "present and undefined" are different types, and a panel's unit is
  // genuinely the second when the viz did not set one.
  unit?: string | undefined;
}) {
  const series = toSeries(result);
  const points = series.flatMap((s) => s.points);
  if (points.length === 0) return <p className="dim">No data in this window.</p>;

  const width = 600;
  const height = 160;
  const pad = { left: 44, right: 8, top: 8, bottom: 18 };

  const times = points.map((p) => p.at);
  const values = points.map((p) => p.value);
  const t0 = Math.min(...times);
  const t1 = Math.max(...times);
  const low = Math.min(0, ...values);
  const high = Math.max(...values);
  // A flat line at v has no range; give it one so it draws through the middle rather
  // than dividing by zero.
  const span = high - low || Math.abs(high) || 1;

  const x = (at: number) =>
    pad.left + ((at - t0) / (t1 - t0 || 1)) * (width - pad.left - pad.right);
  const y = (value: number) =>
    height - pad.bottom - ((value - low) / span) * (height - pad.top - pad.bottom);

  return (
    <figure className="chart">
      <svg viewBox={`0 0 ${width} ${height}`} role="img" preserveAspectRatio="none">
        {/* Three gridlines and their labels: enough to read a value off, few enough not
            to compete with the data. */}
        {[0, 0.5, 1].map((fraction) => {
          const value = low + span * fraction;
          return (
            <g key={fraction}>
              <line
                x1={pad.left}
                x2={width - pad.right}
                y1={y(value)}
                y2={y(value)}
                className="grid"
              />
              <text x={pad.left - 6} y={y(value) + 3} className="axis" textAnchor="end">
                {compact(value)}
              </text>
            </g>
          );
        })}

        {series.map((line: Series, i) => (
          <polyline
            key={line.name}
            className="line"
            stroke={LINE_COLOURS[i % LINE_COLOURS.length]}
            points={line.points.map((p) => `${x(p.at)},${y(p.value)}`).join(" ")}
          />
        ))}

        <text x={pad.left} y={height - 4} className="axis">
          {new Date(t0).toISOString().slice(11, 16)}
        </text>
        <text x={width - pad.right} y={height - 4} className="axis" textAnchor="end">
          {new Date(t1).toISOString().slice(11, 16)}
        </text>
      </svg>

      {/* A legend only when there is something to tell apart. */}
      {series.length > 1 && (
        <figcaption className="legend">
          {series.map((line, i) => (
            <span key={line.name}>
              <i style={{ background: LINE_COLOURS[i % LINE_COLOURS.length] }} />
              {line.name}
            </span>
          ))}
          {unit && <span className="dim">{unit}</span>}
        </figcaption>
      )}
    </figure>
  );
}

/** One number: what it is now. */
export function Stat({
  value,
  unit,
  decimals = 2,
}: {
  value: number | null;
  unit?: string | undefined;
  decimals?: number | undefined;
}) {
  if (value === null) return <p className="dim">No data.</p>;
  return (
    <p className="stat">
      {compact(value, decimals)}
      {unit && <span className="unit">{unit}</span>}
    </p>
  );
}

/**
 * One number against a range somebody chose.
 *
 * The bounds are the panel's, never the data's: a gauge whose range moves with what it is
 * showing always reads half full. A value outside them is drawn at the end it exceeded
 * and printed in full, because clamping the number as well as the bar would hide exactly
 * the reading somebody needs to see.
 */
export function Gauge({
  value,
  min,
  max,
  unit,
}: {
  value: number | null;
  min: number;
  max: number;
  unit?: string | undefined;
}) {
  if (value === null) return <p className="dim">No data.</p>;

  const span = max - min || 1;
  const fraction = Math.min(1, Math.max(0, (value - min) / span));

  return (
    <div className="gauge">
      <div className="track">
        <div
          className="fill"
          style={{ width: `${fraction * 100}%` }}
          role="meter"
          aria-valuenow={value}
          aria-valuemin={min}
          aria-valuemax={max}
        />
      </div>
      <p className="stat">
        {compact(value)}
        {unit && <span className="unit">{unit}</span>}
      </p>
      <p className="dim">
        {compact(min)} – {compact(max)}
        {value > max && " · above the panel's range"}
        {value < min && " · below the panel's range"}
      </p>
    </div>
  );
}

/** The rows as they came back: the escape hatch for a question no picture answers. */
export function Table({ result }: { result: ResultSet }) {
  if (result.rows.length === 0) return <p className="dim">No rows.</p>;

  return (
    <div className="scroll-x">
      <table>
        <thead>
          <tr>
            {result.columns.map((column) => (
              <th key={column.name} title={column.type}>
                {column.name}
              </th>
            ))}
          </tr>
        </thead>
        <tbody>
          {/* Capped: a table panel is a corner of a dashboard, and a query that returned
              five hundred rows would push every panel below it off the screen. The rows
              are all in the Explorer, which is where somebody reading five hundred of
              them should be. */}
          {result.rows.slice(0, 20).map((row, i) => (
            <tr key={i}>
              {row.map((cell, j) => (
                <td key={result.columns[j]?.name ?? j} className="mono">
                  {cell === null || cell === undefined ? "—" : String(cell)}
                </td>
              ))}
            </tr>
          ))}
        </tbody>
      </table>
      {result.rows.length > 20 && (
        <p className="dim">{result.rows.length - 20} more rows — open it in the Explorer.</p>
      )}
    </div>
  );
}

/** What is firing right now. The one panel that reads the control plane. */
export function Alerts({ alerts }: { alerts: Alert[] }) {
  const rows = order(alerts);
  if (rows.length === 0) return <p className="dim">Nothing is firing.</p>;

  return (
    <ul className="panel-alerts">
      {rows.slice(0, 8).map((alert) => (
        <li key={alert.id}>
          <span className={`severity ${alert.severity}`}>{alert.severity}</span>
          <Link to="/resources/$id" params={{ id: alert.resource_id }}>
            {alert.resource}
          </Link>
          <span className="dim">{alert.rule}</span>
          <span className="dim">{ago(alert.since)}</span>
        </li>
      ))}
      {rows.length > 8 && (
        <li className="dim">
          <Link to="/alerts">{rows.length - 8} more</Link>
        </li>
      )}
    </ul>
  );
}

/** What a panel of this kind is called, for the editor's menu. */
export function nameOf(kind: Viz["kind"]): string {
  switch (kind) {
    case "time_series":
      return "Time series";
    case "stat":
      return "Single stat";
    case "table":
      return "Table";
    case "gauge":
      return "Gauge";
    case "alerts":
      return "Alert list";
  }
}
