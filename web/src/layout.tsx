/**
 * The one layout: left nav, header, content.
 *
 * The header holds the two controls that are not about any particular view — the tenant
 * switcher and the time range. SPEC calls the range "the single most-used control in the
 * product", which is why it is in the chrome rather than repeated per page: a range that
 * lives on a page is a range that resets when you leave it.
 */

import { Link, Outlet, useNavigate } from "@tanstack/react-router";
import { useState } from "react";

import { api } from "./api";
import { PRESETS, describeRange, useShell, type ShellSearch, type TimeRange } from "./shell";

/** Carries the shell's search params through every navigation. */
function keepSearch(old: ShellSearch): ShellSearch {
  return old;
}

function TenantSwitcher() {
  const { me, tenant, setTenant } = useShell();

  // A single-tenant install is the common case on-premise, and a select with one option
  // is a control that looks interactive and is not.
  if (me.tenants.length === 1) {
    return (
      <span className="dim" title={tenant.tenant_id}>
        {tenant.name}
      </span>
    );
  }

  return (
    <select
      aria-label="Tenant"
      value={tenant.tenant_id}
      onChange={(e) => setTenant(e.target.value)}
    >
      {me.tenants.map((t) => (
        <option key={t.tenant_id} value={t.tenant_id}>
          {t.name}
        </option>
      ))}
    </select>
  );
}

function RangePicker() {
  const { range, setRange } = useShell();
  const [editing, setEditing] = useState(false);

  if (editing) {
    return (
      <form
        className="range"
        onSubmit={(e) => {
          e.preventDefault();
          const form = new FormData(e.currentTarget);
          const next: TimeRange = {
            from: String(form.get("from") ?? ""),
            to: String(form.get("to") ?? ""),
          };
          setRange(next);
          setEditing(false);
        }}
      >
        <input name="from" defaultValue={range.from} aria-label="From" size={20} />
        <input name="to" defaultValue={range.to} aria-label="To" size={20} />
        <button type="submit" className="primary">
          Apply
        </button>
        <button type="button" onClick={() => setEditing(false)}>
          Cancel
        </button>
      </form>
    );
  }

  return (
    <div className="range">
      <span className="label">{describeRange(range)}</span>
      {PRESETS.map((p) => (
        <button
          key={p.label}
          className="preset"
          aria-pressed={range.from === p.range.from && range.to === p.range.to}
          onClick={() => setRange(p.range)}
        >
          {p.label}
        </button>
      ))}
      <button onClick={() => setEditing(true)} title="Absolute or relative, e.g. now-90m">
        Custom
      </button>
    </div>
  );
}

export function Layout() {
  const { me } = useShell();
  const navigate = useNavigate();

  return (
    <div className="shell">
      <div className="brand">uops</div>

      <header className="header">
        <TenantSwitcher />
        <div className="spacer" />
        <RangePicker />
        <span className="dim" title={me.email}>
          {me.display_name}
        </span>
        <button
          onClick={() => {
            // The cookie is cleared by the server; this only stops showing a shell the
            // session behind it no longer supports.
            void api.logout().finally(() => navigate({ to: "/login" }));
          }}
        >
          Sign out
        </button>
      </header>

      <nav className="nav">
        <Link to="/" search={keepSearch} activeProps={{ className: "active" }} activeOptions={{ exact: true }}>
          Overview
        </Link>
        <Link to="/map" search={keepSearch} activeProps={{ className: "active" }}>
          Map
        </Link>
        <Link to="/resources" search={keepSearch} activeProps={{ className: "active" }}>
          Resources
        </Link>
        <Link to="/explore" search={keepSearch} activeProps={{ className: "active" }}>
          Explore
        </Link>
        <Link to="/dashboards" search={keepSearch} activeProps={{ className: "active" }}>
          Dashboards
        </Link>
        <Link to="/alerts" search={keepSearch} activeProps={{ className: "active" }}>
          Alerts
        </Link>
      </nav>

      <main className="content">
        <Outlet />
      </main>
    </div>
  );
}
