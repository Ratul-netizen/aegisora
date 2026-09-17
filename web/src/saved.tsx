/**
 * The saved-search bar above the Explorer's form.
 *
 * Four verbs — open, save, replace, delete — over a list that is the tenant's, not the
 * user's. That is a decision with a visible consequence: what somebody saves, their
 * colleagues see, which is the point of naming a search at all during an incident. It is
 * also why saving is an operator's verb and a viewer sees the list without the buttons,
 * rather than seeing buttons that fail.
 */

import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useState } from "react";

import type { Role } from "./api";
import { message, type Query } from "./query";
import {
  listSearches,
  removeSearch,
  replaceSearch,
  saveSearch,
  type SavedSearch,
} from "./searches";

export function SavedSearches({
  tenant,
  role,
  current,
  onOpen,
}: {
  tenant: string;
  role: Role;
  /** What the form describes right now, or null when the range is unresolvable. */
  current: () => Query | null;
  onOpen: (saved: SavedSearch) => void;
}) {
  const client = useQueryClient();
  const [selected, setSelected] = useState("");
  const [naming, setNaming] = useState(false);
  const [name, setName] = useState("");
  const [problem, setProblem] = useState<string | null>(null);

  const searches = useQuery({
    queryKey: ["searches", tenant],
    queryFn: () => listSearches(tenant),
    retry: false,
  });

  const done = async () => {
    setProblem(null);
    setNaming(false);
    setName("");
    await client.invalidateQueries({ queryKey: ["searches", tenant] });
  };

  const save = useMutation({
    mutationFn: (body: { name: string; query: Query }) => saveSearch(tenant, body),
    onSuccess: async (saved) => {
      setSelected(saved.id);
      await done();
    },
    // The server's own wording. "A search called Timeouts already exists" is a sentence
    // the operator can act on; "400" is not.
    onError: (error) => setProblem(message(error)),
  });

  const replace = useMutation({
    mutationFn: ({ id, body }: { id: string; body: { name: string; query: Query } }) =>
      replaceSearch(tenant, id, body),
    onSuccess: done,
    onError: (error) => setProblem(message(error)),
  });

  const remove = useMutation({
    mutationFn: (id: string) => removeSearch(tenant, id),
    onSuccess: async () => {
      setSelected("");
      await done();
    },
    onError: (error) => setProblem(message(error)),
  });

  const all = searches.data ?? [];
  const chosen = all.find((s) => s.id === selected) ?? null;
  const mayWrite = role === "operator" || role === "admin";

  return (
    <div className="saved">
      <label>
        Saved searches
        <select
          value={selected}
          onChange={(e) => setSelected(e.target.value)}
          disabled={all.length === 0}
        >
          <option value="">
            {all.length === 0 ? "none saved yet" : `${all.length} saved`}
          </option>
          {all.map((s) => (
            <option key={s.id} value={s.id}>
              {s.name} · {s.signal}
            </option>
          ))}
        </select>
      </label>

      <button type="button" disabled={!chosen} onClick={() => chosen && onOpen(chosen)}>
        Open
      </button>

      {mayWrite && (
        <>
          <button
            type="button"
            disabled={!chosen || replace.isPending}
            onClick={() => {
              const query = current();
              if (chosen && query) replace.mutate({ id: chosen.id, body: { name: chosen.name, query } });
            }}
            title={chosen ? `Overwrite ${chosen.name} with what the form describes` : undefined}
          >
            Replace
          </button>

          <button
            type="button"
            className="danger"
            disabled={!chosen || remove.isPending}
            onClick={() => chosen && remove.mutate(chosen.id)}
          >
            Delete
          </button>

          {naming ? (
            // An inline field rather than a modal or a prompt(): naming a search is one
            // word, and it happens while the operator is reading the rows behind it.
            <>
              <input
                type="text"
                value={name}
                autoFocus
                placeholder="name this search"
                onChange={(e) => setName(e.target.value)}
                onKeyDown={(e) => {
                  if (e.key === "Escape") void done();
                }}
              />
              <button
                type="button"
                className="primary"
                disabled={name.trim() === "" || save.isPending}
                onClick={() => {
                  const query = current();
                  if (query) save.mutate({ name: name.trim(), query });
                }}
              >
                {save.isPending ? "Saving…" : "Save"}
              </button>
              <button type="button" onClick={() => void done()}>
                Cancel
              </button>
            </>
          ) : (
            <button type="button" onClick={() => setNaming(true)}>
              Save this search…
            </button>
          )}
        </>
      )}

      {problem && (
        <span className="problem-inline" role="alert">
          {problem}
        </span>
      )}
      {searches.isError && <span className="dim">Saved searches are unavailable.</span>}
    </div>
  );
}
