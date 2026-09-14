/**
 * The pages that exist so far: sign in, an overview, and the resource inventory.
 *
 * The inventory is the one that matters — it is the first screen in this product that
 * shows a customer their own data, and it is the proof that the tenant header, the
 * session cookie, the scope extractor and the cursor pagination all line up.
 */

import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useNavigate } from "@tanstack/react-router";
import { useState } from "react";

import { ApiError, api, type Resource } from "./api";
import { describeRange, resolveRange, useShell } from "./shell";

export function LoginPage() {
  const navigate = useNavigate();
  const queryClient = useQueryClient();
  const [email, setEmail] = useState("");
  const [password, setPassword] = useState("");

  const login = useMutation({
    mutationFn: () => api.login(email, password),
    onSuccess: async () => {
      // The session cookie has changed, so anything cached under the old one is about
      // somebody else. Clearing beats invalidating: this is a different user.
      queryClient.clear();
      await navigate({ to: "/" });
    },
  });

  return (
    <div className="login">
      <h1>uops</h1>
      <p className="dim">Sign in to continue.</p>

      <form
        onSubmit={(e) => {
          e.preventDefault();
          login.mutate();
        }}
      >
        <label>
          Email
          <input
            type="email"
            autoComplete="username"
            autoFocus
            value={email}
            onChange={(e) => setEmail(e.target.value)}
            required
          />
        </label>

        <label>
          Password
          <input
            type="password"
            autoComplete="current-password"
            value={password}
            onChange={(e) => setPassword(e.target.value)}
            required
          />
        </label>

        {login.isError && (
          <div className="problem" role="alert">
            {/* Never "no such user" or "wrong password" — the server does not
                distinguish them and neither does this. An account-enumeration oracle
                in the error text would undo the constant-time login path behind it. */}
            {login.error instanceof ApiError && login.error.status === 401
              ? "Those credentials were not accepted."
              : "Sign-in failed. The server may be unreachable."}
          </div>
        )}

        <button type="submit" className="primary" disabled={login.isPending}>
          {login.isPending ? "Signing in…" : "Sign in"}
        </button>
      </form>
    </div>
  );
}

export function OverviewPage() {
  const { tenant, range } = useShell();
  const resolved = resolveRange(range);

  return (
    <>
      <h1>{tenant.name}</h1>
      <p className="dim">
        Showing {describeRange(range)}
        {resolved && (
          <>
            {" — "}
            <span className="mono">{resolved.from.toISOString()}</span> to{" "}
            <span className="mono">{resolved.to.toISOString()}</span>
          </>
        )}
        .
      </p>
      <p className="dim">
        Your role on this tenant is <strong>{tenant.role}</strong>.
      </p>
    </>
  );
}

function statusColour(status: string): string {
  if (status === "up" || status === "ok") return "var(--ok)";
  if (status === "down" || status === "critical") return "var(--danger)";
  if (status === "degraded" || status === "warn") return "var(--warn)";
  return "var(--text-dim)";
}

export function ResourcesPage() {
  const { tenant } = useShell();

  // Keyed by tenant, so switching tenants is a different cache entry rather than a
  // refetch over the top of the previous customer's rows.
  const resources = useQuery({
    queryKey: ["resources", tenant.tenant_id],
    queryFn: () => api.resources(tenant.tenant_id),
  });

  if (resources.isPending) return <p className="dim">Loading…</p>;

  if (resources.isError) {
    return (
      <div className="problem" role="alert">
        {resources.error instanceof ApiError
          ? resources.error.message
          : "Could not load resources."}
      </div>
    );
  }

  const items: Resource[] = resources.data.items;

  if (items.length === 0) {
    return (
      <div className="empty-state">
        <h1>No resources yet</h1>
        <p>
          Nothing has been discovered or created in {tenant.name}. Resources appear here
          as collectors report them, or when one is created through the API.
        </p>
      </div>
    );
  }

  return (
    <>
      <h1>Resources</h1>
      <p className="dim">
        {items.length} in {tenant.name}
        {resources.data.next && " (first page)"}
      </p>

      <table>
        <thead>
          <tr>
            <th>Name</th>
            <th>Kind</th>
            <th>Status</th>
            <th>Vendor</th>
            <th>Last seen</th>
          </tr>
        </thead>
        <tbody>
          {items.map((r) => (
            <tr key={r.id}>
              <td>{r.display_name ?? r.name}</td>
              <td className="dim">{r.kind}</td>
              <td style={{ color: statusColour(r.status) }}>{r.status}</td>
              <td className="dim">{r.vendor ?? "—"}</td>
              <td className="mono dim">{r.last_seen.slice(0, 19).replace("T", " ")}</td>
            </tr>
          ))}
        </tbody>
      </table>
    </>
  );
}
