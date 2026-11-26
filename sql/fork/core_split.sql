-- SPDX-License-Identifier: AGPL-3.0-only
--
-- Core split: players, identities, display names, provider accesses,
-- idempotency keys and the Core API audit move from the campaign tables
-- (schema public) into their own schema "core".
--
-- Why: a season reset empties the campaign (pilots, garage, shop, results,
-- leaderboard, turns). Players and their display names belong to the core and
-- must survive it. After this migration the campaign tables can be emptied
-- (deploy/server/maint.sh reset) without touching a single core row.
--
-- Rules after the split:
--   * schema core holds the core, schema public the campaign of this instance;
--   * the campaign refers to the core only by value (player_id = accounts.xuid,
--     identity_id in gateway_result and audit_log); there is no foreign key
--     between the two schemas, so either side can be emptied, restored or
--     moved to its own database on its own;
--   * audit rows of the core (identity created/backfilled, Core API writes)
--     move to core.audit_log; audit rows of the game (gateway writes,
--     annulments) stay in public.audit_log.
--
-- Takeover of existing data, all in the one transaction migrate.sh opens:
--   1. create schema core and its tables,
--   2. copy every core table, then compare row count and an md5 over all rows
--      (ordered by primary key) between source and copy; any difference
--      aborts the transaction and nothing changes,
--   3. record the checked values in core.split_report,
--   4. only then drop the foreign keys from campaign tables to the old core
--      tables, delete the moved audit rows and drop the old core tables.
--
-- Idempotent: recorded once in fork_migrations; on a database that is already
-- split it does nothing. Way back: sql/fork/core_unsplit.sql (see
-- deploy/README.md, "Core split").

DO $takeover$
DECLARE
    t            record;
    c            record;
    src_n        bigint;
    dst_n        bigint;
    src_h        text;
    dst_h        text;
    audit_total  bigint;
    audit_stays  bigint;
    audit_max    bigint;
BEGIN
    IF to_regclass('public.player') IS NULL THEN
        IF to_regclass('core.player') IS NOT NULL THEN
            RAISE NOTICE 'core split: already split, nothing to do';
            RETURN;
        END IF;
        RAISE EXCEPTION 'core split: neither public.player nor core.player exists (apply fork/identity.sql first)';
    END IF;
    IF to_regnamespace('core') IS NOT NULL THEN
        RAISE EXCEPTION 'core split: schema core exists next to public.player; resolve by hand (deploy/README.md, "Core split")';
    END IF;

    CREATE SCHEMA core;

    -- Same definitions as fork/identity.sql and fork/core_api.sql, except that
    -- core.player no longer references the campaign table accounts.
    CREATE TABLE core.provider (
        provider_id         text        PRIMARY KEY CHECK (provider_id ~ '^[a-z][a-z0-9_]{1,31}$'),
        display_name        text        NOT NULL,
        player_id_rule      text        NOT NULL CHECK (player_id_rule IN ('xuid', 'pseudo_xuid')),
        pseudo_xuid_prefix  bigint      CHECK (pseudo_xuid_prefix BETWEEN 1 AND 2147483647),
        trust_level         smallint    NOT NULL DEFAULT 0,
        enabled             boolean     NOT NULL DEFAULT false,
        created_at          timestamptz NOT NULL DEFAULT now(),
        CHECK ((player_id_rule = 'pseudo_xuid') = (pseudo_xuid_prefix IS NOT NULL))
    );

    CREATE TABLE core.player (
        player_id   uuid        PRIMARY KEY,
        created_at  timestamptz NOT NULL DEFAULT now()
    );

    CREATE TABLE core.identity (
        identity_id   uuid        PRIMARY KEY DEFAULT gen_random_uuid(),
        provider_id   text        NOT NULL REFERENCES core.provider (provider_id),
        external_id   text        NOT NULL CHECK (external_id ~ '^[0-9a-f]{1,16}$|^[1-9][0-9]{0,9}$'),
        player_id     uuid        NOT NULL REFERENCES core.player (player_id),
        verified_by   text        NOT NULL CHECK (verified_by IN ('backfill', 'first_login', 'ticket', 'link_code', 'admin')),
        created_at    timestamptz NOT NULL DEFAULT now(),
        last_seen_at  timestamptz NOT NULL DEFAULT now(),
        suspended     boolean     NOT NULL DEFAULT false,
        UNIQUE (provider_id, external_id)
    );
    CREATE INDEX identity_player_idx ON core.identity (player_id);

    CREATE TABLE core.audit_log (
        audit_id     bigint      GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
        at           timestamptz NOT NULL DEFAULT now(),
        source       text        NOT NULL,
        action       text        NOT NULL,
        identity_id  uuid        REFERENCES core.identity (identity_id),
        player_id    uuid        REFERENCES core.player (player_id),
        detail       text,
        access_id    text,
        provider_id  text,
        method       text,
        outcome      text
    );
    CREATE INDEX audit_log_player_idx ON core.audit_log (player_id, at);
    CREATE INDEX audit_log_access_idx ON core.audit_log (access_id, at);

    CREATE TABLE core.display_name (
        player_id   uuid        PRIMARY KEY REFERENCES core.player (player_id),
        name        text        NOT NULL CHECK (char_length(name) BETWEEN 1 AND 15),
        name_key    text        NOT NULL UNIQUE,
        skeleton    text        NOT NULL UNIQUE,
        changed_at  timestamptz NOT NULL DEFAULT now()
    );

    CREATE TABLE core.display_name_reservation (
        player_id   uuid        NOT NULL REFERENCES core.player (player_id),
        name_key    text        NOT NULL,
        skeleton    text        NOT NULL,
        until       timestamptz NOT NULL,
        PRIMARY KEY (player_id, name_key)
    );
    CREATE INDEX display_name_reservation_skeleton_idx ON core.display_name_reservation (skeleton);

    CREATE TABLE core.display_name_blocklist (
        term      text    PRIMARY KEY CHECK (term ~ '^[a-z0-9 ]{2,15}$'),
        contains  boolean NOT NULL DEFAULT true
    );

    CREATE TABLE core.api_access (
        access_id     text        PRIMARY KEY CHECK (access_id ~ '^[a-z0-9][a-z0-9_.-]{1,63}$'),
        provider_id   text        REFERENCES core.provider (provider_id),
        cert_sha256   text        NOT NULL CHECK (cert_sha256 ~ '^[0-9a-f]{64}$'),
        scopes        text[]      NOT NULL,
        first_seen_at timestamptz NOT NULL DEFAULT now(),
        updated_at    timestamptz NOT NULL DEFAULT now(),
        revoked_at    timestamptz
    );

    CREATE TABLE core.api_idempotency (
        access_id     text        NOT NULL,
        key           text        NOT NULL,
        method        text        NOT NULL,
        request_hash  bytea       NOT NULL,
        response      bytea,
        created_at    timestamptz NOT NULL DEFAULT now(),
        PRIMARY KEY (access_id, key)
    );
    CREATE INDEX api_idempotency_created_idx ON core.api_idempotency (created_at);

    -- The values the takeover checked (kept as evidence; small).
    CREATE TABLE core.split_report (
        name   text        PRIMARY KEY,
        rows   bigint      NOT NULL,
        md5    text        NOT NULL,
        at     timestamptz NOT NULL DEFAULT now()
    );

    SELECT count(*), max(audit_id) INTO audit_total, audit_max FROM public.audit_log;

    -- Copy and compare, in foreign key order.
    FOR t IN SELECT * FROM (VALUES
        (1, 'provider', 'provider_id, display_name, player_id_rule, pseudo_xuid_prefix, trust_level, enabled, created_at',
            'provider_id', 'true'),
        (2, 'player', 'player_id, created_at', 'player_id', 'true'),
        (3, 'identity', 'identity_id, provider_id, external_id, player_id, verified_by, created_at, last_seen_at, suspended',
            'identity_id', 'true'),
        (4, 'audit_log', 'audit_id, at, source, action, identity_id, player_id, detail, access_id, provider_id, method, outcome',
            'audit_id', 'action NOT IN (''game_write'', ''results_annulled'')'),
        (5, 'display_name', 'player_id, name, name_key, skeleton, changed_at', 'player_id', 'true'),
        (6, 'display_name_reservation', 'player_id, name_key, skeleton, until', 'player_id, name_key', 'true'),
        (7, 'display_name_blocklist', 'term, contains', 'term', 'true'),
        (8, 'api_access', 'access_id, provider_id, cert_sha256, scopes, first_seen_at, updated_at, revoked_at',
            'access_id', 'true'),
        (9, 'api_idempotency', 'access_id, key, method, request_hash, response, created_at', 'access_id, key', 'true')
    ) AS v(ord, name, cols, pk, filter) ORDER BY ord LOOP
        EXECUTE format('INSERT INTO core.%I (%s) %s SELECT %s FROM public.%I WHERE %s',
                       t.name, t.cols,
                       CASE WHEN t.name = 'audit_log' THEN 'OVERRIDING SYSTEM VALUE' ELSE '' END,
                       t.cols, t.name, t.filter);
        EXECUTE format('SELECT count(*), md5(coalesce(string_agg(ROW(%s)::text, chr(10) ORDER BY %s), '''')) FROM public.%I WHERE %s',
                       t.cols, t.pk, t.name, t.filter) INTO src_n, src_h;
        EXECUTE format('SELECT count(*), md5(coalesce(string_agg(ROW(%s)::text, chr(10) ORDER BY %s), '''')) FROM core.%I',
                       t.cols, t.pk, t.name) INTO dst_n, dst_h;
        IF src_n <> dst_n OR src_h <> dst_h THEN
            RAISE EXCEPTION 'core split: % differs after copy (rows % -> %, md5 % -> %)', t.name, src_n, dst_n, src_h, dst_h;
        END IF;
        INSERT INTO core.split_report (name, rows, md5) VALUES (t.name, dst_n, dst_h);
    END LOOP;

    -- Audit rows: every row is either moved or stays.
    SELECT count(*) INTO audit_stays FROM public.audit_log WHERE action IN ('game_write', 'results_annulled');
    SELECT rows INTO dst_n FROM core.split_report WHERE name = 'audit_log';
    IF audit_stays + dst_n <> audit_total THEN
        RAISE EXCEPTION 'core split: audit rows do not add up (% + % <> %)', dst_n, audit_stays, audit_total;
    END IF;
    INSERT INTO core.split_report (name, rows, md5)
    SELECT 'campaign audit_log (stays)', count(*),
           md5(coalesce(string_agg(ROW(audit_id, at, source, action, identity_id, player_id, detail,
                                        access_id, provider_id, method, outcome)::text, chr(10) ORDER BY audit_id), ''))
    FROM public.audit_log WHERE action IN ('game_write', 'results_annulled');

    -- New core audit ids continue after the highest id so far.
    IF audit_max IS NOT NULL THEN
        PERFORM setval(pg_get_serial_sequence('core.audit_log', 'audit_id'), audit_max, true);
    END IF;

    -- Only now remove the old core tables from the campaign: first every
    -- foreign key of another table that points at them (gateway_result,
    -- audit_log), then the moved audit rows, then the tables themselves
    -- (without CASCADE: anything unexpected still depending on them aborts).
    FOR c IN
        SELECT con.conrelid::regclass AS tbl, con.conname
        FROM pg_constraint con
        WHERE con.contype = 'f'
          AND con.confrelid IN ('public.provider'::regclass, 'public.player'::regclass, 'public.identity'::regclass)
          AND con.conrelid NOT IN ('public.provider'::regclass, 'public.player'::regclass, 'public.identity'::regclass,
                                   'public.display_name'::regclass, 'public.display_name_reservation'::regclass,
                                   'public.api_access'::regclass)
    LOOP
        EXECUTE format('ALTER TABLE %s DROP CONSTRAINT %I', c.tbl, c.conname);
    END LOOP;

    DELETE FROM public.audit_log WHERE action NOT IN ('game_write', 'results_annulled');

    DROP TABLE public.api_idempotency, public.api_access, public.display_name_blocklist,
               public.display_name_reservation, public.display_name, public.identity,
               public.player, public.provider;
END
$takeover$;
