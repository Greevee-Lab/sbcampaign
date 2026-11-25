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
#   3. empties the database (drops and recreates schema public),
#   4. restores the dump with pg_restore,
#   5. deletes the plain dump (and the confirmation) in every case.
#
# Environment: RESTORE_DIR [/etc/steelcore/restore]
set -euo pipefail

dir=${RESTORE_DIR:-/etc/steelcore/restore}
shopt -s nullglob
dumps=("$dir"/*.dump)
[ ${#dumps[@]} -eq 1 ] || { echo "expected exactly one <database>.dump in $dir, found ${#dumps[@]}" >&2; exit 2; }
dump=${dumps[0]}
trap 'rm -f "$dump" "$dir/CONFIRM_PRODUCTION"' EXIT

db=$(basename "$dump" .dump)
case "$db" in
steelcore_test) unit=steelcore-test.service ;;
steelcore)
  unit=steelcore.service
  [ -f "$dir/CONFIRM_PRODUCTION" ] || { echo "restoring the production database needs $dir/CONFIRM_PRODUCTION" >&2; exit 2; }
  ;;
*) echo "unknown database: $db" >&2; exit 2 ;;
esac

if systemctl is-active --quiet "$unit"; then
  echo "$unit is running: stop it first (steelnet-admin svc stop ${unit%.service})" >&2
  exit 2
fi

echo "restoring $(basename "$dump") into $db"
psql -X -q -v ON_ERROR_STOP=1 -d "$db" <<'SQL'
DROP SCHEMA public CASCADE;
CREATE SCHEMA public;
SQL
pg_restore --no-owner --no-privileges --exit-on-error -d "$db" "$dump"
echo "restore of $db done"
