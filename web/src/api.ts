/**
 * The only place this app talks to the server.
 *
 * Three things are true of every request, and each is a decision made on the Rust side
 * that this file is the mirror of:
 *
 * 1. **The session is a cookie nothing here can read.** `uops_session` is HttpOnly, so
 *    there is no token in JavaScript, no token in `localStorage`, and nothing for an
 *    XSS to steal and replay from another machine. The cost is that every request must
 *    say `credentials: "include"`, and a request that forgets is simply unauthenticated.
 *    That is why there is one `request` function and not a `fetch` anywhere else.
 *
 * 2. **Mutations echo the CSRF cookie in a header.** `uops_csrf` is deliberately *not*
 *    HttpOnly — it exists to be read by this file and sent back in `X-Uops-Csrf`. A
 *    cross-site form post carries the cookie but cannot read it to set the header, which
 *    is the whole of the double-submit defence. `POST /api/v1/query` is a read and still
 *    sends it: the exemption list is where this kind of mistake lives.
 *
 * 3. **Every scoped request names its tenant.** The server will not guess. A request
 *    without `X-Uops-Tenant` is rejected rather than defaulted, and a tenant the user
 *    cannot see comes back 404 rather than 403 — so this file cannot tell "does not
 *    exist" from "not yours", which is exactly the point.
 */

/** RFC 7807, which is what every error from this API is. */
export interface Problem {
  type: string;
  title: string;
  status: number;
  detail?: string;
}

export class ApiError extends Error {
  readonly status: number;
  readonly problem: Problem | null;

  constructor(status: number, problem: Problem | null, fallback: string) {
    super(problem?.detail ?? problem?.title ?? fallback);
    this.name = "ApiError";
    this.status = status;
    this.problem = problem;
  }

  /** The session is gone or was never there. The router sends these to /login. */
  get isUnauthenticated(): boolean {
    return this.status === 401;
  }
}

/**
 * Read a cookie by name.
 *
 * Only ever used for `uops_csrf`, which is the one cookie this app is meant to see. If
 * this is ever called with `uops_session` it will return undefined, because that cookie
 * is HttpOnly — and that is the design working, not a bug to route around.
 */
function cookie(name: string): string | undefined {
  for (const part of document.cookie.split(";")) {
    const [key, ...rest] = part.trim().split("=");
    if (key === name) return rest.join("=");
  }
  return undefined;
}

const CSRF_COOKIE = "uops_csrf";
const CSRF_HEADER = "X-Uops-Csrf";
const TENANT_HEADER = "X-Uops-Tenant";

export interface RequestOptions {
  method?: string;
  body?: unknown;
  /** Required for every route except /auth/login, /auth/logout, /me and /health. */
  tenant?: string;
  signal?: AbortSignal;
}

/**
 * One request to the API.
 *
 * @throws {ApiError} for any non-2xx response, with the server's problem document when
 * it sent one. A network failure throws too, with status 0 — the caller has to handle
 * "the server said no" and "there was no server" the same way, because a user cannot
 * tell them apart either.
 */
export async function request<T>(path: string, options: RequestOptions = {}): Promise<T> {
  const method = options.method ?? "GET";
  const headers = new Headers();

  if (options.body !== undefined) headers.set("Content-Type", "application/json");
  if (options.tenant) headers.set(TENANT_HEADER, options.tenant);

  // Sent on every method that is not a plain read, including POST /query — see above.
  if (method !== "GET" && method !== "HEAD") {
    const token = cookie(CSRF_COOKIE);
    if (token) headers.set(CSRF_HEADER, token);
  }

  let response: Response;
  try {
    response = await fetch(path, {
      method,
      headers,
      credentials: "include",
      ...(options.body !== undefined ? { body: JSON.stringify(options.body) } : {}),
      ...(options.signal ? { signal: options.signal } : {}),
    });
  } catch (cause) {
    if (cause instanceof DOMException && cause.name === "AbortError") throw cause;
    throw new ApiError(0, null, "the server could not be reached");
  }

  if (response.status === 204) return undefined as T;

  const text = await response.text();

  if (!response.ok) {
    let problem: Problem | null = null;
    try {
      problem = JSON.parse(text) as Problem;
    } catch {
      // A proxy or a crash, not this API. Fall through to the status line.
    }
    throw new ApiError(response.status, problem, `${response.status} ${response.statusText}`);
  }

  return text ? (JSON.parse(text) as T) : (undefined as T);
}

// ---------------------------------------------------------------------------
// The shapes the server actually returns. Kept next to the client rather than in a
// types file, so that a change to a handler and a change to its type are one diff.
// ---------------------------------------------------------------------------

export type Role = "viewer" | "operator" | "admin";

export interface TenantMembership {
  tenant_id: string;
  name: string;
  slug: string;
  role: Role;
}

export interface Me {
  user_id: string;
  email: string;
  display_name: string;
  tenants: TenantMembership[];
}

/** Matches ResourceStatus in uops-core. */
export const STATUSES = [
  "up",
  "down",
  "degraded",
  "unknown",
  "maintenance",
  "decommissioned",
] as const;

export type ResourceStatus = (typeof STATUSES)[number];

export interface Resource {
  id: string;
  tenant_id: string;
  kind: string;
  name: string;
  display_name: string | null;
  vendor: string | null;
  model: string | null;
  os: string | null;
  os_version: string | null;
  status: ResourceStatus;
  site_id: string | null;
  parent_id: string | null;
  attributes: Record<string, unknown>;
  first_seen: string;
  last_seen: string;
}

export interface Page<T> {
  items: T[];
  /**
   * An opaque keyset cursor, or null at the end.
   *
   * Opaque on purpose: it encodes the sort key of the last row, and a client that takes
   * it apart is a client that breaks when the sort changes. Pass it back verbatim.
   */
  next: string | null;
}

export const api = {
  login: (email: string, password: string) =>
    request<void>("/api/v1/auth/login", { method: "POST", body: { email, password } }),

  logout: () => request<void>("/api/v1/auth/logout", { method: "POST" }),

  me: () => request<Me>("/api/v1/me"),

  resources: (tenant: string, cursor?: string) => {
    const query = cursor ? `?cursor=${encodeURIComponent(cursor)}` : "";
    return request<Page<Resource>>(`/api/v1/resources${query}`, { tenant });
  },

  resource: (tenant: string, id: string) =>
    request<Resource>(`/api/v1/resources/${encodeURIComponent(id)}`, { tenant }),

  setResourceStatus: (tenant: string, id: string, status: string) =>
    request<Resource>(`/api/v1/resources/${encodeURIComponent(id)}/status`, {
      method: "PATCH",
      body: { status },
      tenant,
    }),

  decommission: (tenant: string, id: string) =>
    request<Resource>(`/api/v1/resources/${encodeURIComponent(id)}`, {
      method: "DELETE",
      tenant,
    }),
};
