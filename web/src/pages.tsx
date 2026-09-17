/**
 * Signing in.
 *
 * Everything else that was here has grown its own file — the inventory in resources.tsx,
 * the explorer in explore.tsx, the overview in overview.tsx. This is the one page that
 * exists outside the shell, because it is the one a person reaches without a session.
 */

import { useMutation, useQueryClient } from "@tanstack/react-query";
import { useNavigate } from "@tanstack/react-router";
import { useState } from "react";

import { ApiError, api } from "./api";

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
