/**
 * What is wrong right now, what would say so, and where it would say it.
 *
 * Three pages over one subsystem:
 *
 * * **Alerts** — what is firing and what is one evaluation from firing, with the one
 *   action an operator takes at 3am: taking it.
 * * **Rules** — what each rule says, whether it is on, and where it sends. New rules come
 *   from a saved search rather than an AST editor; see `NewRule` for why.
 * * **Channels** — where a page goes, and the record of every attempt including the ones
 *   the limits refused.
 *
 * # Why the alert list refreshes on its own and the Explorer does not
 *
 * The Explorer runs a query somebody typed, over a window somebody chose, and re-running
 * it on a timer would bill the customer for the page being open. The alert list is a
 * small indexed read of the control plane, and it is the one screen whose whole purpose
 * is to be current — an operator watching an incident must not have to press anything to
 * find out that a fourth device just went.
 */

import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Link } from "@tanstack/react-router";
import { useState } from "react";

import {
  COMPARISONS,
  SEVERITIES,
  acknowledge,
  ago,
  createChannel,
  createRule,
  deleteChannel,
  deleteRule,
  describe,
  describeSeconds,
  listAlerts,
  listChannels,
  listRules,
  listSent,
  order,
  setRuleEnabled,
  type Condition,
  type Rule,
  type Severity,
} from "./alerting";
import { message } from "./query";
import { listSearches, overWindow, type SavedSearch } from "./searches";
import { useShell } from "./shell";

/** How often the alert list re-reads. */
const REFRESH_MS = 10_000;

function mayWrite(role: string): boolean {
  return role === "operator" || role === "admin";
}

/** Severity, as the one thing that decides whether to get out of bed. */
function SeverityTag({ severity }: { severity: Severity }) {
  return <span className={`severity ${severity}`}>{severity}</span>;
}

export function AlertsPage() {
  const { tenant } = useShell();
  const client = useQueryClient();
  const [problem, setProblem] = useState<string | null>(null);

  const alerts = useQuery({
    queryKey: ["alerts", tenant.tenant_id],
    queryFn: () => listAlerts(tenant.tenant_id),
    // The one screen whose purpose is to be current. See the module docs.
    refetchInterval: REFRESH_MS,
    retry: false,
  });

  const ack = useMutation({
    mutationFn: (id: string) => acknowledge(tenant.tenant_id, id),
    onSuccess: async () => {
      setProblem(null);
      await client.invalidateQueries({ queryKey: ["alerts", tenant.tenant_id] });
    },
    onError: (error) => setProblem(message(error)),
  });

  const rows = order(alerts.data ?? []);
  const firing = rows.filter((a) => a.state === "firing").length;

  return (
    <>
      <h1>Alerts</h1>

      {alerts.isError && (
        <div className="problem" role="alert">
          {message(alerts.error)}
        </div>
      )}
      {problem && (
        <div className="problem" role="alert">
          {problem}
        </div>
      )}

      <p className="dim">
        {rows.length === 0
          ? "Nothing is firing."
          : `${firing} firing, ${rows.length - firing} pending. Refreshes every ${
              REFRESH_MS / 1000
            } seconds.`}{" "}
        <Link to="/alerts/rules">Rules</Link> · <Link to="/alerts/channels">Channels</Link>
      </p>

      {rows.length > 0 && (
        <div className="scroll-x">
          <table>
            <thead>
              <tr>
                <th>Severity</th>
                <th>State</th>
                <th>Resource</th>
                <th>Rule</th>
                <th>Value</th>
                <th>For</th>
                <th />
              </tr>
            </thead>
            <tbody>
              {rows.map((alert) => (
                <tr key={alert.id} className={alert.state}>
                  <td>
                    <SeverityTag severity={alert.severity} />
                  </td>
                  <td>
                    {alert.state}
                    {/* A pending alert is one nobody has been told about. Saying so on
                        the row stops it being read as a page that was missed. */}
                    {alert.state === "pending" && (
                      <span className="dim"> · nobody notified</span>
                    )}
                  </td>
                  <td>
                    <Link to="/resources/$id" params={{ id: alert.resource_id }}>
                      {alert.resource}
                    </Link>
                  </td>
                  <td>{alert.rule}</td>
                  <td className="mono">
                    {alert.last_value === null ? "—" : alert.last_value.toFixed(3)}
                  </td>
                  <td>{ago(alert.since)}</td>
                  <td>
                    {alert.acked_at ? (
                      <span className="dim">taken</span>
                    ) : (
                      mayWrite(tenant.role) && (
                        <button
                          type="button"
                          disabled={ack.isPending}
                          onClick={() => ack.mutate(alert.id)}
                          title="Take this alert: it stays in the list and stops paging"
                        >
                          Take
                        </button>
                      )
                    )}
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}
    </>
  );
}

export function RulesPage() {
  const { tenant } = useShell();
  const client = useQueryClient();
  const [problem, setProblem] = useState<string | null>(null);

  const rules = useQuery({
    queryKey: ["rules", tenant.tenant_id],
    queryFn: () => listRules(tenant.tenant_id),
    retry: false,
  });

  const refresh = async () => {
    setProblem(null);
    await client.invalidateQueries({ queryKey: ["rules", tenant.tenant_id] });
  };

  const toggle = useMutation({
    mutationFn: ({ id, enabled }: { id: string; enabled: boolean }) =>
      setRuleEnabled(tenant.tenant_id, id, enabled),
    onSuccess: refresh,
    onError: (error) => setProblem(message(error)),
  });

  const remove = useMutation({
    mutationFn: (id: string) => deleteRule(tenant.tenant_id, id),
    onSuccess: refresh,
    onError: (error) => setProblem(message(error)),
  });

  const all = rules.data ?? [];

  return (
    <>
      <h1>Alert rules</h1>
      <p className="dim">
        <Link to="/alerts">Alerts</Link> · <Link to="/alerts/channels">Channels</Link>
      </p>

      {mayWrite(tenant.role) && <NewRule onCreated={refresh} />}

      {problem && (
        <div className="problem" role="alert">
          {problem}
        </div>
      )}
      {rules.isError && (
        <div className="problem" role="alert">
          {message(rules.error)}
        </div>
      )}

      {all.length === 0 ? (
        <p className="dim">No rules yet.</p>
      ) : (
        <div className="scroll-x">
          <table>
            <thead>
              <tr>
                <th>Name</th>
                <th>Severity</th>
                <th>Says</th>
                <th>Every</th>
                <th>Channels</th>
                <th />
              </tr>
            </thead>
            <tbody>
              {all.map((rule: Rule) => (
                <tr key={rule.id} className={rule.enabled ? undefined : "disabled"}>
                  <td>{rule.name}</td>
                  <td>
                    <SeverityTag severity={rule.severity} />
                  </td>
                  <td>{describe(rule)}</td>
                  <td>{describeSeconds(rule.eval_interval_seconds)}</td>
                  <td>
                    {rule.notify.length === 0 ? (
                      // Legitimate while a rule is being tuned, and a trap afterwards.
                      // Saying it in the list is what stops it being discovered during
                      // the incident it was written for.
                      <span className="warn">tells nobody</span>
                    ) : (
                      `${rule.notify.length}`
                    )}
                  </td>
                  <td>
                    {mayWrite(tenant.role) && (
                      <>
                        <button
                          type="button"
                          disabled={toggle.isPending}
                          onClick={() =>
                            toggle.mutate({ id: rule.id, enabled: !rule.enabled })
                          }
                        >
                          {rule.enabled ? "Disable" : "Enable"}
                        </button>{" "}
                        <button
                          type="button"
                          className="danger"
                          disabled={remove.isPending}
                          onClick={() => remove.mutate(rule.id)}
                        >
                          Delete
                        </button>
                      </>
                    )}
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}
    </>
  );
}

/**
 * A new rule, from a saved search.
 *
 * Deliberately not an AST editor. A rule is a `Query` plus a condition, and the query half
 * already has a place where it is built by somebody who can see the rows it returns — the
 * Explorer. Building a second, worse query builder here would produce rules whose authors
 * never saw what they matched, which is how an alerting system ends up full of rules
 * nobody trusts.
 *
 * So: save the search, then alert on it. That is SPEC's own acceptance criterion — "a
 * saved search from the Log Explorer converts to an alert rule with no edits" — as a
 * screen rather than a test.
 */
function NewRule({ onCreated }: { onCreated: () => Promise<void> }) {
  const { tenant } = useShell();
  const [search, setSearch] = useState("");
  const [name, setName] = useState("");
  const [severity, setSeverity] = useState<Severity>("warning");
  const [op, setOp] = useState("gt");
  const [value, setValue] = useState("0");
  const [hold, setHold] = useState("300");
  const [channels, setChannels] = useState<string[]>([]);
  const [problem, setProblem] = useState<string | null>(null);

  const searches = useQuery({
    queryKey: ["searches", tenant.tenant_id],
    queryFn: () => listSearches(tenant.tenant_id),
    retry: false,
  });
  const known = useQuery({
    queryKey: ["channels", tenant.tenant_id],
    queryFn: () => listChannels(tenant.tenant_id),
    retry: false,
  });

  const create = useMutation({
    mutationFn: (body: Parameters<typeof createRule>[1]) =>
      createRule(tenant.tenant_id, body),
    onSuccess: async () => {
      setName("");
      setSearch("");
      setChannels([]);
      setProblem(null);
      await onCreated();
    },
    onError: (error) => setProblem(message(error)),
  });

  const chosen: SavedSearch | undefined = (searches.data ?? []).find(
    (s) => s.id === search,
  );

  const submit = () => {
    if (!chosen) return;
    const condition: Condition = {
      kind: "threshold",
      op: op as "gt",
      value: Number(value),
      hold_seconds: Number(hold),
    };

    // The saved search's own query, with only the window moved to end now — the same
    // substitution opening it in the Explorer makes. The span is the rule: "how many
    // matched in the last fifteen minutes" is a different rule from "in the last one".
    const from = new Date(Date.parse(chosen.query.time.start));
    const to = new Date(Date.parse(chosen.query.time.end));
    const span = Math.max(60_000, to.getTime() - from.getTime());
    const now = Date.now();

    create.mutate({
      name: name.trim() || chosen.name,
      query: overWindow(chosen.query, new Date(now - span), new Date(now)),
      condition,
      severity,
      notify: channels,
    });
  };

  return (
    <form
      className="explore-form"
      onSubmit={(e) => {
        e.preventDefault();
        submit();
      }}
    >
      <label>
        From saved search
        <select value={search} onChange={(e) => setSearch(e.target.value)}>
          <option value="">choose one…</option>
          {(searches.data ?? []).map((s) => (
            <option key={s.id} value={s.id}>
              {s.name} · {s.signal}
            </option>
          ))}
        </select>
      </label>

      <label className="grow">
        Name
        <input
          type="text"
          value={name}
          placeholder={chosen?.name ?? "what to call this rule"}
          onChange={(e) => setName(e.target.value)}
        />
      </label>

      <label>
        Fires when the count is
        <select value={op} onChange={(e) => setOp(e.target.value)}>
          {COMPARISONS.map((c) => (
            <option key={c.value} value={c.value}>
              {c.label}
            </option>
          ))}
        </select>
      </label>

      <label>
        Value
        <input
          type="number"
          value={value}
          onChange={(e) => setValue(e.target.value)}
          step="any"
        />
      </label>

      <label>
        For
        <select value={hold} onChange={(e) => setHold(e.target.value)}>
          {/* Zero is offered and is not the default: a rule with no dwell fires on the
              first breaching evaluation, which is right for "the interface went down" and
              is how a noisy signal becomes a pager storm. */}
          {[0, 60, 300, 900, 3600].map((s) => (
            <option key={s} value={s}>
              {describeSeconds(s)}
            </option>
          ))}
        </select>
      </label>

      <label>
        Severity
        <select
          value={severity}
          onChange={(e) => setSeverity(e.target.value as Severity)}
        >
          {SEVERITIES.map((s) => (
            <option key={s} value={s}>
              {s}
            </option>
          ))}
        </select>
      </label>

      <label>
        Tell
        <select
          multiple
          value={channels}
          onChange={(e) =>
            setChannels(Array.from(e.target.selectedOptions, (o) => o.value))
          }
        >
          {(known.data ?? []).map((c) => (
            <option key={c.id} value={c.id}>
              {c.name}
            </option>
          ))}
        </select>
      </label>

      <button type="submit" className="primary" disabled={!chosen || create.isPending}>
        {create.isPending ? "Creating…" : "Create rule"}
      </button>

      {(searches.data ?? []).length === 0 && (
        <span className="dim">
          Save a search in the Explorer first — a rule alerts on one.
        </span>
      )}
      {problem && (
        <span className="problem-inline" role="alert">
          {problem}
        </span>
      )}
    </form>
  );
}

export function ChannelsPage() {
  const { tenant } = useShell();
  const client = useQueryClient();
  const [name, setName] = useState("");
  const [url, setUrl] = useState("");
  const [problem, setProblem] = useState<string | null>(null);

  const channels = useQuery({
    queryKey: ["channels", tenant.tenant_id],
    queryFn: () => listChannels(tenant.tenant_id),
    retry: false,
  });
  const sent = useQuery({
    queryKey: ["sent", tenant.tenant_id],
    queryFn: () => listSent(tenant.tenant_id),
    retry: false,
  });

  const refresh = async () => {
    setProblem(null);
    await Promise.all([
      client.invalidateQueries({ queryKey: ["channels", tenant.tenant_id] }),
      client.invalidateQueries({ queryKey: ["sent", tenant.tenant_id] }),
    ]);
  };

  const create = useMutation({
    mutationFn: () =>
      createChannel(tenant.tenant_id, {
        name: name.trim(),
        kind: "webhook",
        config: { url: url.trim() },
      }),
    onSuccess: async () => {
      setName("");
      setUrl("");
      await refresh();
    },
    onError: (error) => setProblem(message(error)),
  });

  const remove = useMutation({
    mutationFn: (id: string) => deleteChannel(tenant.tenant_id, id),
    onSuccess: refresh,
    onError: (error) => setProblem(message(error)),
  });

  const named = new Map((channels.data ?? []).map((c) => [c.id, c.name]));

  return (
    <>
      <h1>Channels</h1>
      <p className="dim">
        <Link to="/alerts">Alerts</Link> · <Link to="/alerts/rules">Rules</Link>
      </p>

      {mayWrite(tenant.role) && (
        <form
          className="explore-form"
          onSubmit={(e) => {
            e.preventDefault();
            create.mutate();
          }}
        >
          <label>
            Name
            <input
              type="text"
              value={name}
              placeholder="ops webhook"
              onChange={(e) => setName(e.target.value)}
            />
          </label>
          <label className="grow">
            URL
            <input
              type="text"
              value={url}
              placeholder="http://hooks.internal/alerts"
              onChange={(e) => setUrl(e.target.value)}
            />
          </label>
          <button
            type="submit"
            className="primary"
            disabled={name.trim() === "" || url.trim() === "" || create.isPending}
          >
            {create.isPending ? "Adding…" : "Add webhook"}
          </button>
          {/* The server refuses https with a sentence explaining the proxy; this says it
              before somebody types one. */}
          <span className="dim">
            http:// only — this server terminates TLS at a proxy, so point an https
            endpoint at that.
          </span>
          {problem && (
            <span className="problem-inline" role="alert">
              {problem}
            </span>
          )}
        </form>
      )}

      {(channels.data ?? []).length === 0 ? (
        <p className="dim">No channels. A rule with none changes state and tells nobody.</p>
      ) : (
        <table>
          <thead>
            <tr>
              <th>Name</th>
              <th>Kind</th>
              <th>Where</th>
              <th>Limit</th>
              <th />
            </tr>
          </thead>
          <tbody>
            {(channels.data ?? []).map((channel) => (
              <tr key={channel.id}>
                <td>{channel.name}</td>
                <td>{channel.kind}</td>
                <td className="mono">{String(channel.config["url"] ?? "—")}</td>
                <td>{channel.max_per_minute}/min</td>
                <td>
                  {mayWrite(tenant.role) && (
                    <button
                      type="button"
                      className="danger"
                      disabled={remove.isPending}
                      onClick={() => remove.mutate(channel.id)}
                    >
                      Delete
                    </button>
                  )}
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      )}

      <h2>Recent notifications</h2>
      <p className="dim">
        Every attempt, including the ones the limits refused — which is the answer to
        &ldquo;why did nobody get paged&rdquo;.
      </p>

      {(sent.data ?? []).length === 0 ? (
        <p className="dim">Nothing has been sent yet.</p>
      ) : (
        <div className="scroll-x">
          <table>
            <thead>
              <tr>
                <th>When</th>
                <th>Channel</th>
                <th>Alert</th>
                <th>State</th>
                <th>Outcome</th>
                <th>Detail</th>
              </tr>
            </thead>
            <tbody>
              {(sent.data ?? []).map((row) => (
                <tr key={row.id}>
                  <td>{ago(row.sent_at)} ago</td>
                  <td>{named.get(row.channel_id) ?? "deleted"}</td>
                  <td className="mono">{row.dedup_key}</td>
                  <td>{row.phase}</td>
                  <td className={row.outcome === "sent" ? undefined : "warn"}>
                    {row.outcome.replace("_", " ")}
                  </td>
                  <td className="mono">{row.detail || "—"}</td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}
    </>
  );
}
