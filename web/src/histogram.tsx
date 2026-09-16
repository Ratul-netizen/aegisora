/**
 * The histogram above the rows, and the drag that zooms it.
 *
 * SPEC §M3 asks for "histogram with drag-to-zoom" in v0.1, and the reason it is not
 * decoration is that a log search is a needle in a window somebody guessed at. The bars
 * say where the needles are, and dragging across them is how a fifteen-minute question
 * gets asked without typing two timestamps.
 *
 * # It is SVG and there is no chart library
 *
 * Bars, an axis and a drag selection. A charting dependency would bring a licence to
 * check, a bundle to ship and an abstraction to fight the first time something needs to
 * be exactly right — and this needs to be exactly right in one way: **a bar must be the
 * bucket it claims to be**. See `web/src/map.tsx`, which made the same call for the same
 * reason.
 *
 * # Empty buckets are drawn
 *
 * `ClickHouse` returns rows only for buckets that have data, so a gap in the result is a
 * gap in time — and a chart that packed the returned buckets side by side would draw a
 * quiet hour and a busy hour at the same width. The gaps are filled in here, which is
 * what makes the x-axis linear and the drag mean what it looks like it means.
 */

import { useRef, useState } from "react";

import type { ResultSet } from "./query";

export interface Bucket {
  /** The bucket's start. */
  at: Date;
  count: number;
}

/**
 * Turn a grouped count result into a dense series.
 *
 * The result has one row per non-empty bucket, `[bucket, n]`. This fills the empty ones
 * and clips to the window, so the returned array is exactly the buckets between `from`
 * and `to` — which is what makes a bar's x-position a time rather than an index.
 */
export function toBuckets(
  result: ResultSet,
  from: Date,
  to: Date,
  seconds: number,
): Bucket[] {
  const step = seconds * 1000;
  if (step <= 0) return [];

  const counted = new Map<number, number>();
  for (const row of result.rows) {
    const at = Date.parse(String(row[0]).replace(" ", "T") + "Z");
    const n = Number(row[1]);
    if (Number.isFinite(at) && Number.isFinite(n)) {
      // Floored to the step so a server bucket and a client bucket cannot disagree by a
      // millisecond and produce two bars where there is one.
      counted.set(Math.floor(at / step) * step, n);
    }
  }

  const start = Math.floor(from.getTime() / step) * step;
  const end = to.getTime();
  const out: Bucket[] = [];
  // A guard rather than a trust: a window and a bucket width that disagree — a
  // mis-derived `seconds`, say — would otherwise loop for a very long time.
  for (let at = start; at <= end && out.length < 5000; at += step) {
    out.push({ at: new Date(at), count: counted.get(at) ?? 0 });
  }
  return out;
}

/** Axis labels: a few, evenly spaced, in the viewer's own timezone. */
function ticks(buckets: Bucket[], count = 4): { at: Date; label: string; index: number }[] {
  const first = buckets[0];
  const last = buckets[buckets.length - 1];
  if (!first || !last) return [];

  // Under a day, the date is noise on every label; over it, the time alone is ambiguous.
  const sameDay = last.at.getTime() - first.at.getTime() < 24 * 3600 * 1000;
  const out: { at: Date; label: string; index: number }[] = [];
  for (let i = 0; i < count; i += 1) {
    const index = Math.floor((i * (buckets.length - 1)) / Math.max(1, count - 1));
    const bucket = buckets[index];
    if (!bucket) continue;
    out.push({
      at: bucket.at,
      index,
      label: sameDay
        ? bucket.at.toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" })
        : bucket.at.toLocaleString([], { month: "short", day: "numeric", hour: "2-digit" }),
    });
  }
  return out;
}

const HEIGHT = 96;

export function Histogram({
  buckets,
  seconds,
  onZoom,
}: {
  buckets: Bucket[];
  seconds: number;
  /** A drag across the bars. Inclusive of `from`, exclusive of `to`. */
  onZoom: (from: Date, to: Date) => void;
}) {
  const svg = useRef<SVGSVGElement | null>(null);
  const [drag, setDrag] = useState<{ from: number; to: number } | null>(null);

  if (buckets.length === 0) return null;

  const max = Math.max(...buckets.map((b) => b.count), 1);
  const width = 1000;
  const barWidth = width / buckets.length;

  /** Which bucket a pointer is over, clamped to the chart. */
  const bucketAt = (clientX: number): number => {
    const box = svg.current?.getBoundingClientRect();
    if (!box || box.width === 0) return 0;
    const fraction = (clientX - box.left) / box.width;
    return Math.min(buckets.length - 1, Math.max(0, Math.floor(fraction * buckets.length)));
  };

  const finish = () => {
    if (!drag) return;
    const [lo, hi] = drag.from <= drag.to ? [drag.from, drag.to] : [drag.to, drag.from];
    setDrag(null);
    // A click is not a zoom. Without this, every stray click on the chart would narrow
    // the window to one bucket and an operator would lose the view they were reading.
    if (lo === hi) return;

    const start = buckets[lo];
    const end = buckets[hi];
    if (!start || !end) return;
    onZoom(
      start.at,
      // Exclusive: the end of the last selected bucket, not its start, or a drag across
      // two bars would ask for a window that excludes the second one.
      new Date(end.at.getTime() + seconds * 1000),
    );
  };

  const total = buckets.reduce((sum, b) => sum + b.count, 0);

  return (
    <figure className="histogram">
      <svg
        ref={svg}
        viewBox={`0 0 ${width} ${HEIGHT}`}
        preserveAspectRatio="none"
        role="img"
        aria-label={`${total.toLocaleString()} rows in ${buckets.length} buckets of ${seconds} seconds`}
        onPointerDown={(e) => {
          e.currentTarget.setPointerCapture(e.pointerId);
          const at = bucketAt(e.clientX);
          setDrag({ from: at, to: at });
        }}
        onPointerMove={(e) => {
          if (drag) setDrag({ ...drag, to: bucketAt(e.clientX) });
        }}
        onPointerUp={finish}
        // A pointer that leaves the window mid-drag would otherwise leave the selection
        // painted forever, and the next click would extend it from wherever it stopped.
        onPointerCancel={() => setDrag(null)}
      >
        {buckets.map((b, i) => (
          <rect
            key={b.at.getTime()}
            x={i * barWidth}
            // Every non-empty bucket gets at least a pixel. A bar of height 0.2 is
            // invisible, and "no rows" and "three rows" are a distinction somebody
            // scanning for an incident needs.
            y={HEIGHT - Math.max(b.count > 0 ? 2 : 0, (b.count / max) * HEIGHT)}
            width={Math.max(barWidth - 1, 0.5)}
            height={Math.max(b.count > 0 ? 2 : 0, (b.count / max) * HEIGHT)}
            className="bar"
          >
            <title>
              {b.at.toLocaleString()} — {b.count.toLocaleString()}
            </title>
          </rect>
        ))}

        {drag && drag.from !== drag.to && (
          <rect
            className="selection"
            x={Math.min(drag.from, drag.to) * barWidth}
            y={0}
            width={(Math.abs(drag.to - drag.from) + 1) * barWidth}
            height={HEIGHT}
          />
        )}
      </svg>

      <figcaption>
        <span className="dim">
          {total.toLocaleString()} rows · one bar is {describeSeconds(seconds)} · drag to zoom
        </span>
        <span className="ticks">
          {ticks(buckets).map((t) => (
            <span key={t.index}>{t.label}</span>
          ))}
        </span>
      </figcaption>
    </figure>
  );
}

/** "5 minutes", not "300 seconds". */
export function describeSeconds(seconds: number): string {
  if (seconds < 60) return `${seconds} second${seconds === 1 ? "" : "s"}`;
  if (seconds < 3600) {
    const m = seconds / 60;
    return `${m} minute${m === 1 ? "" : "s"}`;
  }
  if (seconds < 86_400) {
    const h = seconds / 3600;
    return `${h} hour${h === 1 ? "" : "s"}`;
  }
  const d = seconds / 86_400;
  return `${d} day${d === 1 ? "" : "s"}`;
}
