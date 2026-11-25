-- SPDX-License-Identifier: AGPL-3.0-only
--
-- Pre-season row for a fresh production database.
--
-- The service task needs one row in settings; without it, it logs an alert
-- every minute and does nothing else. This inserts round 1, turn 1 (one week)
-- only if the table is empty. It contains no player data.
--
-- The real season start is set by the operator at launch (see
-- deploy/README.md, "Season start").
INSERT INTO settings (round, turn, "period")
SELECT 1, 1, now() + INTERVAL '1 WEEK'
WHERE NOT EXISTS (SELECT 1 FROM settings);
