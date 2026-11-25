#!/bin/bash
# SPDX-License-Identifier: AGPL-3.0-only
#
# Checks the backfill of sql/fork/identity.sql on a fresh, empty database:
# accounts that exist before the migration become players with one identity,
# keyed by their unchanged account UUID. Synthetic data only.
#
#   tools/test_identity_backfill.sh <empty database>
#
# Connection settings come from the usual PG* environment variables.
set -euo pipefail
db=${1:?usage: test_identity_backfill.sh <empty database>}
sqldir=$(cd "$(dirname "$0")/../sql" && pwd -P)
psql_db() { psql -X -q -v ON_ERROR_STOP=1 -d "$db" "$@"; }

psql_db -f "$sqldir/fork/base_schema.sql"
# Two accounts as the upstream handshake creates them (XUID as UUID): one
# Xbox-style XUID and one Steelbound pseudo-XUID (account 42).
psql_db <<'SQL'
INSERT INTO accounts (xuid, xname, faction) VALUES
    (lpad(to_hex(x'0009FFFF000000AA'::bigint), 32, '0')::uuid, 'SynthXbox', 'hsd'),
    (lpad(to_hex(x'53424C010000002A'::bigint), 32, '0')::uuid, 'SynthSB',   'prf');
SQL
psql_db -f "$sqldir/fork/identity.sql"
psql_db <<'SQL'
DO $$
DECLARE n int;
BEGIN
    SELECT count(*) INTO n FROM player p JOIN accounts a ON a.xuid = p.player_id;
    IF n <> 2 THEN RAISE EXCEPTION 'expected 2 players, got %', n; END IF;

    PERFORM 1 FROM identity
     WHERE provider_id = 'insignia' AND external_id = '0009ffff000000aa'
       AND player_id = '00000000-0000-0000-0009-ffff000000aa' AND verified_by = 'backfill';
    IF NOT FOUND THEN RAISE EXCEPTION 'insignia identity missing or wrong'; END IF;

    PERFORM 1 FROM identity
     WHERE provider_id = 'steelbound' AND external_id = '42'
       AND player_id = '00000000-0000-0000-5342-4c010000002a' AND verified_by = 'backfill';
    IF NOT FOUND THEN RAISE EXCEPTION 'steelbound identity missing or wrong'; END IF;

    SELECT count(*) INTO n FROM audit_log WHERE action = 'identity_backfill';
    IF n <> 2 THEN RAISE EXCEPTION 'expected 2 audit rows, got %', n; END IF;

    -- Campaign rows are untouched.
    SELECT count(*) INTO n FROM accounts WHERE xname IN ('SynthXbox', 'SynthSB');
    IF n <> 2 THEN RAISE EXCEPTION 'accounts changed'; END IF;
END $$;
SQL
echo "$db: identity backfill ok"
