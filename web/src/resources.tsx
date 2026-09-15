/**
 * The resource inventory: the list, and one resource.
 *
 * This is the first screen that shows a customer their own data, and the proof that the
 * session cookie, the tenant header, the scope extractor and the keyset pagination all
 * line up in a browser rather than only in a test.
 */

import { useInfiniteQuery, useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Link, useParams } from "@tanstack/react-router";

import {
  ApiError,
  STATUSES,
  api,
  type Resource,
  type ResourceStatus,
  type Role,
} from "./api";
import type { ShellSearch } from "./shell";
import { useShell } from "./shell";

function statusColour(status: ResourceStatus): string {
  switch (status) {
    case "up":
      return "var(--ok)";
    case "down":
      return "var(--danger)";
    case "degraded":
    case "maintenance":
      return "var(--warn)";
    default:
      return "var(--text-dim)";
  }
}

/** Roles are ordered; a check is "at least this". */
const RANK: Record<Role, number> = { viewer: 0, operator: 1, admin: 2 };

function atLeast(role: Role, needed: Role): boolean {
  return RANK[role] >= RANK[needed];
}

function keepSearch(old: ShellSearch): ShellSearch {
  return old;
}

export function ResourcesPage() {
  const { tenant } = useShell();

  // Keyed by tenant, so switching tenants is a different cache entry rather than a
  // refetch drawn over the previous customer's rows.
  const resources = useInfiniteQuery({
    queryKey: ["resources", tenant.tenant_id],
    queryFn: ({ pageParam }) => api.resources(tenant.tenant_id, pageParam),
    initialPageParam: undefined as string | undefined,
    // The cursor is opaque and the server decides when there are no more pages. A
    // client that computed "is there more" from the page size would be wrong exactly
    // when the last page is full.
    getNextPageParam: (last) => last.next ?? undefined,
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

  const items: Resource[] = resources.data.pages.flatMap((p) => p.items);

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
        {items.length} loaded in {tenant.name}
        {resources.hasNextPage && ", more available"}
      </p>

      <div className="scroll-x">
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
                <td>
                  <Link
                    to="/resources/$id"
                    params={{ id: r.id }}
                    search={keepSearch}
                    className="row-link"
                  >
                    {r.display_name ?? r.name}
                  </Link>
                </td>
                <td className="dim">{r.kind}</td>
                <td style={{ color: statusColour(r.status) }}>{r.status}</td>
                <td className="dim">{r.vendor ?? "—"}</td>
                <td className="mono dim">{r.last_seen.slice(0, 19).replace("T", " ")}</td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>

      {resources.hasNextPage && (
        <p>
          <button
            onClick={() => void resources.fetchNextPage()}
            disabled={resources.isFetchingNextPage}
          >
            {resources.isFetchingNextPage ? "Loading…" : "Load more"}
          </button>
        </p>
      )}
    </>
  );
}

export function ResourcePage() {
  const { tenant } = useShell();
  const { id } = useParams({ from: "/shell/resources/$id" });
  const queryClient = useQueryClient();

  const resource = useQuery({
    queryKey: ["resource", tenant.tenant_id, id],
    queryFn: () => api.resource(tenant.tenant_id, id),
  });

  const invalidate = async () => {
    await queryClient.invalidateQueries({ queryKey: ["resource", tenant.tenant_id, id] });
    await queryClient.invalidateQueries({ queryKey: ["resources", tenant.tenant_id] });
  };

  const setStatus = useMutation({
    mutationFn: (status: ResourceStatus) =>
      api.setResourceStatus(tenant.tenant_id, id, status),
    onSuccess: invalidate,
  });

  const decommission = useMutation({
    mutationFn: () => api.decommission(tenant.tenant_id, id),
    onSuccess: invalidate,
  });

  if (resource.isPending) return <p className="dim">Loading…</p>;

  if (resource.isError) {
    // 404 here is either "no such resource" or "not in this tenant", and the server
    // deliberately does not distinguish them. Neither does this.
    const notFound = resource.error instanceof ApiError && resource.error.status === 404;
    return (
      <div className="empty-state">
        <h1>{notFound ? "No such resource" : "Could not load this resource"}</h1>
        <p>
          {notFound
            ? `Nothing with that id exists in ${tenant.name}.`
            : resource.error instanceof ApiError
              ? resource.error.message
              : String(resource.error)}
        </p>
        <p>
          <Link to="/resources" search={keepSearch}>
            Back to resources
          </Link>
        </p>
      </div>
    );
  }

  const r = resource.data;
  const canEdit = atLeast(tenant.role, "operator");
  const attributes = Object.entries(r.attributes);

  return (
    <>
      <p className="dim">
        <Link to="/resources" search={keepSearch}>
          Resources
        </Link>{" "}
        /
      </p>
      <h1>{r.display_name ?? r.name}</h1>
      <p className="dim">
        <span className="mono">{r.kind}</span> ·{" "}
        <span style={{ color: statusColour(r.status) }}>{r.status}</span>
      </p>

      <table className="detail">
        <tbody>
          <Row label="Id" value={r.id} mono />
          <Row label="Name" value={r.name} />
          <Row label="Vendor" value={r.vendor} />
          <Row label="Model" value={r.model} />
          <Row label="OS" value={[r.os, r.os_version].filter(Boolean).join(" ") || null} />
          <Row label="Site" value={r.site_id} mono />
          <Row label="Parent" value={r.parent_id} mono />
          <Row label="First seen" value={r.first_seen.replace("T", " ")} mono />
          <Row label="Last seen" value={r.last_seen.replace("T", " ")} mono />
        </tbody>
      </table>

      <h2>Attributes</h2>
      {attributes.length === 0 ? (
        <p className="dim">None. Collectors set OpenTelemetry semconv keys here.</p>
      ) : (
        <table className="detail">
          <tbody>
            {attributes.map(([key, value]) => (
              <Row key={key} label={key} value={String(value)} mono />
            ))}
          </tbody>
        </table>
      )}

      {canEdit && (
        <>
          <h2>Status</h2>
          <div className="range">
            <select
              value={r.status}
              onChange={(e) => setStatus.mutate(e.target.value as ResourceStatus)}
              disabled={setStatus.isPending}
              aria-label="Status"
            >
              {STATUSES.map((s) => (
                <option key={s} value={s}>
                  {s}
                </option>
              ))}
            </select>
            <button
              onClick={() => decommission.mutate()}
              disabled={decommission.isPending || r.status === "decommissioned"}
              title="Retires the resource. Telemetry already written keeps resolving to it."
            >
              Decommission
            </button>
          </div>
          {(setStatus.isError || decommission.isError) && (
            <div className="problem" role="alert">
              {String(setStatus.error ?? decommission.error)}
            </div>
          )}
          <p className="dim">
            Decommissioning is a soft delete. History that resolves to nothing is worse
            than a row marked retired.
          </p>
        </>
      )}
    </>
  );
}

function Row({
  label,
  value,
  mono,
}: {
  label: string;
  value: string | null | undefined;
  mono?: boolean;
}) {
  return (
    <tr>
      <th scope="row">{label}</th>
      <td className={mono ? "mono" : undefined}>{value ?? "—"}</td>
    </tr>
  );
}
