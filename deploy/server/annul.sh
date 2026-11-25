#!/bin/bash
# SPDX-License-Identifier: AGPL-3.0-only
#
# Annul the reported results of one identity, run by steelcore-annul.service
# (oneshot, started by hand). Only on the server; there is no network API for it.
#
# The operator writes one line into REQUEST (deploy.sh --annul does this):
#
#   <database> <provider> <external_id> [apply] [suspend]
#
#   database     steelcore or steelcore_test
#   provider     steelbound (or insignia)
#   external_id  the provider's account id (steelbound: decimal account id)
#   apply        without it, nothing changes (dry run: only counts)
#   suspend      also suspend the identity (the gateway refuses it from then on;
#                reinstate with the Core API AdminService)
#
# Calls annul_identity_results() (sql/fork/gateway_annul.sql): subtracts the
# identity's ledger rows from leaderboard and war tide, marks them annulled,
# writes an audit row. Prints counts, never contents. The request file is
# deleted in every case.
#
# Environment: REQUEST [/etc/steelcore/annul/request],
#              ANNUL_DATABASES [steelcore steelcore_test]
set -euo pipefail

req=${REQUEST:-/etc/steelcore/annul/request}
[ -f "$req" ] || { echo "no request in $req" >&2; exit 2; }
trap 'rm -f "$req"' EXIT
read -r db provider external rest < "$req" || true
apply=false; suspend=false
for word in $rest; do
  case "$word" in
  apply) apply=true ;;
  suspend) suspend=true ;;
  *) echo "unknown option: $word" >&2; exit 2 ;;
  esac
done
allowed=" ${ANNUL_DATABASES:-steelcore steelcore_test} "
[[ "$db" =~ ^[a-z_][a-z0-9_]*$ && "$allowed" == *" $db "* ]] || { echo "unknown database: $db" >&2; exit 2; }
case "$provider" in steelbound | insignia) ;; *) echo "unknown provider: $provider" >&2; exit 2 ;; esac
[[ "$external" =~ ^[0-9A-Fa-fx]{1,18}$ ]] || { echo "invalid external id" >&2; exit 2; }
$apply || echo "dry run (add 'apply' to change data)"

dry=true; $apply && dry=false
out=$(psql -X -q -At -F ' ' -v ON_ERROR_STOP=1 -d "$db" \
  -v provider="$provider" -v ext="$external" -v suspend="$suspend" -v dry="$dry" <<'SQL'
SELECT annulled_player, user_reports, mission_reports, now_suspended
FROM annul_identity_results(:'provider', :'ext', :'suspend'::boolean, :'dry'::boolean, 'annul.sh');
SQL
)
if [ -z "$out" ]; then
  echo "$db: identity $provider/$external not found"
  exit 1
fi
read -r player users missions suspended <<<"$out"
echo "$db: $provider/$external player $player: user_reports=$users mission_reports=$missions suspended=$suspended applied=$apply"
