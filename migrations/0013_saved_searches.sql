-- Saved searches — a question somebody wants to ask again.
--
-- SPEC §M3: "Saved searches are stored `Query` ASTs. They become alert rules in M4 with
-- no translation." That sentence is the whole design, and it is why this table stores
-- the AST itself rather than the Explorer's form state. A saved search that had to be
-- translated into an alert rule would be two representations of one question, and the
-- day they disagreed the alert would fire on something the operator never searched for.
--
-- The M4 acceptance criterion is already written: *"a saved search from the Log Explorer
-- converts to an alert rule with no edits"*. It is met by `alert_rule.query` and
-- `saved_search.query` holding the same bytes.

CREATE TABLE saved_search (
    id          uuid        PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id   uuid        NOT NULL REFERENCES tenant (id) ON DELETE CASCADE,

    -- What the operator calls it. Unique within the tenant, like every other name a
    -- human writes here: two customers of one MSP both have a search called "BGP".
    name        text        NOT NULL,
    -- Why it exists. Optional, unlike a maintenance window's reason — a window that
    -- nobody can explain goes on suppressing alerts forever, whereas an unexplained
    -- search just sits there until somebody runs it.
    description text        NOT NULL DEFAULT '',

    -- ----------------------------------------------------------------------
    -- The query
    -- ----------------------------------------------------------------------
    --
    -- A `uops_query::Query` exactly as `POST /api/v1/query` receives it. jsonb rather
    -- than a column per clause, because the AST is a tree with an `and`/`or`/`not`
    -- interior and flattening it into columns would be inventing a second, weaker query
    -- language to store the first one in.
    --
    -- Not validated by this table beyond its shape. The compiler is the authority on
    -- whether a query is answerable — `severity` does not exist on `metrics`, a time
    -- bucket over a day is refused — and a CHECK constraint here could only ever hold a
    -- stale copy of those rules. `uops-store-pg` compiles the AST before it writes the
    -- row, so an unanswerable search is refused at the API rather than stored and then
    -- found to be broken by whoever opens it next.
    query       jsonb       NOT NULL,

    -- Denormalised from `query->>'signal'`, and kept honest by the CHECK below.
    --
    -- It exists so that listing a tenant's searches, grouping them by signal and — in M4
    -- — finding every search over metrics does not mean parsing a jsonb document per
    -- row. The CHECK is what makes the duplication safe: the two cannot drift, because
    -- PostgreSQL will not store a row where they disagree.
    signal      text        NOT NULL,

    created_by  uuid        REFERENCES app_user (id),
    created_at  timestamptz NOT NULL DEFAULT now(),
    updated_at  timestamptz NOT NULL DEFAULT now(),

    CONSTRAINT saved_search_name_is_unique_in_the_tenant UNIQUE (tenant_id, name),
    CONSTRAINT saved_search_name_not_blank CHECK (length(trim(name)) > 0),

    -- A jsonb column accepts `"hello"` and `[1,2,3]` as happily as an object. Everything
    -- reading this expects a document with a `signal`, so anything else is a row that
    -- parses as JSON and fails as a query.
    CONSTRAINT saved_search_query_is_an_object CHECK (jsonb_typeof(query) = 'object'),
    CONSTRAINT saved_search_signal_matches_the_query CHECK (signal = query->>'signal'),

    -- The signals that have a table behind them. `trace` (M8) and `flow` (M7) are in the
    -- AST and the compiler refuses them, so a search over one could be saved, listed,
    -- opened and never run — the worst kind of broken, because it looks fine until an
    -- incident.
    CONSTRAINT saved_search_signal_is_queryable CHECK (
        signal IN ('log', 'metric', 'event', 'state')
    )
);

-- ----------------------------------------------------------------------------
-- What the stored time window means
-- ----------------------------------------------------------------------------
--
-- `query->'time'` is absolute — the AST has no relative windows on purpose, so that a
-- compiled query is a pure function of its inputs and an alert that fired last Tuesday
-- can be re-run exactly. A saved search therefore carries the window it happened to be
-- saved over.
--
-- Every consumer substitutes its own: the Explorer puts the header's time range in when
-- it opens one, and M4's evaluator will put its evaluation interval in. The stored
-- window is *provenance* — "this was saved while looking at a 15-minute window" — and
-- never the window the search runs with. A saved search that reopened showing the same
-- forty rows forever would be a screenshot, not a question.

CREATE TRIGGER saved_search_set_updated_at
    BEFORE UPDATE ON saved_search
    FOR EACH ROW EXECUTE FUNCTION set_updated_at();

-- The list view: a tenant's searches, most recently changed first. That ordering is the
-- one an operator wants during an incident — the search they were editing five minutes
-- ago is the one they want back — and it is also what the UI sorts by, so it is the
-- index that stops that sort being a sequential scan plus a sort at every page load.
CREATE INDEX saved_search_by_tenant_idx
    ON saved_search (tenant_id, updated_at DESC);

-- The referencing-side index for the `created_by` foreign key. Migration 0010 explains
-- why every one of them needs its own, and the guard in migrations/tests/ requires it.
CREATE INDEX saved_search_created_by_idx
    ON saved_search (created_by);
