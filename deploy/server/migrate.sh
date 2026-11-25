#!/bin/bash
# SPDX-License-Identifier: AGPL-3.0-only
#
# Apply the schema files listed in sql/fork/migrations.list to one database.
# Runs as the service user (peer login over the local socket), normally from
# steelcore-migrate.service.
#
#   migrate.sh <database> [seed.sql]
#
# Every file is applied in its own transaction together with its record in
# fork_migrations, so an interrupted run leaves no half-applied file behind.
# A second run with the same release changes nothing.
#
# The optional seed file (path relative to the release) is applied once per
# database and recorded like a migration (kind "seed").
set -euo pipefail

db=${1:?usage: migrate.sh <database> [seed.sql]}
seed=${2:-}
release=$(cd "$(dirname "$0")/../.." && pwd -P)
sqldir=$release/sql
manifest=$sqldir/fork/migrations.list

[[ "$db" =~ ^[a-z_][a-z0-9_]*$ ]] || { echo "invalid database name: $db" >&2; exit 2; }
[ -f "$manifest" ] || { echo "missing $manifest" >&2; exit 2; }

psql_db() { psql -X -q -v ON_ERROR_STOP=1 -d "$db" "$@"; }

psql_db <<'SQL'
CREATE TABLE IF NOT EXISTS fork_migrations (
    name       text        PRIMARY KEY,
    kind       text        NOT NULL,
    sha256     text        NOT NULL,
    applied_at timestamptz NOT NULL DEFAULT now()
);
SQL

applied=0
unchanged=0

# apply <kind> <name> <file>
apply() {
  local kind=$1 name=$2 file=$3 sum recorded
  [[ "$name" =~ ^[A-Za-z0-9_./:-]+$ ]] || { echo "invalid migration name: $name" >&2; exit 2; }
  [ -f "$file" ] || { echo "missing $file" >&2; exit 2; }
  sum=$(sha256sum "$file" | cut -d' ' -f1)
  recorded=$(psql_db -At -c "SELECT sha256 FROM fork_migrations WHERE name = '$name'")

  if [ "$recorded" = "$sum" ]; then
    unchanged=$((unchanged + 1))
    return
  fi
  if [ -n "$recorded" ] && [ "$kind" != repeatable ]; then
    echo "$db: $name was changed after it was applied (add a new file instead)" >&2
    exit 1
  fi

  {
    echo 'BEGIN;'
    cat "$file"
    echo
    echo "INSERT INTO fork_migrations (name, kind, sha256) VALUES ('$name', '$kind', '$sum')"
    echo "  ON CONFLICT (name) DO UPDATE SET sha256 = EXCLUDED.sha256, applied_at = now();"
    echo 'COMMIT;'
  } | psql_db
  echo "$db: applied $name ($kind)"
  applied=$((applied + 1))
}

while read -r kind name _; do
  case "$kind" in
  '' | '#'*) continue ;;
  once | repeatable) apply "$kind" "$name" "$sqldir/$name" ;;
  *) echo "unknown kind '$kind' in $manifest" >&2; exit 2 ;;
  esac
done <"$manifest"

if [ -n "$seed" ]; then
  apply seed "seed:$(basename "$seed")" "$release/$seed"
fi

echo "$db: $applied applied, $unchanged unchanged"
