-- SPDX-License-Identifier: AGPL-3.0-only
--
-- Synthetic seed data for local testing and CI. Never load this into a
-- production database. Contains no real player data.

-- Start the campaign: round 1, turn 1, ending one week from now.
INSERT INTO settings (round, turn, "period") VALUES (1, 1, now() + INTERVAL '1 WEEK');

-- One synthetic account (XUID 0x0009FFFF00000001, the default of
-- tools/handshake_client.py) with a known name and faction.
INSERT INTO accounts (xuid, xname, faction, "language")
VALUES ('00000000-0000-0000-0009-ffff00000001', 'TestPilot01', 'hsd', 'en');
