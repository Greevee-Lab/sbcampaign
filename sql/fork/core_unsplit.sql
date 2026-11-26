-- SPDX-License-Identifier: AGPL-3.0-only
--
-- Way back from the core split (fork/core_split.sql), without losing data.
-- NOT in migrations.list: run by hand through deploy/server/maint.sh
-- ("unsplit"), with the instance stopped and right before switching back to a
-- release from before the split (deploy.sh --rollback).
--
-- Puts the core tables back into schema public exactly as fork/identity.sql
-- and fork/core_api.sql define them (including the foreign keys between
-- campaign and core), copies every row back and compares row count and md5
-- like the split did, then drops schema core and forgets the split in
-- fork_migrations, so that a later deploy of a split release splits again.
--
-- Two differences to the state before the split, both needed by the old
-- schema:
--   * every core player without a campaign account gets an empty account row
--     (public.player references public.accounts);
--   * core audit rows get new audit ids in public.audit_log (the ids of the
--     two audit tables overlap after the split); time, source, action and all
--     other columns stay, the check compares them without the id.
-- The function annul_identity_results gets its pre-split body back.

DO $unsplit$
DECLARE
    t      record;
    src_n  bigint;
    dst_n  bigint;
    src_h  text;
    dst_h  text;
BEGIN
    IF to_regclass('core.player') IS NULL THEN
        IF to_regclass('public.player') IS NOT NULL THEN
            RAISE NOTICE 'core unsplit: not split, nothing to do';
            RETURN;
        END IF;
        RAISE EXCEPTION 'core unsplit: neither core.player nor public.player exists';
    END IF;
    IF to_regclass('public.player') IS NOT NULL THEN
        RAISE EXCEPTION 'core unsplit: public.player exists next to core.player; resolve by hand';
    END IF;

    INSERT INTO public.accounts (xuid)
    SELECT player_id FROM core.player
    ON CONFLICT (xuid) DO NOTHING;

    -- fork/identity.sql
    CREATE TABLE public.provider (
        provider_id         text        PRIMARY KEY CHECK (provider_id ~ '^[a-z][a-z0-9_]{1,31}$'),
        display_name        text        NOT NULL,
        player_id_rule      text        NOT NULL CHECK (player_id_rule IN ('xuid', 'pseudo_xuid')),
        pseudo_xuid_prefix  bigint      CHECK (pseudo_xuid_prefix BETWEEN 1 AND 2147483647),
        trust_level         smallint    NOT NULL DEFAULT 0,
        enabled             boolean     NOT NULL DEFAULT false,
        created_at          timestamptz NOT NULL DEFAULT now(),
        CHECK ((player_id_rule = 'pseudo_xuid') = (pseudo_xuid_prefix IS NOT NULL))
    );
    CREATE TABLE public.player (
        player_id   uuid        PRIMARY KEY REFERENCES public.accounts (xuid),
        created_at  timestamptz NOT NULL DEFAULT now()
    );
    CREATE TABLE public.identity (
        identity_id   uuid        PRIMARY KEY DEFAULT gen_random_uuid(),
        provider_id   text        NOT NULL REFERENCES public.provider (provider_id),
        external_id   text        NOT NULL CHECK (external_id ~ '^[0-9a-f]{1,16}$|^[1-9][0-9]{0,9}$'),
        player_id     uuid        NOT NULL REFERENCES public.player (player_id),
        verified_by   text        NOT NULL CHECK (verified_by IN ('backfill', 'first_login', 'ticket', 'link_code', 'admin')),
        created_at    timestamptz NOT NULL DEFAULT now(),
        last_seen_at  timestamptz NOT NULL DEFAULT now(),
        UNIQUE (provider_id, external_id)
    );
    CREATE INDEX identity_player_idx ON public.identity (player_id);

    -- fork/core_api.sql
    CREATE TABLE public.display_name (
        player_id   uuid        PRIMARY KEY REFERENCES public.player (player_id),
        name        text        NOT NULL CHECK (char_length(name) BETWEEN 1 AND 15),
        name_key    text        NOT NULL UNIQUE,
        skeleton    text        NOT NULL UNIQUE,
        changed_at  timestamptz NOT NULL DEFAULT now()
    );
    CREATE TABLE public.display_name_reservation (
        player_id   uuid        NOT NULL REFERENCES public.player (player_id),
        name_key    text        NOT NULL,
        skeleton    text        NOT NULL,
        until       timestamptz NOT NULL,
        PRIMARY KEY (player_id, name_key)
    );
    CREATE INDEX display_name_reservation_skeleton_idx ON public.display_name_reservation (skeleton);
    CREATE TABLE public.display_name_blocklist (
        term      text    PRIMARY KEY CHECK (term ~ '^[a-z0-9 ]{2,15}$'),
        contains  boolean NOT NULL DEFAULT true
    );
    CREATE TABLE public.api_access (
        access_id     text        PRIMARY KEY CHECK (access_id ~ '^[a-z0-9][a-z0-9_.-]{1,63}$'),
        provider_id   text        REFERENCES public.provider (provider_id),
        cert_sha256   text        NOT NULL CHECK (cert_sha256 ~ '^[0-9a-f]{64}$'),
        scopes        text[]      NOT NULL,
        first_seen_at timestamptz NOT NULL DEFAULT now(),
        updated_at    timestamptz NOT NULL DEFAULT now(),
        revoked_at    timestamptz
    );
    CREATE TABLE public.api_idempotency (
        access_id     text        NOT NULL,
        key           text        NOT NULL,
        method        text        NOT NULL,
        request_hash  bytea       NOT NULL,
        response      bytea,
        created_at    timestamptz NOT NULL DEFAULT now(),
        PRIMARY KEY (access_id, key)
    );
    CREATE INDEX api_idempotency_created_idx ON public.api_idempotency (created_at);
    ALTER TABLE public.identity ADD COLUMN suspended boolean NOT NULL DEFAULT false;

    FOR t IN SELECT * FROM (VALUES
        (1, 'provider', 'provider_id, display_name, player_id_rule, pseudo_xuid_prefix, trust_level, enabled, created_at',
            'provider_id'),
        (2, 'player', 'player_id, created_at', 'player_id'),
        (3, 'identity', 'identity_id, provider_id, external_id, player_id, verified_by, created_at, last_seen_at, suspended',
            'identity_id'),
        (5, 'display_name', 'player_id, name, name_key, skeleton, changed_at', 'player_id'),
        (6, 'display_name_reservation', 'player_id, name_key, skeleton, until', 'player_id, name_key'),
        (7, 'display_name_blocklist', 'term, contains', 'term'),
        (8, 'api_access', 'access_id, provider_id, cert_sha256, scopes, first_seen_at, updated_at, revoked_at',
            'access_id'),
        (9, 'api_idempotency', 'access_id, key, method, request_hash, response, created_at', 'access_id, key')
    ) AS v(ord, name, cols, pk) ORDER BY ord LOOP
        EXECUTE format('INSERT INTO public.%I (%s) SELECT %s FROM core.%I', t.name, t.cols, t.cols, t.name);
        EXECUTE format('SELECT count(*), md5(coalesce(string_agg(ROW(%s)::text, chr(10) ORDER BY %s), '''')) FROM core.%I',
                       t.cols, t.pk, t.name) INTO src_n, src_h;
        EXECUTE format('SELECT count(*), md5(coalesce(string_agg(ROW(%s)::text, chr(10) ORDER BY %s), '''')) FROM public.%I',
                       t.cols, t.pk, t.name) INTO dst_n, dst_h;
        IF src_n <> dst_n OR src_h <> dst_h THEN
            RAISE EXCEPTION 'core unsplit: % differs after copy (rows % -> %)', t.name, src_n, dst_n;
        END IF;
    END LOOP;

    -- Core audit rows back into the one audit table (new ids, same content).
    SELECT count(*), md5(coalesce(string_agg(ROW(at, source, action, identity_id, player_id, detail,
                                                 access_id, provider_id, method, outcome)::text,
                                             chr(10) ORDER BY at, audit_id), ''))
    INTO src_n, src_h FROM core.audit_log;
    CREATE TEMP TABLE unsplit_new_audit (audit_id bigint) ON COMMIT DROP;
    WITH ins AS (
        INSERT INTO public.audit_log (at, source, action, identity_id, player_id, detail,
                                      access_id, provider_id, method, outcome)
        SELECT at, source, action, identity_id, player_id, detail, access_id, provider_id, method, outcome
        FROM core.audit_log ORDER BY audit_id
        RETURNING audit_id
    ) INSERT INTO unsplit_new_audit SELECT audit_id FROM ins;
    SELECT count(*), md5(coalesce(string_agg(ROW(at, source, action, identity_id, player_id, detail,
                                                 access_id, provider_id, method, outcome)::text,
                                             chr(10) ORDER BY at, audit_id), ''))
    INTO dst_n, dst_h FROM public.audit_log WHERE audit_id IN (SELECT audit_id FROM unsplit_new_audit);
    IF src_n <> dst_n OR src_h <> dst_h THEN
        RAISE EXCEPTION 'core unsplit: audit rows differ after copy (rows % -> %)', src_n, dst_n;
    END IF;

    -- Foreign keys of the campaign tables, as before the split.
    ALTER TABLE public.audit_log
        ADD CONSTRAINT audit_log_identity_id_fkey FOREIGN KEY (identity_id) REFERENCES public.identity (identity_id),
        ADD CONSTRAINT audit_log_player_id_fkey FOREIGN KEY (player_id) REFERENCES public.player (player_id);
    ALTER TABLE public.gateway_result
        ADD CONSTRAINT gateway_result_identity_id_fkey FOREIGN KEY (identity_id) REFERENCES public.identity (identity_id),
        ADD CONSTRAINT gateway_result_player_id_fkey FOREIGN KEY (player_id) REFERENCES public.player (player_id);

    DROP SCHEMA core CASCADE;
    -- A split release applies both again; a pre-split release does not know
    -- them and ignores the missing rows.
    DELETE FROM public.fork_migrations
    WHERE name IN ('fork/core_split.sql', 'fork/gateway_annul.sql', 'fork/season_reset.sql');
    DROP FUNCTION IF EXISTS public.campaign_reset(boolean, timestamptz, text);
    DROP FUNCTION IF EXISTS public.campaign_reset_core_fingerprint();
END
$unsplit$;

-- annul_identity_results as before the split (fork/gateway_annul.sql of the
-- pre-split releases).
CREATE OR REPLACE FUNCTION annul_identity_results(
    p_provider    text,
    p_external_id text,
    p_suspend     boolean,
    p_dry_run     boolean,
    p_source      text
) RETURNS TABLE (
    annulled_identity uuid,
    annulled_player   uuid,
    user_reports      bigint,
    mission_reports   bigint,
    now_suspended     boolean
)
LANGUAGE plpgsql AS $$
DECLARE
    v_identity  uuid;
    v_player    uuid;
    v_user      bigint;
    v_mission   bigint;
    v_suspended boolean;
BEGIN
    SELECT i.identity_id, i.player_id, i.suspended INTO v_identity, v_player, v_suspended
    FROM identity i
    WHERE i.provider_id = p_provider AND i.external_id = p_external_id
    FOR UPDATE;
    IF NOT FOUND THEN
        RETURN;
    END IF;

    SELECT count(*) FILTER (WHERE g.kind = 'user_points'),
           count(*) FILTER (WHERE g.kind = 'mission_points')
    INTO v_user, v_mission
    FROM gateway_result g
    WHERE g.identity_id = v_identity AND g.annulled_at IS NULL;

    IF NOT p_dry_run THEN
        UPDATE leaderboard l SET
            stat1  = greatest(l.stat1  - s.s1,  0), stat2 = greatest(l.stat2 - s.s2, 0),
            stat3  = greatest(l.stat3  - s.s3,  0), stat4 = greatest(l.stat4 - s.s4, 0),
            stat5  = greatest(l.stat5  - s.s5,  0), stat6 = greatest(l.stat6 - s.s6, 0),
            stat7  = greatest(l.stat7  - s.s7,  0), stat8 = greatest(l.stat8 - s.s8, 0),
            stat9  = greatest(l.stat9  - s.s9,  0),
            stat10 = greatest(l.stat10 - s.s10, 0)
        FROM (
            SELECT g.player_id, g.faction, g.round, g.turn, g."map",
                   sum(g.points[1]) AS s1, sum(g.points[2]) AS s2, sum(g.points[3]) AS s3,
                   sum(g.points[4]) AS s4, sum(g.points[5]) AS s5, sum(g.points[6]) AS s6,
                   sum(g.points[7]) AS s7, sum(g.points[8]) AS s8, sum(g.points[9]) AS s9,
                   sum(g.points[10]) AS s10
            FROM gateway_result g
            WHERE g.identity_id = v_identity AND g.annulled_at IS NULL AND g.kind = 'user_points'
            GROUP BY g.player_id, g.faction, g.round, g.turn, g."map"
        ) s
        WHERE l.xuid = s.player_id AND l.faction = s.faction AND l.round = s.round
          AND l.turn = s.turn AND l."map" = s."map";

        UPDATE wartide w SET
            hsd = greatest(w.hsd - s.hsd, 0),
            prf = greatest(w.prf - s.prf, 0),
            rb  = greatest(w.rb  - s.rb,  0)
        FROM (
            SELECT g.round, (g."map" + (g.turn - 1) * 3)::smallint AS map_id,
                   coalesce(sum(g.points[1]) FILTER (WHERE g.faction = 'hsd'), 0) AS hsd,
                   coalesce(sum(g.points[1]) FILTER (WHERE g.faction = 'prf'), 0) AS prf,
                   coalesce(sum(g.points[1]) FILTER (WHERE g.faction = 'rb'), 0)  AS rb
            FROM gateway_result g
            WHERE g.identity_id = v_identity AND g.annulled_at IS NULL AND g.kind = 'mission_points'
            GROUP BY g.round, map_id
        ) s
        WHERE w.round = s.round AND w."map" = s.map_id;

        UPDATE gateway_result g SET annulled_at = now()
        WHERE g.identity_id = v_identity AND g.annulled_at IS NULL;

        IF p_suspend THEN
            UPDATE identity i SET suspended = true WHERE i.identity_id = v_identity;
            v_suspended := true;
        END IF;

        INSERT INTO audit_log (source, action, identity_id, player_id, provider_id, method, outcome, detail)
        VALUES (left(p_source, 64), 'results_annulled', v_identity, v_player, p_provider, 'annul', 'ok',
                format('user_reports=%s mission_reports=%s suspend=%s', v_user, v_mission, p_suspend));
    END IF;

    RETURN QUERY SELECT v_identity, v_player, v_user, v_mission, v_suspended;
END;
$$;
