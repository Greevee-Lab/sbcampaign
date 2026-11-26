#!/bin/bash
# SPDX-License-Identifier: AGPL-3.0-only
#
# Collect the parts of the status that need the service user, run by
# steelcore-status.service (timer, every 10 minutes): database ping, applied
# migrations, current campaign round/turn, core size (players, display names),
# newest backup per database, and a
# content fingerprint per database (row count + md5 per table, used to compare
# a restore with its source). Results go to STATUS_DIR as key=value text, which
# deploy/server/status.sh reads without database access.
#
# Environment (defaults in brackets):
#   STATUS_DBS   databases to check  [steelcore steelcore_test]
#   STATUS_DIR   output directory    [/var/lib/steelcore/status]
#   BACKUP_DIR   backup directory    [/var/lib/steelcore/backups]
set -uo pipefail

dbs=${STATUS_DBS:-steelcore steelcore_test}
dir=${STATUS_DIR:-/var/lib/steelcore/status}
backups=${BACKUP_DIR:-/var/lib/steelcore/backups}
umask 027
mkdir -p "$dir"

q() { psql -X -At -v ON_ERROR_STOP=1 -d "$1" -c "$2" 2>/dev/null; }

out=$(mktemp "$dir/.status.XXXXXX")
{
  echo "time=$(date -u +%s)"
  echo "release=$(cat "$(dirname "$0")/../../COMMIT" 2>/dev/null || echo unknown)"
  for db in $dbs; do
    if [ "$(q "$db" 'SELECT 1')" = 1 ]; then
      echo "db.$db=ok"
      echo "db.$db.migrations=$(q "$db" 'SELECT count(*) FROM fork_migrations' || echo 0)"
      echo "db.$db.procedures=$(q "$db" "SELECT count(*) FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace WHERE n.nspname = 'public'")"
      echo "db.$db.period=$(q "$db" "SELECT round || '/' || turn FROM settings ORDER BY round DESC, turn DESC LIMIT 1" || echo none)"
      # Core (players and display names, schema core since the core split).
      if [ "$(q "$db" "SELECT to_regclass('core.player') IS NOT NULL")" = t ]; then
        echo "db.$db.core=$(q "$db" "SELECT (SELECT count(*) FROM core.player) || ' players, ' || (SELECT count(*) FROM core.display_name) || ' display names'")"
      else
        echo "db.$db.core=not split"
      fi

      # Fingerprint: one line per table, then one hash over all of them.
      fp=$dir/fingerprint-$db.txt
      : >"$fp.tmp"
      # Campaign tables (schema public) as "<table>", core tables as "core.<table>".
      for t in $(q "$db" "SELECT tablename FROM pg_tables WHERE schemaname = 'public' ORDER BY tablename"); do
        q "$db" "SELECT '$t', count(*), md5(coalesce(string_agg(x::text, chr(10) ORDER BY x::text), '')) FROM public.\"$t\" x" >>"$fp.tmp"
      done
      for t in $(q "$db" "SELECT tablename FROM pg_tables WHERE schemaname = 'core' ORDER BY tablename"); do
        q "$db" "SELECT 'core.$t', count(*), md5(coalesce(string_agg(x::text, chr(10) ORDER BY x::text), '')) FROM core.\"$t\" x" >>"$fp.tmp"
      done
      mv "$fp.tmp" "$fp"
      echo "db.$db.fingerprint=$(md5sum <"$fp" | cut -d' ' -f1)"
    else
      echo "db.$db=fail"
    fi

    newest=$(ls -1t "$backups"/"$db"-*.dump.gpg 2>/dev/null | head -n1)
    if [ -n "$newest" ]; then
      echo "backup.$db=$(basename "$newest")"
      echo "backup.$db.time=$(stat -c %Y "$newest")"
    fi
  done
} >"$out"
chmod 0640 "$out"
mv "$out" "$dir/status.txt"
cat "$dir/status.txt"
