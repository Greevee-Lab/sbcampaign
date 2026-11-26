-- SPDX-License-Identifier: AGPL-3.0-only
-- Synthetic demo campaign for the public campaign data (W524), on top of a test
-- database (migrate.sh <db> sql/fork/test_seed.sql). Never on production.
-- Round 1, turn 4, 60 pilots (some without display name, one banned, one
-- suspended, one with only annulled results), war tide for turns 1-4, battles
-- of the last two days. Deterministic (setseed). W526: hits always below
-- shots, every 7th pilot only played once this turn (below the page's minimum
-- for rates). W527: settings rows for the closed turns 1-3 (turn reports),
-- 2-7 missions per pilot and turn (so the game's award minimums are reached).
-- Used by tools/public_demo.sh.

BEGIN;

SELECT setseed(0.524);

DELETE FROM settings;
INSERT INTO settings (round, turn, "period")
SELECT 1, t, date_trunc('hour', now()) + INTERVAL '2 days 5 hours' - (4 - t) * INTERVAL '7 days'
FROM generate_series(1, 4) AS t;

DO $$
DECLARE
    names text[] := ARRAY['Iron Vale', 'Kestrel', 'Grey Lantern', 'Ashfall', 'Bulwark', 'Nightjar', 'Cobalt',
        'Tin Soldier', 'Rook', 'Vantage', 'Foxtrot Nine', 'Halberd', 'Mossback', 'Quarry', 'Sable', 'Driftwood',
        'Ember', 'Longbow', 'Pike', 'Marrow', 'Talon', 'Wren', 'Gantry', 'Basalt', 'Corvid', 'Hollow Point',
        'Slate', 'Rampart', 'Thistle', 'Anvil', 'Bramble', 'Cinder', 'Dunmore', 'Echo Six', 'Flint', 'Glacier',
        'Harrow', 'Ironclad', 'Juniper', 'Kodiak', 'Lantern', 'Magpie', 'Nettle', 'Onyx', 'Peregrine', 'Quill',
        'Riptide', 'Starling', 'Tundra', 'Umber', 'Vesper', 'Warden', 'Yarrow', 'Zephyr', NULL, NULL, NULL, 'Banned Demo', 'Suspended Demo', 'Annulled Demo'];
    sides text[] := ARRAY['hsd', 'prf', 'rb'];
    i int; t int; m int; k int;
    pid uuid; iid uuid; side text;
    st int[]; war int; shots int; at timestamptz;
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
        FOR t IN (CASE WHEN i % 7 = 0 THEN 4 WHEN side = 'rb' THEN 3 ELSE 1 END)..4 LOOP
            FOR k IN 1..(CASE WHEN i % 7 = 0 THEN 1 ELSE (2 + floor(random() * 6))::int END) LOOP
                -- Map 1..3 within the turn, as the game sends it.
                m := 1 + floor(random() * 3)::int;
                shots := (60 + random() * 340)::int;
                st := ARRAY[
                    (shots * (0.15 + random() * 0.5))::int, shots, (random() * 6)::int,
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

-- W528: one clearly led sector in the running turn (turn 4, third map), so the
-- page shows calm and contested sectors side by side. Demo only.
INSERT INTO wartide (round, "map", prf) VALUES (1, 12, 2500)
ON CONFLICT (round, "map") DO UPDATE SET prf = wartide.prf + excluded.prf;

-- W529: whole battles in the last hours of turn 4 (every pilot of a match
-- reports within seconds): 1:1, 2:2, 6:6, 5:3, 2:2:2, a contradiction (both
-- sides report a win), a one-sided report, a draw, and a match with a banned
-- pilot (left out of the export). Pilot n is the n-th demo name; its side is
-- hsd for n % 3 = 0, prf for 1, rb for 2. The random results above end five
-- hours ago, so these are the newest.
UPDATE gateway_result SET at = at - INTERVAL '5 hours' WHERE round = 1 AND turn = 4;
CREATE FUNCTION pg_temp.demo_report(n int, mins numeric, m int, win bool, kp int, kills int) RETURNS void
LANGUAGE plpgsql AS $$
DECLARE
    pid uuid := ('5342dead-0000-4000-8000-' || lpad(to_hex(n), 12, '0'))::uuid;
    iid uuid;
    side text := (ARRAY['hsd', 'prf', 'rb'])[1 + (n % 3)];
    st bigint[] := ARRAY[kills * 9 + 4, kills * 20 + 30, kills, kp, (NOT win)::int, win::int, 1, 0, 0, 0];
BEGIN
    SELECT identity_id INTO iid FROM core.identity WHERE player_id = pid;
    INSERT INTO gateway_result (at, identity_id, player_id, kind, round, turn, "map", faction, points)
    VALUES (now() - mins * INTERVAL '1 minute', iid, pid, 'user_points', 1, 4, m, side::faction, st),
           (now() - mins * INTERVAL '1 minute', iid, pid, 'mission_points', 1, 4, m, side::faction,
            ARRAY[CASE WHEN win THEN 60 ELSE 10 END]::bigint[]);
END $$;
-- One match: winners and losers (pilot numbers), minutes ago, map; reports 4 s apart.
CREATE FUNCTION pg_temp.demo_match(mins int, m int, winners int[], losers int[]) RETURNS void
LANGUAGE plpgsql AS $$
DECLARE k int := 0; n int;
BEGIN
    FOREACH n IN ARRAY winners LOOP
        PERFORM pg_temp.demo_report(n, mins - k * 4 / 60.0, m, true, 50 + (n * 37) % 250, 1 + n % 3);
        k := k + 1;
    END LOOP;
    FOREACH n IN ARRAY losers LOOP
        PERFORM pg_temp.demo_report(n, mins - k * 4 / 60.0, m, false, (n * 13) % 60, n % 2);
        k := k + 1;
    END LOOP;
END $$;
SELECT pg_temp.demo_match(15, 2, ARRAY[1], ARRAY[3]);                                   -- 1:1 PRF beats HSD
SELECT pg_temp.demo_match(35, 1, ARRAY[6, 9], ARRAY[2, 5]);                             -- 2:2 HSD beats RB
SELECT pg_temp.demo_match(55, 3, ARRAY[4, 7, 10, 13, 16, 55], ARRAY[12, 15, 18, 21, 24, 57]); -- 6:6, two unnamed
SELECT pg_temp.demo_match(80, 2, ARRAY[27, 19], ARRAY[]::int[]);                        -- contradiction
SELECT pg_temp.demo_match(100, 1, ARRAY[8, 11], ARRAY[]::int[]);                        -- only RB reported
SELECT pg_temp.demo_match(125, 3, ARRAY[22, 25, 28], ARRAY[14, 17, 20, 23, 26]);        -- 3 PRF beat 5 RB
SELECT pg_temp.demo_match(150, 1, ARRAY[31, 58], ARRAY[30]);                            -- with the banned pilot
SELECT pg_temp.demo_match(170, 2, ARRAY[]::int[], ARRAY[33, 29]);                       -- draw
SELECT pg_temp.demo_match(190, 3, ARRAY[34, 37], ARRAY[36, 39]);                        -- 2:2
SELECT pg_temp.demo_match(210, 2, ARRAY[32], ARRAY[42]);                                -- 1:1
SELECT pg_temp.demo_match(230, 1, ARRAY[45, 48], ARRAY[40, 43, 35, 38]);                -- 2:2:2
SELECT pg_temp.demo_match(250, 3, ARRAY[46], ARRAY[51]);                                -- 1:1
SELECT pg_temp.demo_match(270, 2, ARRAY[49, 52, 1, 4], ARRAY[54, 3, 6, 9]);             -- 4:4
SELECT pg_temp.demo_match(285, 1, ARRAY[44], ARRAY[18]);                                -- 1:1 RB beats HSD
SELECT pg_temp.demo_match(295, 3, ARRAY[50, 53], ARRAY[7, 10]);                         -- 2:2 RB beats PRF

COMMIT;
