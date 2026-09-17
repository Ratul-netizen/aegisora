-- Discovery jobs, runs and candidates — M5, specified in docs/M5-discovery.md.
--
-- Migration 0008 is also called discovery and is a different thing: it keys the
-- *children* of a device that is already known. This is about finding the device.
--
-- Three tables, because discovery answers three questions that change at three
-- different rates. `discovery_job` is what an operator wrote down and edits rarely.
-- `discovery_run` is one execution and is append-only. `discovery_candidate` is the
-- residue — everything a run found and could not turn into a resource by itself — and
-- is the table that keeps the feature honest: a product that silently drops what it
-- cannot classify is one whose inventory an operator cannot trust.

-- ----------------------------------------------------------------------------
-- The bound on a sweep
-- ----------------------------------------------------------------------------
--
-- §2.3 of the spec: *"the caps are constants in one place, and the schema enforces the
-- first two — a limit that lives only in the application is one a second caller does not
-- have."* A CHECK cannot contain a subquery, so the arithmetic over the array lives in
-- IMMUTABLE functions and the constraints call them.

-- The number of addresses a job would probe. 2^(32 - masklen) per range, summed.
--
-- IPv4 only, which is why `family(r) = 4` is a separate constraint below rather than
-- something this function tolerates: on a /64 the same arithmetic returns a number with
-- no meaning, and a function that silently returns nonsense for half its inputs is worse
-- than one that is never reached with them. IPv6 discovery is not a sweep — the address
-- space forbids it — and will arrive as neighbour-walking only.
CREATE FUNCTION discovery_address_count(ranges cidr[]) RETURNS bigint
LANGUAGE sql IMMUTABLE STRICT AS $$
    SELECT coalesce(sum((2::numeric) ^ (32 - masklen(r))), 0)::bigint
      FROM unnest(ranges) AS r
     WHERE family(r) = 4
$$;

-- The widest range in the array, as a prefix length. 16 for a /16, 8 for a /8.
CREATE FUNCTION discovery_widest_prefix(ranges cidr[]) RETURNS int
LANGUAGE sql IMMUTABLE STRICT AS $$
    SELECT coalesce(min(masklen(r)), 32) FROM unnest(ranges) AS r
$$;

-- Whether every range is IPv4. See `discovery_address_count`.
CREATE FUNCTION discovery_ranges_are_v4(ranges cidr[]) RETURNS boolean
LANGUAGE sql IMMUTABLE STRICT AS $$
    SELECT coalesce(bool_and(family(r) = 4), false) FROM unnest(ranges) AS r
$$;

-- ----------------------------------------------------------------------------
-- Jobs
-- ----------------------------------------------------------------------------

CREATE TABLE discovery_job (
    id              uuid        PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id       uuid        NOT NULL REFERENCES tenant (id) ON DELETE CASCADE,

    name            text        NOT NULL,
    description     text        NOT NULL DEFAULT '',

    -- What to scan. `cidr` rather than text: the type refuses 10.0.0.300/24 and
    -- 10.0.0.1/24 (host bits set) at insert, so no code downstream has to decide what a
    -- malformed range means.
    ranges          cidr[]      NOT NULL,

    -- Which site the resources this job creates belong to. Discovery cannot infer a site
    -- from an address — the same RFC 1918 range is in use in every building on earth —
    -- and a resource with no site is one that no dashboard filtered by site will show.
    site_id         uuid,

    -- The credentials this job may use, in order, most-likely first.
    --
    -- §2.2: *"credentials are supplied, never guessed."* This column is the whole of what
    -- a run is permitted to try. There is no wordlist, no default, and no fallback: an
    -- empty array means the job probes nothing, which is why the CHECK below refuses it
    -- rather than letting it silently scan with no credential and report an empty estate.
    credential_refs uuid[]      NOT NULL,

    -- Where the agent listens. Per job rather than per range, because an operator who has
    -- moved SNMP off 161 has moved all of it.
    snmp_port       int         NOT NULL DEFAULT 161,

    -- Ping first, and skip what does not answer.
    --
    -- Off by default and named for what it costs. A device that drops ICMP while
    -- answering SNMP is common — the default on several firewall platforms — so this
    -- trades completeness for speed on a large range, and the operator should be the one
    -- making that trade.
    skip_silent_hosts boolean   NOT NULL DEFAULT false,

    -- NULL means manual only. A job that is only ever run by hand is a normal thing to
    -- want — a one-off sweep of a newly acquired site — and expressing it as an interval
    -- of infinity would put a row in the scheduler that never fires.
    schedule        interval,
    enabled         boolean     NOT NULL DEFAULT true,

    last_run_at     timestamptz,

    created_by      uuid        REFERENCES app_user (id),
    created_at      timestamptz NOT NULL DEFAULT now(),
    updated_at      timestamptz NOT NULL DEFAULT now(),

    CONSTRAINT discovery_job_name_is_unique_in_the_tenant UNIQUE (tenant_id, name),
    -- The composite-FK target, so a run cannot hold a job from another tenant. Same
    -- pattern as every intra-tenant reference since migration 0002.
    UNIQUE (id, tenant_id),
    FOREIGN KEY (site_id, tenant_id) REFERENCES site (id, tenant_id),

    CONSTRAINT discovery_job_name_not_blank CHECK (length(trim(name)) > 0),

    CONSTRAINT discovery_job_has_a_range CHECK (cardinality(ranges) > 0),

    -- IPv4 only. See `discovery_address_count`.
    CONSTRAINT discovery_job_ranges_are_ipv4 CHECK (discovery_ranges_are_v4(ranges)),

    -- A /16 is the largest single range. An operator who types a /8 means something
    -- else — usually "everything", which is not a range, it is a wish — and a sweep that
    -- accepted it would spend a fortnight and 16 million packets finding out.
    CONSTRAINT discovery_job_no_range_wider_than_a_16 CHECK (
        discovery_widest_prefix(ranges) >= 16
    ),

    -- And 65 536 addresses across all of them, so that ten /16s are refused for the same
    -- reason one /12 is. Split the job; the runs are independent.
    CONSTRAINT discovery_job_is_at_most_65536_addresses CHECK (
        discovery_address_count(ranges) <= 65536
    ),

    CONSTRAINT discovery_job_has_a_credential CHECK (cardinality(credential_refs) > 0),

    CONSTRAINT discovery_job_port_is_a_port CHECK (snmp_port BETWEEN 1 AND 65535),

    -- A floor, because a sweep is not a poll: the estate does not change every minute,
    -- and a job scanning more often than hourly is generating traffic a security team
    -- will ask about without learning anything. The ceiling is a month.
    CONSTRAINT discovery_job_schedule_is_sane CHECK (
        schedule IS NULL
        OR schedule BETWEEN interval '1 hour' AND interval '30 days'
    )
);

CREATE TRIGGER discovery_job_set_updated_at
    BEFORE UPDATE ON discovery_job
    FOR EACH ROW EXECUTE FUNCTION set_updated_at();

-- The scheduler's question: which enabled, scheduled jobs are due?
CREATE INDEX discovery_job_due_idx
    ON discovery_job (tenant_id, last_run_at)
 WHERE enabled AND schedule IS NOT NULL;

-- FK-index guard: `site_id` is the referencing side of a composite FK, so deleting a
-- site must not sequential-scan this table.
CREATE INDEX discovery_job_site_idx ON discovery_job (site_id, tenant_id);
-- And for `created_by`. Migration 0010 explains why every foreign key needs one.
CREATE INDEX discovery_job_created_by_idx ON discovery_job (created_by);

-- ----------------------------------------------------------------------------
-- Runs
-- ----------------------------------------------------------------------------
--
-- Append-only, and the reason is §2.7: *"scanning a network is a sensitive operation —
-- it is the thing a customer's security team will ask about first."* This is the row
-- that answers "who scanned 10.0.0.0/16 on Tuesday", alongside the `audit_log` entry.

CREATE TABLE discovery_run (
    id           uuid        PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id    uuid        NOT NULL REFERENCES tenant (id) ON DELETE CASCADE,

    -- NULL for an ad-hoc run — an operator probing one address from the candidate list,
    -- which has no job behind it. ON DELETE SET NULL rather than CASCADE: deleting a job
    -- must not delete the record that it once scanned somebody's network.
    job_id       uuid,

    -- What was actually scanned, snapshotted. Not a join to `discovery_job.ranges`,
    -- because that column is editable and the audit question is about the ranges as they
    -- were at the time, not as they are now.
    ranges       cidr[]      NOT NULL,

    trigger      text        NOT NULL,
    started_by   uuid        REFERENCES app_user (id),
    started_at   timestamptz NOT NULL DEFAULT now(),
    finished_at  timestamptz,
    status       text        NOT NULL DEFAULT 'running',
    -- Set when status is 'failed'. The sentence an operator reads, not a stack trace.
    error        text,

    -- What it did. Counters rather than a join, because the run list is the first screen
    -- and counting candidates per run across a year of runs to draw it would not do.
    --
    -- `probed` and `answered` differ by the silent hosts, which is the number that tells
    -- an operator whether their credential list is right: 4 000 probed and 0 answered is
    -- a wrong community string, not an empty network.
    probed       int         NOT NULL DEFAULT 0,
    answered     int         NOT NULL DEFAULT 0,
    -- Resources created, and existing resources this run resolved onto.
    created      int         NOT NULL DEFAULT 0,
    merged       int         NOT NULL DEFAULT 0,
    -- Sent to the identity review queue: neither confident enough to merge nor different
    -- enough to be new. §2.4.
    for_review   int         NOT NULL DEFAULT 0,
    -- Left in `discovery_candidate`.
    candidates   int         NOT NULL DEFAULT 0,
    -- `connected_to` edges written. §2.6.
    edges        int         NOT NULL DEFAULT 0,

    UNIQUE (id, tenant_id),
    -- SET NULL naming the column, because a composite FK defaults to nulling *every*
    -- column in the key -- including tenant_id, which is NOT NULL. Deleting a job would
    -- have failed with a not-null violation on a table the operator was not looking at.
    -- The column list is PostgreSQL 15; PLAN's floor is 16.
    FOREIGN KEY (job_id, tenant_id) REFERENCES discovery_job (id, tenant_id)
        ON DELETE SET NULL (job_id),

    CONSTRAINT discovery_run_status_is_known CHECK (
        status IN ('running', 'succeeded', 'failed', 'cancelled')
    ),
    CONSTRAINT discovery_run_trigger_is_known CHECK (
        trigger IN ('schedule', 'manual', 'probe')
    ),
    -- A finished run has an end, and a running one does not. The pair is what makes
    -- "runs that are stuck" answerable without a heuristic about age.
    CONSTRAINT discovery_run_finishes_iff_it_is_over CHECK (
        (status = 'running') = (finished_at IS NULL)
    ),
    CONSTRAINT discovery_run_error_iff_failed CHECK (
        (status = 'failed') = (error IS NOT NULL)
    ),
    CONSTRAINT discovery_run_counts_are_not_negative CHECK (
        least(probed, answered, created, merged, for_review, candidates, edges) >= 0
    ),
    -- Nothing can answer that was not probed. Catches a counter incremented on the wrong
    -- path, which is the kind of bug that makes an operator distrust the whole screen.
    CONSTRAINT discovery_run_answered_at_most_probed CHECK (answered <= probed)
);

-- The run list, newest first, and the FK index for `job_id`.
CREATE INDEX discovery_run_recent_idx ON discovery_run (tenant_id, started_at DESC);
CREATE INDEX discovery_run_job_idx ON discovery_run (job_id, tenant_id);
CREATE INDEX discovery_run_started_by_idx ON discovery_run (started_by);

-- ----------------------------------------------------------------------------
-- Candidates
-- ----------------------------------------------------------------------------
--
-- Everything a run found and did not turn into a resource: an address that answered but
-- could not be identified, an LLDP neighbour with no matching resource (§2.5), an
-- address that answered nothing with the supplied credentials (§2.2).
--
-- One row per *thing*, not per sighting. A nightly sweep over a /22 that finds the same
-- 40 unidentifiable printers would otherwise write 14 600 rows a year about 40 printers,
-- and the screen an operator is supposed to work through would be a log.

CREATE TABLE discovery_candidate (
    id            uuid        PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id     uuid        NOT NULL REFERENCES tenant (id) ON DELETE CASCADE,

    -- The run that *last* saw it. Deleting a run does not delete what it found, for the
    -- same reason deleting a job does not delete its runs.
    last_run_id   uuid,

    source        text        NOT NULL,

    -- The management address, if there is one. NULL is legitimate and common: an LLDP
    -- neighbour reports a chassis ID and a port, and many platforms report no management
    -- address at all — which is precisely why §2.5 refuses to invent a resource from
    -- one. A candidate with no address is a thing that exists and cannot yet be reached.
    address       inet,

    -- What the probe or the neighbour table said. All nullable: which of these is present
    -- is exactly what distinguishes a sweep candidate from an LLDP one, and a NOT NULL
    -- here would force one of them to store a lie.
    chassis_id    text,
    port_id       text,
    platform      text,
    sys_name      text,
    sys_descr     text,
    sys_object_id text,
    -- Six bytes, from ARP. `macaddr` rather than text so 00:1b:21:3c:4d:5e and
    -- 001b.213c.4d5e — the same address, spelled the two ways two vendors spell it — are
    -- one value rather than two candidates.
    mac           macaddr,

    -- The resource whose neighbour table this came from. NULL for a sweep. This is what
    -- makes "where did this come from" answerable, and it is the answer an operator needs
    -- before deciding whether to trust an unexpected device.
    seen_from     uuid,

    state         text        NOT NULL DEFAULT 'unidentified',
    -- Set when state is 'promoted': what this candidate became.
    resource_id   uuid,
    -- Why it is still here, in a sentence. 'no credential in the job was accepted', 'two
    -- resources match this hostname'. Shown in the list, because a candidate with no
    -- reason is one an operator can only shrug at.
    reason        text        NOT NULL DEFAULT '',

    first_seen    timestamptz NOT NULL DEFAULT now(),
    last_seen     timestamptz NOT NULL DEFAULT now(),
    -- An operator saying "I know, it is a printer, stop showing me". Distinct from
    -- deleting the row, which the next run would simply re-create.
    ignored_at    timestamptz,
    ignored_by    uuid        REFERENCES app_user (id),

    -- The deduplication key, derived rather than supplied.
    --
    -- A generated column rather than `UNIQUE NULLS NOT DISTINCT` on (address,
    -- chassis_id), which this PostgreSQL does support: that index would make the *pair*
    -- the identity, so the same device seen once by sweep (address, no chassis) and once
    -- by LLDP (chassis, no address) is two rows either way, while the column also gives
    -- the API one value to quote back at an operator. Collapsing those two rows is
    -- identity resolution's job, not a unique index's, and this key is deliberately only
    -- strong enough to stop a run duplicating its own findings.
    fingerprint   text        GENERATED ALWAYS AS (
        source || '|' || coalesce(host(address), '') || '|' || coalesce(chassis_id, '')
    ) STORED,

    -- Column-list SET NULL, for the reason discovery_run.job_id gives.
    FOREIGN KEY (last_run_id, tenant_id) REFERENCES discovery_run (id, tenant_id)
        ON DELETE SET NULL (last_run_id),
    FOREIGN KEY (seen_from, tenant_id) REFERENCES resource (id, tenant_id) ON DELETE CASCADE,
    FOREIGN KEY (resource_id, tenant_id) REFERENCES resource (id, tenant_id)
        ON DELETE SET NULL (resource_id),

    CONSTRAINT discovery_candidate_is_one_per_thing UNIQUE (tenant_id, fingerprint),
    CONSTRAINT discovery_candidate_source_is_known CHECK (
        source IN ('sweep', 'lldp', 'cdp', 'arp')
    ),
    CONSTRAINT discovery_candidate_state_is_known CHECK (
        state IN ('unidentified', 'ambiguous', 'unreachable', 'promoted', 'ignored')
    ),
    -- A candidate with neither an address nor a chassis ID is not a candidate, it is an
    -- empty row — and its fingerprint would collide with every other empty row, which is
    -- a confusing way to find out.
    CONSTRAINT discovery_candidate_is_addressable CHECK (
        address IS NOT NULL OR chassis_id IS NOT NULL
    ),
    CONSTRAINT discovery_candidate_promoted_iff_resource CHECK (
        (state = 'promoted') = (resource_id IS NOT NULL)
    ),
    CONSTRAINT discovery_candidate_ignored_iff_marked CHECK (
        (state = 'ignored') = (ignored_at IS NOT NULL)
    ),
    -- Only a neighbour has a port, and only a neighbour was seen from somewhere.
    CONSTRAINT discovery_candidate_sweep_has_no_neighbour_fields CHECK (
        source <> 'sweep' OR (port_id IS NULL AND seen_from IS NULL)
    )
);

-- The screen: this tenant's outstanding candidates, newest first. Partial, because
-- promoted and ignored rows are history and are the bulk of the table after a month.
CREATE INDEX discovery_candidate_outstanding_idx
    ON discovery_candidate (tenant_id, last_seen DESC)
 WHERE state IN ('unidentified', 'ambiguous', 'unreachable');

-- FK indexes, so that deleting a run or a resource does not scan this table.
CREATE INDEX discovery_candidate_run_idx ON discovery_candidate (last_run_id, tenant_id);
CREATE INDEX discovery_candidate_seen_from_idx ON discovery_candidate (seen_from, tenant_id);
CREATE INDEX discovery_candidate_resource_idx ON discovery_candidate (resource_id, tenant_id);
CREATE INDEX discovery_candidate_ignored_by_idx ON discovery_candidate (ignored_by);
