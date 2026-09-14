-- 0005 — sealed credentials and the access log. SPEC §M0.4.
--
-- This platform holds SNMPv3 auth/priv keys, SSH private keys and API tokens for a
-- customer's entire network. A leak here is a full network compromise, not a data
-- breach.
--
-- The columns below are the persisted form of `uops_secrets::SealedCredential`. Nothing
-- in this table is usable without the KEK, and the KEK never enters the database.

CREATE TABLE credential (
    id          uuid        PRIMARY KEY,
    tenant_id   uuid        NOT NULL REFERENCES tenant (id),
    name        text        NOT NULL,
    kind        text        NOT NULL CHECK (kind IN (
                                'snmp_community', 'snmpv3',
                                'ssh_password', 'ssh_key', 'api_token')),
    -- Rotation writes a new row rather than overwriting one. Collectors read the
    -- highest non-revoked version, so a rotation that turns out to be wrong is undone
    -- by revoking a row, not by restoring a backup.
    version     int         NOT NULL DEFAULT 1 CHECK (version >= 1),

    -- Which KEK wrapped this DEK. Rotation finds rows by this column.
    kek_id      text        NOT NULL,
    -- The DEK, encrypted under the KEK. 32 bytes of key, plus nonce and tag.
    wrapped_dek bytea       NOT NULL,
    dek_nonce   bytea       NOT NULL,

    -- The credential material, encrypted under the DEK.
    ciphertext  bytea       NOT NULL,
    nonce       bytea       NOT NULL,

    -- Which crypto build sealed these bytes ('rustcrypto' | 'aws-lc-fips'). Required by
    -- the M0.4 obligations: a re-key after a backend change has to be detectable, and
    -- an auditor has to be able to see what actually produced the ciphertext rather
    -- than what the deployment claims to be running.
    backend_id  text        NOT NULL,

    created_at  timestamptz NOT NULL DEFAULT now(),
    revoked_at  timestamptz,

    UNIQUE (tenant_id, name, version),
    UNIQUE (id, tenant_id)
);

-- NOTE — the SPEC §M0.4 sketch also lists an `aad bytea` column. It is deliberately
-- NOT here.
--
-- The AAD is `tenant_id || credential_id || version`, length-prefixed and
-- domain-separated, and `uops_secrets` *derives* it from the row's own identity at
-- decrypt time. Storing it would make it an input rather than a binding: an attacker
-- with database write access could transplant a ciphertext into another tenant and
-- store the matching AAD alongside it, and decryption would succeed. Deriving it means
-- that same attacker gets an authentication failure, because the identity the row now
-- claims no longer matches the identity the bytes were sealed under.
--
-- `uops-secrets` has a test for exactly that attack. A stored `aad` column would make
-- the test pass and the property false.

-- How collectors resolve a credential: highest live version for a name.
CREATE INDEX credential_latest_idx
    ON credential (tenant_id, name, version DESC)
    WHERE revoked_at IS NULL;

-- How KEK rotation finds what it still has to re-wrap. Partial, because rotation only
-- ever cares about live rows.
CREATE INDEX credential_kek_idx
    ON credential (kek_id)
    WHERE revoked_at IS NULL;

-- A resource's credential must belong to the same tenant as the resource. Added here
-- rather than in 0002 because the target table did not exist yet.
ALTER TABLE resource
    ADD CONSTRAINT resource_credential_ref_fkey
    FOREIGN KEY (credential_ref, tenant_id) REFERENCES credential (id, tenant_id);

-- Every get() writes a row here, including failures. A credential store that logs only
-- successful reads cannot answer the question that actually gets asked after an
-- incident: what was tried, and what was refused.
CREATE TABLE credential_access_log (
    id            bigserial   PRIMARY KEY,
    tenant_id     uuid        NOT NULL,
    credential_id uuid        NOT NULL,
    -- 'user:<uuid>' | 'collector' | 'system', from Actor::as_audit_str.
    actor         text        NOT NULL,
    -- What it was being used for, when known.
    resource_id   uuid,
    -- 'snmp-poll' | 'ssh-runbook' | 'config-backup'.
    purpose       text        NOT NULL,
    succeeded     bool        NOT NULL,
    at            timestamptz NOT NULL DEFAULT now()
);

-- No foreign key to `credential` on purpose: the log outlives what it describes. A
-- deleted credential must not take the record of who used it with it, and a
-- cascade here would quietly delete exactly the rows an investigation needs.

CREATE INDEX credential_access_log_credential_idx
    ON credential_access_log (tenant_id, credential_id, at DESC);
-- "What did this collector touch, and when" — the other question incidents ask.
CREATE INDEX credential_access_log_actor_idx
    ON credential_access_log (tenant_id, actor, at DESC);
