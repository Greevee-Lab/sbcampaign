#!/bin/bash
# SPDX-License-Identifier: AGPL-3.0-only
#
# Restore a decrypted backup, run by steelcore-restore.service (oneshot).
#
# The server cannot decrypt backups (no private key). The operator decrypts
# locally and drops the plain dump into RESTORE_DIR as <database>.dump, then
# starts this service. The script
#   1. refuses unless the matching service is stopped
#      (steelcore -> steelcore.service, steelcore_test -> steelcore-test.service),
#   2. for the production database also requires RESTORE_DIR/CONFIRM_PRODUCTION,
#   3. restores the campaign only (schema public: drop, recreate,
#      pg_restore --schema=public) and keeps the core (schema core: players,
#      identities, display names) as it is; this is the normal case, e.g.
#      the campaign of yesterday with today's players and names,
#   4. or, only if RESTORE_DIR/RESTORE_CORE exists, restores everything
#      (drops schemas public and core, restores the whole dump): disaster
#      recovery, or the way back to the state before the core split. A dump
#      from before the split (players still in public) can only be restored
#      this way,
#   5. deletes the plain dump and the confirmation files in every case.
#
# Environment: RESTORE_DIR [/etc/steelcore/restore],
#              RESTORE_TEST_DATABASES (tests only: further allowed databases)
set -euo pipefail

dir=${RESTORE_DIR:-/etc/steelcore/restore}
shopt -s nullglob
dumps=("$dir"/*.dump)
[ ${#dumps[@]} -eq 1 ] || { echo "expected exactly one <database>.dump in $dir, found ${#dumps[@]}" >&2; exit 2; }
dump=${dumps[0]}
trap 'rm -f "$dump" "$dir/CONFIRM_PRODUCTION" "$dir/RESTORE_CORE"' EXIT

db=$(basename "$dump" .dump)
case "$db" in
steelcore_test) unit=steelcore-test.service ;;
steelcore)
  unit=steelcore.service
  [ -f "$dir/CONFIRM_PRODUCTION" ] || { echo "restoring the production database needs $dir/CONFIRM_PRODUCTION" >&2; exit 2; }
  ;;
*)
  # Tests only: another database named in RESTORE_TEST_DATABASES.
  [[ "$db" =~ ^[a-z_][a-z0-9_]*$ && " ${RESTORE_TEST_DATABASES:-} " == *" $db "* ]] ||
    { echo "unknown database: $db" >&2; exit 2; }
  unit=steelcore-restore-test-$db.service
  ;;
esac

if command -v systemctl >/dev/null && systemctl is-active --quiet "$unit"; then
  echo "$unit is running: stop it first (steelnet-admin svc stop ${unit%.service})" >&2
  exit 2
fi

toc=$(pg_restore -l "$dump")
# Before the core split the players were campaign tables (public.player).
presplit=false
grep -Eq '^[0-9]+; [0-9]+ [0-9]+ TABLE public player ' <<<"$toc" && presplit=true

if [ -f "$dir/RESTORE_CORE" ]; then
  echo "restoring $(basename "$dump") into $db: everything (campaign and core)"
  psql -X -q -v ON_ERROR_STOP=1 -d "$db" <<'SQL'
DROP SCHEMA IF EXISTS core CASCADE;
DROP SCHEMA public CASCADE;
CREATE SCHEMA public;
SQL
  pg_restore --no-owner --no-privileges --exit-on-error -d "$db" "$dump"
else
  if $presplit; then
    echo "$(basename "$dump") is from before the core split (players in public): it would bring back"       "old players and names; restore it only with $dir/RESTORE_CORE (replaces the core too)" >&2
    exit 2
  fi
  echo "restoring $(basename "$dump") into $db: campaign only, the core stays"
  psql -X -q -v ON_ERROR_STOP=1 -d "$db" <<'SQL'
DROP SCHEMA public CASCADE;
CREATE SCHEMA public;
SQL
  pg_restore --no-owner --no-privileges --exit-on-error --schema=public -d "$db" "$dump"
fi
echo "restore of $db done"
