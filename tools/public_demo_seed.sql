-- SPDX-License-Identifier: AGPL-3.0-only
-- Synthetic demo campaign for the public campaign data (W524), on top of a test
-- database (migrate.sh <db> sql/fork/test_seed.sql). Never on production.
-- Round 1, turn 4, 36 pilots (some without display name, one banned, one
-- suspended, one with only annulled results), war tide for turns 1-4, battles
-- of the last two days. Deterministic (setseed).
-- Used by tools/public_demo.sh.

BEGIN;

SELECT setseed(0.524);

DELETE FROM settings;
INSERT INTO settings (round, turn, "period")
VALUES (1, 4, date_trunc('hour', now()) + INTERVAL '2 days 5 hours');

DO $$
DECLARE
    names text[] := ARRAY['Iron Vale', 'Kestrel', 'Grey Lantern', 'Ashfall', 'Bulwark', 'Nightjar', 'Cobalt',
        'Tin Soldier', 'Rook', 'Vantage', 'Foxtrot Nine', 'Halberd', 'Mossback', 'Quarry', 'Sable', 'Driftwood',
        'Ember', 'Longbow', 'Pike', 'Marrow', 'Talon', 'Wren', 'Gantry', 'Basalt', 'Corvid', 'Hollow Point',
        'Slate', 'Rampart', 'Thistle', 'Anvil', NULL, NULL, NULL, 'Banned Demo', 'Suspended Demo', 'Annulled Demo'];
    sides text[] := ARRAY['hsd', 'prf', 'rb'];
    i int; t int; m int; k int;
    pid uuid; iid uuid; side text;
    st int[]; war int; at timestamptz;
BEGIN
    FOR i IN 1..array_length(names, 1) LOOP
        pid := ('5342dead-0000-4000-8000-' || lpad(to_hex(i), 12, '0'))::uuid;
        side := sides[1 + (i % 3)];
        INSERT INTO accounts (xuid, xname, faction, banned)
        VALUES (pid, 'DemoAcct' || i, side::faction, names[i] IS NOT DISTINCT FROM 'Banned Demo');
        INSERT INTO core.player (player_id) VALUES (pid);
        INSERT INTO core.identity (provider_id, external_id, player_id, verified_by, suspended)
        VALUES ('steelbound', (900000 + i)::text, pid, 'ticket', names[i] IS NOT DISTINCT FROM 'Suspended Demo')
        RETURNING identity_id INTO iid;
        IF names[i] IS NOT NULL THEN
            INSERT INTO core.display_name (player_id, name, name_key, skeleton)
            VALUES (pid, names[i], lower(names[i]), lower(names[i]));
        END IF;
        -- RB joins in turn 3.
        FOR t IN (CASE WHEN side = 'rb' THEN 3 ELSE 1 END)..4 LOOP
            FOR k IN 1..(1 + floor(random() * 3))::int LOOP
                m := floor(random() * 3)::int;
                st := ARRAY[
                    (40 + random() * 160)::int, (150 + random() * 250)::int, (random() * 6)::int,
                    (random() * 900)::int, (random() * 4)::int, (random() < 0.5)::int, 1,
                    (random() < 0.2)::int, (random() < 0.3)::int, (random() < 0.1)::int];
                war := (20 + random() * 180)::int;
                at := now() - (4 - t) * INTERVAL '7 days' - random() * INTERVAL '46 hours';
                IF names[i] IS NOT DISTINCT FROM 'Annulled Demo' THEN
                    -- Annulled: the ledger keeps the rows (marked), the sums never had them.
                    INSERT INTO gateway_result (at, identity_id, player_id, kind, round, turn, "map", faction, points, annulled_at)
                    VALUES (at, iid, pid, 'user_points', 1, t, m, side::faction, st::bigint[], now());
                    CONTINUE;
                END IF;
                INSERT INTO leaderboard (xuid, faction, round, turn, "map",
                                         stat1, stat2, stat3, stat4, stat5, stat6, stat7, stat8, stat9, stat10)
                VALUES (pid, side::faction, 1, t, m, st[1], st[2], st[3], st[4], st[5], st[6], st[7], st[8], st[9], st[10])
                ON CONFLICT (xuid, faction, round, turn, "map") DO UPDATE SET
                    stat1 = leaderboard.stat1 + excluded.stat1, stat2 = leaderboard.stat2 + excluded.stat2,
                    stat3 = leaderboard.stat3 + excluded.stat3, stat4 = leaderboard.stat4 + excluded.stat4,
                    stat5 = leaderboard.stat5 + excluded.stat5, stat6 = leaderboard.stat6 + excluded.stat6,
                    stat7 = leaderboard.stat7 + excluded.stat7, stat8 = leaderboard.stat8 + excluded.stat8,
                    stat9 = leaderboard.stat9 + excluded.stat9, stat10 = leaderboard.stat10 + excluded.stat10;
                EXECUTE format('INSERT INTO wartide (round, "map", %1$I) VALUES (1, $1, $2)
                                ON CONFLICT (round, "map") DO UPDATE SET %1$I = wartide.%1$I + excluded.%1$I', side)
                    USING (m + (t - 1) * 3)::smallint, war;
                INSERT INTO gateway_result (at, identity_id, player_id, kind, round, turn, "map", faction, points)
                VALUES (at, iid, pid, 'user_points', 1, t, m, side::faction, st::bigint[]),
                       (at, iid, pid, 'mission_points', 1, t, m, side::faction, ARRAY[war]::bigint[]);
            END LOOP;
        END LOOP;
    END LOOP;
END $$;

COMMIT;
