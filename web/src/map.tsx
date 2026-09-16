/**
 * The estate, on a map.
 *
 * The screen an MSP opens first: forty customer sites across a country, and the one that
 * is red. Everything below it — the poller, the availability check, the state
 * transitions — exists so that this is true at a glance.
 *
 * # No tiles
 *
 * The coastline is a bundled SVG path, not a tile layer. This product is deployed
 * on-premise and often air-gapped, so a map that fetched tiles would be a blank rectangle
 * for exactly the customers most likely to have sites across a country — and it would
 * send every viewer's map extent to a third party, which is a data-protection
 * conversation nobody wants to have about a status board.
 *
 * So this is a locator map: coastlines, no roads, no labels, no zoom into a street. For
 * "which of my sites is red" that is the whole requirement.
 *
 * # Equirectangular, and why the arithmetic is right here
 *
 * `x = lon + 180`, `y = 90 - lat`, in a 360x180 viewBox. The same two lines position the
 * coastline and the pins, which is the point: a projection the pins and the map disagreed
 * about would put every site slightly in the sea, and slightly is the hardest kind of
 * wrong to notice.
 */

import { useMemo, useState } from "react";
import { useQuery } from "@tanstack/react-query";
import { Link } from "@tanstack/react-router";

import { api, type Site, type SiteCounts } from "./api";
import { useShell } from "./shell";
import { WORLD_LAND } from "./world";

/** The viewBox the bundled outline is drawn in. */
const WIDTH = 360;
const HEIGHT = 180;

/** Where a coordinate lands. See the module docs. */
function project(latitude: number, longitude: number): { x: number; y: number } {
  return { x: longitude + 180, y: 90 - latitude };
}

/**
 * What colour a site is.
 *
 * Worst-first, and deliberately not a proportion: one device down out of two hundred is
 * still an outage for whoever depends on that device, and a pin that faded to amber
 * because the other hundred and ninety-nine were fine would be hiding it. The size of the
 * problem is in the numbers on the card, not in the colour.
 */
function siteColour(counts: SiteCounts): string {
  if (counts.down > 0) return "var(--danger)";
  if (counts.degraded > 0) return "var(--warn)";
  if (counts.up > 0) return "var(--ok)";
  return "var(--text-dim)";
}

/**
 * How big a pin is.
 *
 * By resource count, on a cube root so that a site with a thousand devices is about
 * twice the radius of one with a hundred rather than ten times — a linear radius makes
 * one large site cover a country, and a linear *area* makes every small site invisible.
 */
function pinRadius(total: number): number {
  const base = 1.8;
  return base + Math.cbrt(Math.max(total, 1)) * 0.9;
}

function summarise(counts: SiteCounts): string {
  if (counts.total === 0) return "no resources";
  const parts: string[] = [];
  if (counts.down > 0) parts.push(`${counts.down} down`);
  if (counts.degraded > 0) parts.push(`${counts.degraded} degraded`);
  if (counts.up > 0) parts.push(`${counts.up} up`);
  if (counts.maintenance > 0) parts.push(`${counts.maintenance} in maintenance`);
  if (counts.unknown > 0) parts.push(`${counts.unknown} unknown`);
  return parts.join(", ");
}

export function MapPage() {
  const { tenant } = useShell();
  const [selected, setSelected] = useState<string | null>(null);

  const sites = useQuery({
    // Keyed by tenant, so switching customers is a different cache entry rather than a
    // refetch drawn over the previous one's estate.
    queryKey: ["sites", tenant.tenant_id],
    queryFn: () => api.sites(tenant.tenant_id),
    refetchInterval: 30_000,
  });

  const all = useMemo(() => sites.data ?? [], [sites.data]);
  const placed = useMemo(() => all.filter((s) => s.location), [all]);
  const unplaced = useMemo(() => all.filter((s) => !s.location), [all]);

  // Worst last, so a red pin draws over a green one it overlaps. SVG has no z-index.
  const ordered = useMemo(() => {
    const rank = (s: Site) =>
      s.resources.down > 0 ? 3 : s.resources.degraded > 0 ? 2 : s.resources.up > 0 ? 1 : 0;
    return [...placed].sort((a, b) => rank(a) - rank(b));
  }, [placed]);

  if (sites.isPending) return <p className="muted">Loading sites…</p>;
  if (sites.isError) return <p className="error">Sites could not be loaded.</p>;

  if (all.length === 0) {
    return (
      <section>
        <h1>Map</h1>
        <p className="muted">This tenant has no sites yet.</p>
      </section>
    );
  }

  const chosen = all.find((s) => s.id === selected) ?? null;

  return (
    <section className="map-page">
      <h1>Map</h1>

      <svg
        className="world"
        viewBox={`0 0 ${WIDTH} ${HEIGHT}`}
        role="img"
        aria-label={`${placed.length} of ${all.length} sites placed on a world map`}
      >
        <rect x={0} y={0} width={WIDTH} height={HEIGHT} className="ocean" />
        <path d={WORLD_LAND} className="land" />

        {ordered.map((site) => {
          const { x, y } = project(site.location!.latitude, site.location!.longitude);
          const r = pinRadius(site.resources.total);
          return (
            <g
              key={site.id}
              className={`pin${selected === site.id ? " selected" : ""}`}
              onClick={() => setSelected(site.id === selected ? null : site.id)}
              role="button"
              tabIndex={0}
              onKeyDown={(e) => {
                if (e.key === "Enter" || e.key === " ") {
                  e.preventDefault();
                  setSelected(site.id === selected ? null : site.id);
                }
              }}
              aria-label={`${site.name}: ${summarise(site.resources)}`}
            >
              {/* A halo on anything not healthy, so a red pin is findable on a map the
                  size of a postage stamp without relying on colour alone. */}
              {site.resources.down > 0 && (
                <circle cx={x} cy={y} r={r * 2.2} className="halo" />
              )}
              <circle cx={x} cy={y} r={r} fill={siteColour(site.resources)} />
              <title>
                {site.name} — {summarise(site.resources)}
              </title>
            </g>
          );
        })}
      </svg>

      {chosen && (
        <aside className="site-card">
          <h2>{chosen.name}</h2>
          <p className="muted">{chosen.timezone}</p>
          <p>{summarise(chosen.resources)}</p>
          <Link to="/resources" search={{ site: chosen.id } as never}>
            {chosen.resources.total} resource{chosen.resources.total === 1 ? "" : "s"}
          </Link>
        </aside>
      )}

      {unplaced.length > 0 && (
        <section className="unplaced">
          <h2>Not on the map</h2>
          {/* Listed rather than dropped. A site missing from a map looks like a site with
              nothing wrong, and these are the ones nobody has placed yet. */}
          <ul>
            {unplaced.map((site) => (
              <li key={site.id}>
                <span
                  className="dot"
                  style={{ background: siteColour(site.resources) }}
                  aria-hidden="true"
                />
                {site.name} — <span className="muted">{summarise(site.resources)}</span>
              </li>
            ))}
          </ul>
        </section>
      )}
    </section>
  );
}
