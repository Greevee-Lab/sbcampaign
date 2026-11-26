#!/bin/bash
# SPDX-License-Identifier: AGPL-3.0-only
#
# Maintenance of one instance database, run by steelcore-maint.service
# (production, database steelcore) or steelcore-test-maint.service (test,
# database steelcore_test), oneshot, started by hand only:
#
#   maint.sh <database>
#
# The operator writes one line into MAINT_DIR/<database>.request
# (deploy.sh --reset / --unsplit does this):
#
#   reset   [apply] [start=<YYYY-MM-DDTHH:MM[:SS]Z>]
#           Season reset: empty the campaign (pilots, garage, results,
#           leaderboard, turns, game audit), refill the shop, start round 1,
#           turn 1, ending at <start> (default: one week from now). Players,
#           identities and display names (schema core) stay, row for row
#           (campaign_reset() in sql/fork/season_reset.sql checks it).
#   unsplit [apply]
#           Way back from the core split (sql/fork/core_unsplit.sql), right
#           before switching to a release from before the split.
#
# Without "apply" nothing changes (dry run: prints what would happen).
# With "apply":
#   1. the database must be one of MAINT_DATABASES (the unit sets it),
#   2. for the production database MAINT_DIR/CONFIRM_PRODUCTION must exist,
#   3. the instance service must be stopped,
#   4. an encrypted backup of the database is written first (backup.sh); if
#      that fails, nothing changes,
#   5. the change runs in one transaction; any failed check rolls it back.
# The request and the confirmation are deleted in every case. Prints counts,
# never contents.
#
# Environment: MAINT_DIR [/etc/steelcore/maint], MAINT_DATABASES [steelcore_test],
#              BACKUP_* (see backup.sh),
#              MAINT_SKIP_SERVICE_CHECK=1 only where there is no systemd (tests)
set -euo pipefail

db=${1:?usage: maint.sh <database>}
dir=${MAINT_DIR:-/etc/steelcore/maint}
here=$(cd "$(dirname "$0")" && pwd -P)
release=$(cd "$here/../.." && pwd -P)
req=$dir/$db.request

allowed=" ${MAINT_DATABASES:-steelcore_test} "
[[ "$db" =~ ^[a-z_][a-z0-9_]*$ && "$allowed" == *" $db "* ]] || { echo "database not allowed here: $db" >&2; exit 2; }
[ -f "$req" ] || { echo "no request in $req" >&2; exit 2; }
trap 'rm -f "$req" "$dir/CONFIRM_PRODUCTION"' EXIT

read -r action rest < "$req" || true
apply=false
start=
for word in $rest; do
  case "$word" in
  apply) apply=true ;;
  start=*)
    start=${word#start=}
    [[ "$start" =~ ^[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}(:[0-9]{2})?Z$ ]] ||
      { echo "start must look like 2026-12-02T18:45Z" >&2; exit 2; }
    ;;
  *) echo "unknown option: $word" >&2; exit 2 ;;
  esac
done
case "$action" in
reset) ;;
unsplit) [ -z "$start" ] || { echo "unsplit takes no start" >&2; exit 2; } ;;
*) echo "unknown action: ${action:-<empty>} (reset or unsplit)" >&2; exit 2 ;;
esac

case "$db" in
steelcore) unit=steelcore.service ;;
steelcore_test) unit=steelcore-test.service ;;
*) unit=${MAINT_SERVICE:-}.service ;;
esac

psql_db() { psql -X -q -v ON_ERROR_STOP=1 -d "$db" "$@"; }
# Row counts of the core tables (needs a split database).
counts() {
  psql_db -At -F ' ' -c "SELECT part, table_name, rows_before FROM campaign_reset(false) WHERE part = 'core'"
}

if ! $apply; then
  echo "$db: dry run of $action (add 'apply' to change data)"
  case "$action" in
  reset)
    psql_db -At -F ' ' -v start="${start:-}" <<'SQL'
SELECT part, table_name, rows_before
FROM campaign_reset(false, nullif(:'start', '')::timestamptz, 'maint.sh');
SQL
    ;;
  unsplit)
    if [ "$(psql_db -At -c "SELECT to_regclass('core.player') IS NOT NULL")" != t ]; then
      echo "$db: not split, nothing to do"
      exit 0
    fi
    counts | sed 's/^/would move back: /'
    ;;
  esac
  exit 0
fi

if [ "$db" = steelcore ] && [ ! -f "$dir/CONFIRM_PRODUCTION" ]; then
  echo "changing the production database needs $dir/CONFIRM_PRODUCTION" >&2
  exit 2
fi

if [ "${MAINT_SKIP_SERVICE_CHECK:-}" = 1 ]; then
  echo "service check skipped (MAINT_SKIP_SERVICE_CHECK=1)"
elif ! command -v systemctl >/dev/null; then
  echo "systemctl not found: cannot check that $unit is stopped" >&2
  exit 2
elif systemctl is-active --quiet "$unit"; then
  echo "$unit is running: stop it first (steelnet-admin svc stop ${unit%.service})" >&2
  exit 2
fi

echo "$db: backup before $action"
BACKUP_DBS=$db bash "$here/backup.sh"

case "$action" in
reset)
  echo "$db: season reset${start:+, turn 1 ends $start}"
  psql_db -At -F ' ' -v start="${start:-}" <<'SQL'
SELECT part, table_name, rows_before
FROM campaign_reset(true, nullif(:'start', '')::timestamptz, 'maint.sh');
SQL
  echo "$db: after the reset"
  psql_db -At -F ' ' <<'SQL'
SELECT 'settings', round, turn, to_char("period" AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS"Z"') FROM settings;
SELECT 'campaign accounts', count(*) FROM accounts;
SQL
  counts
  echo "$db: season reset done"
  ;;
unsplit)
  echo "$db: core unsplit"
  { echo 'BEGIN;'; cat "$release/sql/fork/core_unsplit.sql"; echo ';'; echo 'COMMIT;'; } | psql_db
  psql_db -At -F ' ' <<'SQL'
SELECT 'public player', count(*) FROM public.player;
SELECT 'public display_name', count(*) FROM public.display_name;
SELECT 'schema core left', count(*) FROM pg_namespace WHERE nspname = 'core';
SQL
  echo "$db: core unsplit done; now roll back to a release from before the split"
  ;;
esac
