/**
 * Sign in, and the overview.
 *
 * The resource inventory lives in resources.tsx and the query explorer in explore.tsx;
 * both are large enough to read on their own.
 */

import { useMutation, useQueryClient } from "@tanstack/react-query";
import { useNavigate } from "@tanstack/react-router";
import { useState } from "react";

import { ApiError, api } from "./api";
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
