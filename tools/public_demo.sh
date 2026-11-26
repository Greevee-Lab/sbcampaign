#!/usr/bin/env bash
# SPDX-License-Identifier: AGPL-3.0-only
# Demo of the public campaign data (W524): fresh test database + synthetic
# demo campaign (tools/public_demo_seed.sql), then one export run.
#
#   PGHOST=127.0.0.1 PGPORT=5432 PGUSER=sbcampaign tools/public_demo.sh <new db> <out dir> [sbcampaign binary]
#
# Writes <out dir>/steelbound-s1.json. Drops and recreates <new db>; never
# point it at a real campaign database.
set -euo pipefail
db=${1:?database name}
out=${2:?output directory}
bin=${3:-target/debug/sbcampaign}
case "$db" in steelcore|steelcore_test|loc_campaign) echo "refusing to touch $db" >&2; exit 2 ;; esac
here=$(cd "$(dirname "$0")/.." && pwd)
mkdir -p "$out"
dropdb --if-exists "$db"
createdb "$db"
bash "$here/deploy/server/migrate.sh" "$db" sql/fork/test_seed.sql | tail -1
psql -X -q -v ON_ERROR_STOP=1 -d "$db" -f "$here/tools/public_demo_seed.sql"
cfg="$out/demo.toml"
# Placeholder key (the exporter creates it from /dev/urandom on the server).
[ -s "$out/.demo-name.key" ] || head -c 32 /dev/urandom > "$out/.demo-name.key"
{
  sed -e "s/^database = .*/database = \"$db\"\nhost = \"${PGHOST:-127.0.0.1}\"\nport = ${PGPORT:-5432}/" \
      -e "s/^username = .*/username = \"${PGUSER:-sbcampaign}\"/" "$here/loc.toml"
  cat <<EOF

[ticket]
campaign = "steelbound-s1"

[public]
name = "Steelbound Campaign"
season = "Season 1"
out_dir = "$out"
name_key = "$out/.demo-name.key"
EOF
} > "$cfg"
"$bin" "$cfg" --public-export
rm -f "$cfg"
echo "written: $out/steelbound-s1.json"
