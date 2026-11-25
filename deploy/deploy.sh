#!/bin/bash
# SPDX-License-Identifier: AGPL-3.0-only
#
# Deploy a CI build of this repository to the campaign server.
#
#   deploy/deploy.sh <commit>               deploy the CI artifact of <commit>
#   deploy/deploy.sh --rollback <release>   switch back to an older release
#   deploy/deploy.sh --status               run the status check
#   deploy/deploy.sh --releases             list the releases on the server
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
--rollback)
  rel=${2:?usage: deploy.sh --rollback <release>}
  [[ "$rel" =~ ^[0-9a-f]{7,40}$ ]] || { echo "invalid release: $rel" >&2; exit 2; }
  remote "bash $root/$rel/deploy/server/activate.sh $rel --rollback"
  exit $?
  ;;
'' | -*)
  sed -n '4,16p' "$0"
  exit 2
  ;;
esac

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
  bash $root/$rel/deploy/server/activate.sh $rel"
