-- SPDX-License-Identifier: AGPL-3.0-only
--
-- Annul the results of one identity (Steelbound gateway).
--
--   SELECT * FROM annul_identity_results('steelbound', '4242', true, false, 'admin');
--
-- Takes back every ledger row of the identity that is not annulled yet:
-- subtracts its stats from the leaderboard and its points from the war tide
-- (never below zero), marks the rows annulled, optionally suspends the
-- identity (the gateway then refuses it) and writes one audit row.
-- p_dry_run = true only counts. Returns no row if the identity is unknown.
--
-- Awards already handed out at a turn or round end are not recomputed.
-- Used by deploy/server/annul.sh and by the server's tests.
--
-- Since the core split (fork/core_split.sql) the identity lives in schema
-- core and the ledger, leaderboard, war tide and game audit in schema public;
-- every name is therefore schema-qualified. Listed after fork/core_split.sql
-- in migrations.list. The annulment writes its audit row to the campaign
-- (public.audit_log); a suspension also writes one to core.audit_log.

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
    v_ids       bigint[];
BEGIN
    -- FOR UPDATE serializes annulments of the same identity.
    SELECT i.identity_id, i.player_id, i.suspended INTO v_identity, v_player, v_suspended
    FROM core.identity i
    WHERE i.provider_id = p_provider AND i.external_id = p_external_id
    FOR UPDATE;
    IF NOT FOUND THEN
        RETURN;
    END IF;

    -- The open ledger rows of the identity, locked and fixed here: every step
    -- below works on exactly these rows. (The ledger refers to the identity by
    -- value since the core split, so a result reported meanwhile is not
    -- held back; it stays open and a second run annuls it.)
    v_ids := ARRAY(
        SELECT g.result_id FROM public.gateway_result g
        WHERE g.identity_id = v_identity AND g.annulled_at IS NULL
        ORDER BY g.result_id
        FOR UPDATE
    );
    SELECT count(*) FILTER (WHERE g.kind = 'user_points'),
           count(*) FILTER (WHERE g.kind = 'mission_points')
    INTO v_user, v_mission
    FROM public.gateway_result g
    WHERE g.result_id = ANY (v_ids);

    IF NOT p_dry_run THEN
        UPDATE public.leaderboard l SET
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
            FROM public.gateway_result g
            WHERE g.result_id = ANY (v_ids) AND g.kind = 'user_points'
            GROUP BY g.player_id, g.faction, g.round, g.turn, g."map"
        ) s
        WHERE l.xuid = s.player_id AND l.faction = s.faction AND l.round = s.round
          AND l.turn = s.turn AND l."map" = s."map";

        -- Same map numbering as the upstream AddMissionPoint handler.
        UPDATE public.wartide w SET
            hsd = greatest(w.hsd - s.hsd, 0),
            prf = greatest(w.prf - s.prf, 0),
            rb  = greatest(w.rb  - s.rb,  0)
        FROM (
            SELECT g.round, (g."map" + (g.turn - 1) * 3)::smallint AS map_id,
                   coalesce(sum(g.points[1]) FILTER (WHERE g.faction = 'hsd'), 0) AS hsd,
                   coalesce(sum(g.points[1]) FILTER (WHERE g.faction = 'prf'), 0) AS prf,
                   coalesce(sum(g.points[1]) FILTER (WHERE g.faction = 'rb'), 0)  AS rb
            FROM public.gateway_result g
            WHERE g.result_id = ANY (v_ids) AND g.kind = 'mission_points'
            GROUP BY g.round, map_id
        ) s
        WHERE w.round = s.round AND w."map" = s.map_id;

        UPDATE public.gateway_result g SET annulled_at = now()
        WHERE g.result_id = ANY (v_ids);

        IF p_suspend THEN
            UPDATE core.identity i SET suspended = true WHERE i.identity_id = v_identity;
            v_suspended := true;
            INSERT INTO core.audit_log (source, action, identity_id, player_id, provider_id, method, outcome)
            VALUES (left(p_source, 64), 'identity_suspended', v_identity, v_player, p_provider, 'annul', 'ok');
        END IF;

        INSERT INTO public.audit_log (source, action, identity_id, player_id, provider_id, method, outcome, detail)
        VALUES (left(p_source, 64), 'results_annulled', v_identity, v_player, p_provider, 'annul', 'ok',
                format('user_reports=%s mission_reports=%s suspend=%s', v_user, v_mission, p_suspend));
    END IF;

    RETURN QUERY SELECT v_identity, v_player, v_user, v_mission, v_suspended;
END;
$$;
