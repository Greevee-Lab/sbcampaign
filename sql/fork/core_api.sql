-- SPDX-License-Identifier: AGPL-3.0-only
--
-- Core API (steelcore.v1): display names, provider accesses, idempotency keys,
-- write audit and identity suspension. Additive only: new tables, new
-- nullable/defaulted columns on fork tables; upstream tables are not altered.
-- Applied once by deploy/server/migrate.sh (sql/fork/migrations.list).

-- The player's display name (gamertag): unique across all providers, also
-- by its look-alike skeleton (rules in src/core_api/names.rs).
CREATE TABLE display_name (
    player_id   uuid        PRIMARY KEY REFERENCES player (player_id),
    name        text        NOT NULL CHECK (char_length(name) BETWEEN 1 AND 15),
    name_key    text        NOT NULL UNIQUE,
    skeleton    text        NOT NULL UNIQUE,
    changed_at  timestamptz NOT NULL DEFAULT now()
);

-- A given-up name stays reserved for its previous owner for a while.
CREATE TABLE display_name_reservation (
    player_id   uuid        NOT NULL REFERENCES player (player_id),
    name_key    text        NOT NULL,
    skeleton    text        NOT NULL,
    until       timestamptz NOT NULL,
    PRIMARY KEY (player_id, name_key)
);

CREATE INDEX display_name_reservation_skeleton_idx ON display_name_reservation (skeleton);

-- Terms no display name may be or (contains = true) contain. Compared on the
-- look-alike skeleton, so "Adm1n" matches "admin". Operators add more rows.
CREATE TABLE display_name_blocklist (
    term      text    PRIMARY KEY CHECK (term ~ '^[a-z0-9 ]{2,15}$'),
    contains  boolean NOT NULL DEFAULT true
);

INSERT INTO display_name_blocklist (term, contains) VALUES
    ('admin', true), ('moderator', true), ('steelcore', true), ('steelbound', true),
    ('official', true), ('gamemaster', true), ('system', false), ('server', false),
    ('support', false), ('everyone', false), ('here', false);

-- Provider accesses of the Core API, mirrored from the configuration at
-- startup (the configuration is the source). Never deleted: audit rows keep
-- referring to them by name.
CREATE TABLE api_access (
    access_id     text        PRIMARY KEY CHECK (access_id ~ '^[a-z0-9][a-z0-9_.-]{1,63}$'),
    provider_id   text        REFERENCES provider (provider_id),
    cert_sha256   text        NOT NULL CHECK (cert_sha256 ~ '^[0-9a-f]{64}$'),
    scopes        text[]      NOT NULL,
    first_seen_at timestamptz NOT NULL DEFAULT now(),
    updated_at    timestamptz NOT NULL DEFAULT now(),
    revoked_at    timestamptz
);

-- Idempotency keys of write calls (kept 24 hours).
CREATE TABLE api_idempotency (
    access_id     text        NOT NULL,
    key           text        NOT NULL,
    method        text        NOT NULL,
    request_hash  bytea       NOT NULL,
    response      bytea,
    created_at    timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (access_id, key)
);

CREATE INDEX api_idempotency_created_idx ON api_idempotency (created_at);

-- Write audit of the Core API: access, provider, method, outcome. No request
-- contents.
ALTER TABLE audit_log
    ADD COLUMN access_id   text,
    ADD COLUMN provider_id text,
    ADD COLUMN method      text,
    ADD COLUMN outcome     text;

CREATE INDEX audit_log_access_idx ON audit_log (access_id, at);

-- Suspended identities may not play (reported by ResolveIdentity).
ALTER TABLE identity ADD COLUMN suspended boolean NOT NULL DEFAULT false;
