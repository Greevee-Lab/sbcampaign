-- SPDX-License-Identifier: AGPL-3.0-only
--
-- Players and identities (steelcore ADR-0002).
--
-- A player owns pilot, garage, awards and leaderboard rows. A player is known
-- by one or more identities, one per provider account, for example
-- (steelbound, <account id>) or (insignia, <XUID>).
--
-- player_id is the existing account UUID (accounts.xuid), so no campaign row
-- changes:
--   * insignia:   player_id = the XUID as UUID, exactly as the campaign server
--                 derives it today: lpad(to_hex(xuid), 32, '0')::uuid
--   * steelbound: player_id = the same derivation applied to the pseudo-XUID
--                 0x53424C01_<account id> (account id 1 .. 2^32-1)
--
-- Additive only: upstream tables are not altered. The account row is the
-- player's campaign record; player and identity are added next to it.
-- Applied once by deploy/server/migrate.sh (sql/fork/migrations.list).

CREATE TABLE provider (
    provider_id         text        PRIMARY KEY CHECK (provider_id ~ '^[a-z][a-z0-9_]{1,31}$'),
    display_name        text        NOT NULL,
    -- How the player_id of a new identity is derived from its external id.
    player_id_rule      text        NOT NULL CHECK (player_id_rule IN ('xuid', 'pseudo_xuid')),
    -- High 32 bits of the pseudo-XUID range ('pseudo_xuid' providers only).
    pseudo_xuid_prefix  bigint      CHECK (pseudo_xuid_prefix BETWEEN 1 AND 2147483647),
    trust_level         smallint    NOT NULL DEFAULT 0,
    enabled             boolean     NOT NULL DEFAULT false,
    created_at          timestamptz NOT NULL DEFAULT now(),
    CHECK ((player_id_rule = 'pseudo_xuid') = (pseudo_xuid_prefix IS NOT NULL))
);

INSERT INTO provider (provider_id, display_name, player_id_rule, pseudo_xuid_prefix, trust_level, enabled) VALUES
    ('steelbound', 'Steelbound',     'pseudo_xuid', 1396853761, 1, true),   -- 0x53424C01
    ('insignia',   'Insignia (XUID)', 'xuid',        NULL,      1, false);  -- prepared, no gateway yet

CREATE TABLE player (
    player_id   uuid        PRIMARY KEY REFERENCES accounts (xuid),
    created_at  timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE identity (
    identity_id   uuid        PRIMARY KEY DEFAULT gen_random_uuid(),
    provider_id   text        NOT NULL REFERENCES provider (provider_id),
    external_id   text        NOT NULL CHECK (external_id ~ '^[0-9a-f]{1,16}$|^[1-9][0-9]{0,9}$'),
    player_id     uuid        NOT NULL REFERENCES player (player_id),
    verified_by   text        NOT NULL CHECK (verified_by IN ('backfill', 'first_login', 'ticket', 'link_code', 'admin')),
    created_at    timestamptz NOT NULL DEFAULT now(),
    last_seen_at  timestamptz NOT NULL DEFAULT now(),
    UNIQUE (provider_id, external_id)
);

CREATE INDEX identity_player_idx ON identity (player_id);

-- One row per identity change and, later, per write command of a gateway:
-- who (identity/player), what (action), when. Never game input.
CREATE TABLE audit_log (
    audit_id     bigint      GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    at           timestamptz NOT NULL DEFAULT now(),
    source       text        NOT NULL,
    action       text        NOT NULL,
    identity_id  uuid        REFERENCES identity (identity_id),
    player_id    uuid        REFERENCES player (player_id),
    detail       text
);

CREATE INDEX audit_log_player_idx ON audit_log (player_id, at);

-- Backfill: every existing account becomes a player with one identity.
-- Accounts in the steelbound pseudo-XUID range get (steelbound, <account id>),
-- all others (insignia, <XUID as 16 lowercase hex digits>).
INSERT INTO player (player_id)
SELECT xuid FROM accounts
ON CONFLICT DO NOTHING;

INSERT INTO identity (provider_id, external_id, player_id, verified_by)
SELECT
    CASE WHEN x >> 32 = 1396853761 AND (x & 4294967295) <> 0 THEN 'steelbound' ELSE 'insignia' END,
    CASE WHEN x >> 32 = 1396853761 AND (x & 4294967295) <> 0
         THEN (x & 4294967295)::text
         ELSE lpad(to_hex(x), 16, '0') END,
    xuid,
    'backfill'
FROM (
    -- Accounts keyed by a 64-bit value (upper 64 bits of the UUID are zero).
    SELECT xuid, ('x' || right(replace(xuid::text, '-', ''), 16))::bit(64)::bigint AS x
    FROM accounts
    WHERE left(replace(xuid::text, '-', ''), 16) = '0000000000000000'
) a
ON CONFLICT (provider_id, external_id) DO NOTHING;

INSERT INTO audit_log (source, action, identity_id, player_id, detail)
SELECT 'migration', 'identity_backfill', identity_id, player_id, provider_id
FROM identity WHERE verified_by = 'backfill';
