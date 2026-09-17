/**
 * The mark, and the one string that is the product's name.
 *
 * # Why this file exists at all
 *
 * SPEC's branding rule: *"Veyronis is a provisional product brand. No product-facing
 * identifier may be used as a persistence, crate, database, environment-variable, Docker
 * image or encryption identifier until brand clearance is complete. Product-facing
 * surfaces — README, documentation, the UI, marketing — say Veyronis. Everything in the
 * tree says `uops`."*
 *
 * The UI is a product-facing surface, so it is allowed to say the name — and the name is
 * kept in exactly one constant here so that saying it costs one line to change. That is
 * not hypothetical: the product was called *Aegisora* until 2026-09-16, and renaming it
 * touched eight lines of documentation and no code.
 *
 * As of 2026-09-17 the provisional name has a known conflict — `veyronis.com` is an
 * active software consultancy, and *Varonis* is a registered mark in an adjacent field —
 * so a rename is likely. Everything below is built to survive one.
 *
 * # Why the mark has no letter in it
 *
 * A monogram is a bet on a name. This one is a geometric figure: separate signals
 * converging on a single node — which is the product's actual thesis, that syslog, SNMP,
 * OTLP, flows and topology all resolve to one `resource_id`. It means the same thing
 * whatever the product ends up being called, and it reads at 16 pixels because it is
 * three strokes and a dot.
 *
 * Drawn in `currentColor` so it themes with everything else and needs no second asset for
 * dark and light.
 */

/**
 * The product's name, on product-facing surfaces only.
 *
 * One constant. See the module docs for why.
 */
export const PRODUCT = "Veyronis";

/**
 * The mark: three signals converging on one resource.
 *
 * `size` is a pixel box; the geometry is a 24-unit grid scaled into it. Given
 * `aria-hidden`, because it appears beside the wordmark everywhere it is used and a
 * screen reader announcing "logo" before the product's name is noise.
 */
export function Mark({ size = 20 }: { size?: number }) {
  return (
    <svg
      width={size}
      height={size}
      viewBox="0 0 24 24"
      fill="none"
      aria-hidden="true"
      focusable="false"
    >
      {/* Three signals, arriving at different angles. Rounded caps so they read as
          traces rather than as arrows, and thinned as they converge. */}
      <path
        d="M2 5.5 L10.5 10.2"
        stroke="currentColor"
        strokeWidth="2"
        strokeLinecap="round"
        opacity="0.55"
      />
      <path
        d="M2 12 L10.5 12"
        stroke="currentColor"
        strokeWidth="2"
        strokeLinecap="round"
        opacity="0.75"
      />
      <path
        d="M2 18.5 L10.5 13.8"
        stroke="currentColor"
        strokeWidth="2"
        strokeLinecap="round"
        opacity="0.55"
      />

      {/* The resource they resolve to. A ring rather than a disc: the identity is a
          thing signals attach to, not a full stop. */}
      <circle cx="15.5" cy="12" r="4.5" stroke="currentColor" strokeWidth="2" />
      <circle cx="15.5" cy="12" r="1.4" fill="currentColor" />
    </svg>
  );
}

/** The mark and the name, as they appear in the header. */
export function Wordmark() {
  return (
    <span className="brand">
      <span className="brand-mark">
        <Mark size={20} />
      </span>
      <span className="brand-name">{PRODUCT}</span>
    </span>
  );
}
