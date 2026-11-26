#!/bin/bash
# SPDX-License-Identifier: AGPL-3.0-only
#
# Deploy a CI build of this repository to the campaign server.
#
#   deploy/deploy.sh <commit>               deploy the CI artifact of <commit>
#                                           (both instances, both databases)
#   deploy/deploy.sh --test <commit>        deploy it to the test instance only
#                                           (releases/test, database
#                                           steelcore_test); production stays
#   deploy/deploy.sh --rollback <release>   switch back to an older release
#   deploy/deploy.sh --status               run the status check
#   deploy/deploy.sh --releases             list the releases on the server
#   deploy/deploy.sh --annul <db> <provider> <external_id> [apply] [suspend]
#                                           annul the results of one identity
#                                           (dry run without "apply")
#   deploy/deploy.sh --reset <db> [apply] [start=<YYYY-MM-DDTHH:MMZ>] [--confirm-production]
#                                           season reset: empty the campaign,
#                                           keep players and display names
#                                           (dry run without "apply"; with
#                                           "apply": stop, backup, reset, start)
#   deploy/deploy.sh --report <db>          read only: values checked by the core
#                                           split, row counts, period
#   deploy/deploy.sh --unsplit <db> [apply] [--confirm-production]
#                                           way back from the core split
#                                           (stops the instance, leaves it
#                                           stopped for the rollback)
#
# Environment:
#   STEELCORE_SSH      ssh target, e.g. deploy@campaign.example.org (required)
#   STEELCORE_SSH_KEY  ssh private key file (optional)
#   STEELCORE_REPO     GitHub repository with the CI runs [Greevee-Lab/sbcampaign]
#
# Needs: gh (logged in), ssh, tar. The host name is never stored in this repo.
set -euo pipefail

repo=${STEELCORE_REPO:-Greevee-Lab/sbcampaign}
target=${STEELCORE_SSH:?set STEELCORE_SSH to user@host of the campaign server}
ssh_opts=(-o BatchMode=yes -o ConnectTimeout=20)
[ -n "${STEELCORE_SSH_KEY:-}" ] && ssh_opts+=(-i "$STEELCORE_SSH_KEY")
root=/opt/steelcore/releases

remote() { ssh "${ssh_opts[@]}" "$target" "$@"; }

case "${1:-}" in
--status)
  remote "bash $root/current/deploy/server/status.sh"
  exit $?
  ;;
--releases)
  remote "ls -1t $root | grep -E '^[0-9a-f]{7,40}\$' | while read r; do
            [ \"\$r\" = \"\$(readlink $root/current)\" ] && m='*' || m=' ';
            echo \"\$m \$r \$(stat -c %y $root/\$r | cut -c1-16)\"; done"
  exit $?
  ;;
--annul)
  shift
  line="$*"
  [[ "$line" =~ ^(steelcore|steelcore_test)\ (steelbound|insignia)\ [0-9A-Fa-fx]{1,18}(\ (apply|suspend))*$ ]] ||
    { echo "usage: deploy.sh --annul <steelcore|steelcore_test> <provider> <external_id> [apply] [suspend]" >&2; exit 2; }
  remote "set -e; umask 027; mkdir -p -m 2770 /etc/steelcore/annul; echo '$line' > /etc/steelcore/annul/request
    sudo steelnet-admin svc start steelcore-annul || true
    sudo steelnet-admin logs steelcore-annul 6"
  exit $?
  ;;
--reset | --unsplit | --report)
  action=${1#--}
  db=${2:-}
  shift 2 || true
  case "$db" in
  steelcore) unit=steelcore-maint; svc=steelcore ;;
  steelcore_test) unit=steelcore-test-maint; svc=steelcore-test ;;
  *) echo "usage: deploy.sh --$action <steelcore|steelcore_test> [apply] [start=...] [--confirm-production]" >&2; exit 2 ;;
  esac
  opts=() apply=false confirm=false
  for word in "$@"; do
    case "$word" in
    apply) [ "$action" != report ] || { echo "report is read only" >&2; exit 2; }; apply=true; opts+=(apply) ;;
    start=*)
      [ "$action" = reset ] && [[ "${word#start=}" =~ ^[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}(:[0-9]{2})?Z$ ]] ||
        { echo "start=YYYY-MM-DDTHH:MMZ (reset only)" >&2; exit 2; }
      opts+=("$word") ;;
    --confirm-production) confirm=true ;;
    *) echo "unknown option: $word" >&2; exit 2 ;;
    esac
  done
  if $apply && [ "$db" = steelcore ] && ! $confirm; then
    echo "applying to the production database needs --confirm-production" >&2
    exit 2
  fi
  line="$action ${opts[*]}"
  conf=; $confirm && conf="touch /etc/steelcore/maint/CONFIRM_PRODUCTION"
  if $apply; then
    restart="sudo steelnet-admin svc start $svc"
    [ "$action" = unsplit ] && restart="echo '$svc stays stopped: roll back now (deploy.sh --rollback <release before the split>)'"
    remote "set -e; umask 027; mkdir -p -m 2770 /etc/steelcore/maint
      echo '$line' > /etc/steelcore/maint/$db.request; $conf
      sudo steelnet-admin svc stop $svc
      rc=0; sudo steelnet-admin svc start $unit || rc=\$?
      sudo steelnet-admin logs $unit 40
      $restart
      exit \$rc"
  else
    remote "set -e; umask 027; mkdir -p -m 2770 /etc/steelcore/maint
      echo '$line' > /etc/steelcore/maint/$db.request
      rc=0; sudo steelnet-admin svc start $unit || rc=\$?
      sudo steelnet-admin logs $unit 25
      exit \$rc"
  fi
  exit $?
  ;;
--rollback)
  rel=${2:?usage: deploy.sh --rollback <release>}
  [[ "$rel" =~ ^[0-9a-f]{7,40}$ ]] || { echo "invalid release: $rel" >&2; exit 2; }
  remote "bash $root/$rel/deploy/server/activate.sh $rel --rollback"
  exit $?
  ;;
--test) ;;
'' | -*)
  sed -n '4,34p' "$0"
  exit 2
  ;;
esac

mode=
if [ "$1" = --test ]; then
  mode=--test
  shift
  [ -n "${1:-}" ] || { echo "usage: deploy.sh --test <commit>" >&2; exit 2; }
fi

sha=$(gh api "repos/$repo/commits/$1" --jq .sha)
rel=${sha:0:12}
run=$(gh run list -R "$repo" --commit "$sha" --workflow ci.yml --status success \
  --limit 1 --json databaseId --jq '.[0].databaseId // empty')
[ -n "$run" ] || { echo "no successful CI run for $sha" >&2; exit 1; }
echo "commit $sha, CI run $run"

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
gh run download "$run" -R "$repo" -n "sbcampaign-x86_64-$sha" -D "$work/pkg"
[ "$(tr -d '\r\n' <"$work/pkg/COMMIT")" = "$sha" ] || { echo "artifact COMMIT does not match $sha" >&2; exit 1; }
[ -f "$work/pkg/deploy/server/activate.sh" ] || { echo "artifact has no deploy/ folder (too old?)" >&2; exit 1; }
tar -czf "$work/release.tar.gz" -C "$work/pkg" .

echo "uploading release $rel"
remote "set -e; umask 022; mkdir -p $root/.incoming-$rel; cat > $root/.incoming-$rel.tar.gz" <"$work/release.tar.gz"
remote "set -e; umask 022
  if [ -d $root/$rel ]; then rm -rf $root/.incoming-$rel; else
    tar -xzf $root/.incoming-$rel.tar.gz -C $root/.incoming-$rel
    chmod 0755 $root/.incoming-$rel/sbcampaign
    mv $root/.incoming-$rel $root/$rel
  fi
  rm -f $root/.incoming-$rel.tar.gz
  bash $root/$rel/deploy/server/activate.sh $rel $mode"
