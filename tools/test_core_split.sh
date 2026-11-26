#!/bin/bash
# SPDX-License-Identifier: AGPL-3.0-only
#
# Checks the core split end to end on a fresh, empty database, with synthetic
# data shaped like a running season (players with display names, renamed
# players with reservations, Core API audit, game audit, results ledger,
# pilots, garage, leaderboard):
#
#   1. build the state before the split and fill it,
#   2. migrate (fork/core_split.sql takes the core over) and compare every
#      core table, row count and md5, with the state before,
#   3. migrate again (nothing to do), annul one identity's results,
#   4. season reset through deploy/server/maint.sh (dry run, then apply with
#      backup): the campaign is empty, the core unchanged row for row,
#   5. way back (maint.sh unsplit) and compare with the state before,
#   6. split again.
#
#   tools/test_core_split.sh <empty database>
#
# Environment: the usual PG* variables; PLAYERS [40] synthetic players;
# OLD_RELEASE=<dir> builds step 1 with the migrate.sh and SQL files of an
# older release (e.g. an unpacked "git archive <commit> sql deploy/server")
# instead of stopping before fork/core_split.sql. Synthetic data only.
set -euo pipefail
db=${1:?usage: test_core_split.sh <empty database>}
players=${PLAYERS:-40}
[[ "$players" =~ ^[0-9]+$ ]] && [ "$players" -ge 5 ] && [ "$players" -le 99999 ] || { echo "PLAYERS: 5..99999" >&2; exit 2; }
src=$(cd "$(dirname "$0")/.." && pwd -P)
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
export PGOPTIONS="${PGOPTIONS:-} -c client_min_messages=warning"
psql_db() { psql -X -q -v ON_ERROR_STOP=1 -d "$db" "$@"; }
# (tr: psql on Windows ends lines with CR LF)
q() { psql_db -At -F ' ' -c "$1" | tr -d '\r'; }
fail() { echo "FAIL: $*" >&2; exit 1; }

# Row count and md5 per core table, same columns and order as the split.
fingerprint() { # <schema>
  local s=$1
  psql_db -At -F ' ' <<SQL | tr -d '\r'
SELECT 'provider', count(*), md5(coalesce(string_agg(ROW(provider_id, display_name, player_id_rule, pseudo_xuid_prefix, trust_level, enabled, created_at)::text, chr(10) ORDER BY provider_id), '')) FROM $s.provider;
SELECT 'player', count(*), md5(coalesce(string_agg(ROW(player_id, created_at)::text, chr(10) ORDER BY player_id), '')) FROM $s.player;
SELECT 'identity', count(*), md5(coalesce(string_agg(ROW(identity_id, provider_id, external_id, player_id, verified_by, created_at, last_seen_at, suspended)::text, chr(10) ORDER BY identity_id), '')) FROM $s.identity;
SELECT 'display_name', count(*), md5(coalesce(string_agg(ROW(player_id, name, name_key, skeleton, changed_at)::text, chr(10) ORDER BY player_id), '')) FROM $s.display_name;
SELECT 'display_name_reservation', count(*), md5(coalesce(string_agg(ROW(player_id, name_key, skeleton, until)::text, chr(10) ORDER BY player_id, name_key), '')) FROM $s.display_name_reservation;
SELECT 'display_name_blocklist', count(*), md5(coalesce(string_agg(ROW(term, contains)::text, chr(10) ORDER BY term), '')) FROM $s.display_name_blocklist;
SELECT 'api_access', count(*), md5(coalesce(string_agg(ROW(access_id, provider_id, cert_sha256, scopes, first_seen_at, updated_at, revoked_at)::text, chr(10) ORDER BY access_id), '')) FROM $s.api_access;
SELECT 'api_idempotency', count(*), md5(coalesce(string_agg(ROW(access_id, key, method, request_hash, response, created_at)::text, chr(10) ORDER BY access_id, key), '')) FROM $s.api_idempotency;
SQL
}
# Core audit rows without their id (the way back renumbers them).
audit_core() { # <schema> <filter>
  q "SELECT count(*), md5(coalesce(string_agg(ROW(at, source, action, identity_id, player_id, detail, access_id, provider_id, method, outcome)::text, chr(10) ORDER BY at, audit_id), '')) FROM $1.audit_log WHERE $2"
}
CORE_AUDIT="action NOT IN ('game_write', 'results_annulled', 'campaign_reset')"

# ---- 1. State before the split ----------------------------------------------
if [ -n "${OLD_RELEASE:-}" ]; then
  echo "== 1. before the split: migrate.sh of $(basename "$OLD_RELEASE")"
  bash "$OLD_RELEASE/deploy/server/migrate.sh" "$db" sql/fork/preseason.sql | tail -n 1
else
  echo "== 1. before the split: migrate.sh up to fork/core_split.sql"
  MIGRATE_STOP_BEFORE=fork/core_split.sql bash "$src/deploy/server/migrate.sh" "$db" sql/fork/preseason.sql | tail -n 1
fi
[ "$(q "SELECT to_regclass('public.player') IS NOT NULL AND to_regnamespace('core') IS NULL")" = t ] ||
  fail "unexpected starting state"

psql_db -v n="$players" <<'SQL'
-- Synthetic Steelbound accounts 990001.. (pseudo-XUID 0x53424C01_<id>).
CREATE TEMP TABLE synth AS
SELECT g, (990000 + g) AS account,
       lpad(to_hex(x'53424C0100000000'::bigint + 990000 + g), 32, '0')::uuid AS pid,
       'Pilot ' || translate(g::text, '0123456789', 'abcdefghjk') AS name,
       'Old ' || translate(g::text, '0123456789', 'abcdefghjk') AS old_name
FROM generate_series(1, :n) g;

INSERT INTO accounts (xuid, xname, faction, pilot, "language")
SELECT pid, name, (ARRAY['hsd', 'prf', 'rb']::faction[])[1 + g % 3], decode(repeat('5a', 96), 'hex'), 'en'
FROM synth WHERE g % 4 <> 0;           -- every 4th player has not played yet
INSERT INTO accounts (xuid) SELECT pid FROM synth WHERE g % 4 = 0;   -- as resolve_identity did
INSERT INTO player (player_id, created_at) SELECT pid, now() - make_interval(hours => g) FROM synth;
INSERT INTO identity (provider_id, external_id, player_id, verified_by, created_at, last_seen_at, suspended)
SELECT 'steelbound', account::text, pid, CASE WHEN g % 2 = 0 THEN 'ticket' ELSE 'first_login' END,
       now() - make_interval(hours => g), now() - make_interval(mins => g), g % 17 = 0
FROM synth;
-- Display names: most players; every 5th renamed two days ago (old name
-- reserved, next change in five days).
INSERT INTO display_name (player_id, name, name_key, skeleton, changed_at)
SELECT pid, name, lower(name), replace(replace(translate(lower(name), 'i10 ', 'llo'), 'rn', 'm'), 'vv', 'w'),
       CASE WHEN g % 5 = 0 THEN now() - interval '2 days' ELSE now() - interval '20 days' END
FROM synth WHERE g % 7 <> 0;
INSERT INTO display_name_reservation (player_id, name_key, skeleton, until)
SELECT pid, lower(old_name), replace(replace(translate(lower(old_name), 'i10 ', 'llo'), 'rn', 'm'), 'vv', 'w'),
       now() + interval '28 days'
FROM synth WHERE g % 5 = 0 AND g % 7 <> 0;
INSERT INTO display_name_blocklist (term, contains) VALUES ('synthblock', true);
INSERT INTO api_access (access_id, provider_id, cert_sha256, scopes) VALUES
    ('steelnet-synth', 'steelbound', repeat('ab', 32), ARRAY['identity:write', 'profile:write', 'read:public']),
    ('stats-synth', NULL, repeat('cd', 32), ARRAY['read:public']);
UPDATE api_access SET revoked_at = now() WHERE access_id = 'stats-synth';
INSERT INTO api_idempotency (access_id, key, method, request_hash, response)
SELECT 'steelnet-synth', 'key-' || g, 'SetDisplayName', sha256(g::text::bytea), CASE WHEN g % 3 = 0 THEN NULL ELSE sha256(('r' || g)::bytea) END
FROM synth WHERE g % 7 <> 0;
-- Audit: core rows and game rows interleaved, as in a running season.
INSERT INTO audit_log (at, source, action, identity_id, player_id, detail)
SELECT now() - make_interval(hours => s.g), 'gateway', 'identity_created', i.identity_id, s.pid, 'steelbound verified_by=ticket'
FROM synth s JOIN identity i ON i.player_id = s.pid;
INSERT INTO audit_log (at, source, action, identity_id, player_id, access_id, provider_id, method, outcome)
SELECT now() - make_interval(mins => s.g), 'steelnet-synth', 'api_write', i.identity_id, s.pid, 'steelnet-synth', 'steelbound', 'SetDisplayName', 'ok'
FROM synth s JOIN identity i ON i.player_id = s.pid WHERE s.g % 7 <> 0;
INSERT INTO audit_log (at, source, action, identity_id, player_id, provider_id, method, outcome)
SELECT now() - make_interval(mins => s.g * 3 + k), 'gateway-steelbound', 'game_write', i.identity_id, s.pid, 'steelbound',
       (ARRAY['NewUserInfo', 'SetUserInfo', 'AddUserPoint'])[k], 'ok'
FROM synth s JOIN identity i ON i.player_id = s.pid, generate_series(1, 3) k WHERE s.g % 4 <> 0;
-- Campaign: garage, leaderboard, results ledger.
INSERT INTO garage ("type", "serial", xuid, faction)
SELECT 1 + g % 4, 900000 + g, pid, (ARRAY['hsd', 'prf', 'rb']::faction[])[1 + g % 3] FROM synth WHERE g % 4 <> 0;
INSERT INTO leaderboard (xuid, faction, round, turn, "map", stat1, stat2)
SELECT pid, (ARRAY['hsd', 'prf', 'rb']::faction[])[1 + g % 3], 1, 1, 1, 10 + g, g FROM synth WHERE g % 4 <> 0;
INSERT INTO gateway_result (identity_id, player_id, kind, round, turn, "map", faction, points)
SELECT i.identity_id, s.pid, 'user_points', 1, 1, 1, (ARRAY['hsd', 'prf', 'rb']::faction[])[1 + s.g % 3],
       ARRAY[10 + s.g, s.g, 0, 0, 0, 0, 0, 0, 0, 0]::bigint[]
FROM synth s JOIN identity i ON i.player_id = s.pid WHERE s.g % 4 <> 0;
UPDATE shop SET vtstock = greatest(vtstock - 3, 0), "serial" = "serial" + 3 WHERE vtlimit IS NOT NULL;
SQL

pg_dump -Fc --no-owner --no-privileges -d "$db" -f "$work/presplit.dump"
fingerprint public >"$work/before.txt"
audit_core public "$CORE_AUDIT" >"$work/audit_before.txt"
game_audit_before=$(q "SELECT count(*) FROM audit_log WHERE action = 'game_write'")
campaign_before=$(q "SELECT md5(string_agg(x::text, chr(10) ORDER BY x::text)) FROM accounts x")
echo "   synthetic season: $(q "SELECT count(*) FROM player") players, $(q "SELECT count(*) FROM identity") identities," \
  "$(q "SELECT count(*) FROM display_name") display names, $(q "SELECT count(*) FROM display_name_reservation") reservations," \
  "$(q "SELECT count(*) FROM audit_log") audit rows ($game_audit_before game), $(q "SELECT count(*) FROM gateway_result") results," \
  "$(q "SELECT count(*) FROM accounts") accounts"

# ---- 2. Takeover ------------------------------------------------------------
echo "== 2. takeover (migrate.sh of this release)"
bash "$src/deploy/server/migrate.sh" "$db" | grep -E "core_split|applied," || true
fingerprint core >"$work/after.txt"
diff "$work/before.txt" "$work/after.txt" >/dev/null || { diff "$work/before.txt" "$work/after.txt" >&2; fail "core differs after the takeover"; }
audit_core core true >"$work/audit_after.txt"
cmp -s "$work/audit_before.txt" "$work/audit_after.txt" || fail "core audit differs after the takeover"
# The split's own report says the same.
while read -r name n h; do
  [ "$(q "SELECT rows || ' ' || md5 FROM core.split_report WHERE name = '$name'")" = "$n $h" ] || fail "split_report $name"
done <"$work/before.txt"
[ "$(q "SELECT count(*) FROM pg_tables WHERE schemaname = 'public' AND tablename IN ('player', 'identity', 'provider', 'display_name', 'display_name_reservation', 'display_name_blocklist', 'api_access', 'api_idempotency')")" = 0 ] ||
  fail "old core tables still in public"
[ "$(q "SELECT count(*) FROM audit_log")" = "$game_audit_before" ] || fail "campaign audit rows changed"
[ "$(q "SELECT count(*) FROM pg_constraint WHERE contype = 'f' AND connamespace = 'public'::regnamespace")" = 0 ] ||
  fail "campaign tables still reference the core"
[ "$(q "SELECT md5(string_agg(x::text, chr(10) ORDER BY x::text)) FROM accounts x")" = "$campaign_before" ] || fail "accounts changed"
echo "   core: $(tr '\n' ';' <"$work/after.txt" | sed 's/ [0-9a-f]\{32\};/; /g')"
echo "   audit: core $(cut -d' ' -f1 "$work/audit_after.txt"), campaign $(q "SELECT count(*) FROM audit_log")"

# ---- 3. Idempotent, annulment ----------------------------------------------
echo "== 3. second run, annulment"
bash "$src/deploy/server/migrate.sh" "$db" | tail -n 1 | grep -q " 0 applied" || fail "second migrate run applied something"
{ echo 'BEGIN;'; cat "$src/sql/fork/core_split.sql"; echo ';COMMIT;'; } | psql_db
fingerprint core | cmp -s - "$work/after.txt" || fail "running core_split.sql again changed the core"
annul=$(q "SELECT user_reports, now_suspended FROM annul_identity_results('steelbound', '990001', true, false, 'test')")
[ "$annul" = "1 t" ] || fail "annul: $annul"
[ "$(q "SELECT count(*) FROM core.audit_log WHERE action = 'identity_suspended'")" = 1 ] || fail "annul: core audit"
[ "$(q "SELECT count(*) FROM audit_log WHERE action = 'results_annulled'")" = 1 ] || fail "annul: campaign audit"
fingerprint core >"$work/after.txt"            # one identity is suspended now
audit_core core true >"$work/audit_after.txt"

# ---- 4. Season reset --------------------------------------------------------
echo "== 4. season reset (maint.sh)"
export GNUPGHOME=$work/gnupg MAINT_DIR=$work/maint BACKUP_DIR=$work/backups MAINT_DATABASES=$db \
  MAINT_SKIP_SERVICE_CHECK=1 BACKUP_PUBKEY=$work/pub.asc
mkdir -p "$GNUPGHOME" "$MAINT_DIR"; chmod 700 "$GNUPGHOME" 2>/dev/null || true
gpg --batch --quiet --passphrase '' --quick-gen-key 'synthetic backup test' ed25519 cert never 2>/dev/null
fpr=$(gpg --batch --list-keys --with-colons 2>/dev/null | awk -F: '/^fpr/ {print $10; exit}')
gpg --batch --quiet --passphrase '' --quick-add-key "$fpr" cv25519 encr never 2>/dev/null
gpg --batch --armor --export "$fpr" >"$BACKUP_PUBKEY"
core_fp=$(q "SELECT campaign_reset_core_fingerprint()" | md5sum | cut -d' ' -f1)

echo "reset" >"$MAINT_DIR/$db.request"
bash "$src/deploy/server/maint.sh" "$db" >"$work/dry.txt"
[ ! -e "$MAINT_DIR/$db.request" ] || fail "request not deleted"
grep -q "^campaign accounts $players$" "$work/dry.txt" || fail "dry run: accounts"
[ "$(q "SELECT count(*) FROM accounts")" = "$players" ] || fail "dry run changed data"

echo "reset apply start=2030-01-08T18:00Z" >"$MAINT_DIR/$db.request"
bash "$src/deploy/server/maint.sh" "$db" >"$work/reset.txt"
grep -q "season reset done" "$work/reset.txt" || fail "reset did not finish"
ls "$BACKUP_DIR"/"$db"-*.dump.gpg >/dev/null 2>&1 || fail "no backup before the reset"
for t in accounts garage leaderboard gateway_result; do
  [ "$(q "SELECT count(*) FROM $t")" = 0 ] || fail "reset: $t not empty"
done
[ "$(q "SELECT round || '/' || turn || ' ' || to_char(\"period\" AT TIME ZONE 'UTC', 'YYYY-MM-DD HH24:MI') FROM settings")" = "1/1 2030-01-08 18:00" ] ||
  fail "reset: settings"
[ "$(q "SELECT count(*) FILTER (WHERE vtstock IS NOT DISTINCT FROM vtlimit AND \"serial\" = 1) FROM shop")" = 31 ] || fail "reset: shop"
[ "$(q "SELECT action FROM audit_log")" = campaign_reset ] || fail "reset: campaign audit"
[ "$(q "SELECT campaign_reset_core_fingerprint()" | md5sum | cut -d' ' -f1)" = "$core_fp" ] || fail "reset changed the core"
fingerprint core | cmp -s - "$work/after.txt" || fail "reset changed the core (fingerprint)"
echo "   reset: $(grep -c '^campaign ' "$work/dry.txt") campaign tables emptied, core $(grep -c '^core ' "$work/dry.txt") tables kept," \
  "$(q "SELECT count(*) FROM core.player") players, $(q "SELECT count(*) FROM core.display_name") display names," \
  "$(q "SELECT count(*) FROM core.display_name_reservation") reservations unchanged"

# The backup taken before the reset brings the campaign back (restore.sh,
# campaign only); the core stays as it is.
export RESTORE_DIR=$work/restore RESTORE_TEST_DATABASES=$db
mkdir -p "$RESTORE_DIR"
gpg --batch --quiet --decrypt -o "$RESTORE_DIR/$db.dump" "$(ls "$BACKUP_DIR"/"$db"-*.dump.gpg | head -n1)" 2>/dev/null
bash "$src/deploy/server/restore.sh" >"$work/restore.txt" 2>&1 || { cat "$work/restore.txt" >&2; fail "restore: campaign only"; }
grep -q "campaign only, the core stays" "$work/restore.txt" || fail "restore: campaign only (message)"
[ ! -e "$RESTORE_DIR/$db.dump" ] || fail "restore: plain dump not deleted"
[ "$(q "SELECT count(*) FROM accounts")" = "$players" ] || fail "restore: accounts not back"
[ "$(q "SELECT count(*) FROM audit_log WHERE action = 'game_write'")" = "$game_audit_before" ] || fail "restore: game audit not back"
fingerprint core | cmp -s - "$work/after.txt" || fail "restore changed the core"
# A dump from before the split is refused unless the core is replaced too.
cp "$work/presplit.dump" "$RESTORE_DIR/$db.dump"
if bash "$src/deploy/server/restore.sh" >/dev/null 2>&1; then fail "restore: pre-split dump accepted without RESTORE_CORE"; fi
fingerprint core | cmp -s - "$work/after.txt" || fail "refused restore changed the core"
echo "   restore of the pre-reset backup: campaign back ($players accounts), core unchanged; pre-split dump refused"
echo "reset apply start=2030-01-08T18:00Z" >"$MAINT_DIR/$db.request"
bash "$src/deploy/server/maint.sh" "$db" >/dev/null

# Production needs the confirmation; a database outside MAINT_DATABASES is refused.
echo "reset apply" >"$MAINT_DIR/steelcore.request"
if MAINT_DATABASES=steelcore bash "$src/deploy/server/maint.sh" steelcore >/dev/null 2>&1; then fail "production without confirmation"; fi
echo "reset apply" >"$MAINT_DIR/other.request"
if bash "$src/deploy/server/maint.sh" other >/dev/null 2>&1; then fail "database outside MAINT_DATABASES"; fi
rm -f "$MAINT_DIR"/*.request

# ---- 5. Way back ------------------------------------------------------------
echo "== 5. way back (maint.sh unsplit)"
echo "unsplit apply" >"$MAINT_DIR/$db.request"
bash "$src/deploy/server/maint.sh" "$db" | grep -E "unsplit done" >/dev/null || fail "unsplit did not finish"
fingerprint public | cmp -s - "$work/after.txt" || fail "core differs after the way back"
audit_core public "$CORE_AUDIT" | cmp -s - "$work/audit_after.txt" || fail "core audit differs after the way back"
[ "$(q "SELECT count(*) FROM pg_namespace WHERE nspname = 'core'")" = 0 ] || fail "schema core left"
[ "$(q "SELECT count(*) FROM accounts")" = "$players" ] || fail "way back: one empty account per player expected"
[ "$(q "SELECT count(*) FROM fork_migrations WHERE name = 'fork/core_split.sql'")" = 0 ] || fail "way back: split still recorded"
annul=$(q "SELECT user_reports FROM annul_identity_results('steelbound', '990002', false, true, 'test')")
[ "$annul" = 0 ] || fail "way back: old annul function: $annul"
echo "   way back: core in public again, identical (audit renumbered), $(q "SELECT count(*) FROM player") players"

# ---- 6. Split again ---------------------------------------------------------
echo "== 6. split again"
bash "$src/deploy/server/migrate.sh" "$db" >"$work/resplit.txt"
grep -q "applied fork/core_split.sql" "$work/resplit.txt" || fail "second split not applied"
fingerprint core | cmp -s - "$work/after.txt" || fail "core differs after the second split"

# ---- 7. Way back through the backup -----------------------------------------
echo "== 7. way back through a pre-split backup (restore.sh with RESTORE_CORE)"
cp "$work/presplit.dump" "$RESTORE_DIR/$db.dump"
touch "$RESTORE_DIR/RESTORE_CORE"
bash "$src/deploy/server/restore.sh" >"$work/restore.txt" 2>&1 || { cat "$work/restore.txt" >&2; fail "restore: everything"; }
grep -q "everything (campaign and core)" "$work/restore.txt" || fail "restore: everything (message)"
[ ! -e "$RESTORE_DIR/RESTORE_CORE" ] || fail "restore: RESTORE_CORE not deleted"
fingerprint public | cmp -s - "$work/before.txt" || fail "pre-split restore differs from the state before"
[ "$(q "SELECT count(*) FROM pg_namespace WHERE nspname = 'core'")" = 0 ] || fail "pre-split restore left schema core"
bash "$src/deploy/server/migrate.sh" "$db" >/dev/null
fingerprint core | cmp -s - "$work/before.txt" || fail "split after the pre-split restore differs"
echo "$db: core split, season reset, restore and way back ok"
