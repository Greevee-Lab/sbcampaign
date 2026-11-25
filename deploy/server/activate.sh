#!/bin/bash
# SPDX-License-Identifier: AGPL-3.0-only
#
# Activate an unpacked release on the server. Runs as the deploy user (member
# of group steelcore), called by deploy/deploy.sh:
#
#   bash /opt/steelcore/releases/<release>/deploy/server/activate.sh <release>
#   bash /opt/steelcore/releases/<release>/deploy/server/activate.sh <release> --rollback
#
# Steps: point releases/current at <release>, install changed unit files
# through the host's admin tool, run the migrations (steelcore-migrate), restart
# the services, run the status check, keep the newest KEEP_RELEASES releases.
# If the migrations fail, current is switched back and the services keep the
# previous release. --rollback skips the migrations (a rollback never changes
# the database).
set -euo pipefail

root=/opt/steelcore/releases
units_dir=/opt/steelcore/units
admin=(sudo -n /usr/local/sbin/steelnet-admin)
keep=${KEEP_RELEASES:-5}

rel=${1:?usage: activate.sh <release> [--rollback]}
mode=${2:-}
[[ "$rel" =~ ^[0-9a-f]{7,40}$ ]] || { echo "invalid release name: $rel" >&2; exit 2; }
dir=$root/$rel
[ -x "$dir/sbcampaign" ] || chmod 0755 "$dir/sbcampaign"
[ -f "$dir/COMMIT" ] || { echo "$dir/COMMIT missing" >&2; exit 2; }

for f in /etc/steelcore/steelcore.toml /etc/steelcore/steelcore-test.toml /etc/steelcore/backup-pubkey.asc; do
  [ -r "$f" ] || { echo "missing $f (see deploy/README.md, first setup)" >&2; exit 2; }
done

previous=$(readlink "$root/current" 2>/dev/null || true)
switch() {
  ln -sfn "$1" "$root/.current.new"
  mv -T "$root/.current.new" "$root/current"
  echo "current -> $1"
}
switch "$rel"

# Unit files: copy and install only what changed. A timer is installed together
# with its service.
changed=()
for src in "$dir"/deploy/units/steelcore*.service; do
  name=$(basename "$src" .service)
  timer=${src%.service}.timer
  if ! cmp -s "$src" "$units_dir/$name.service" ||
    { [ -f "$timer" ] && ! cmp -s "$timer" "$units_dir/$name.timer"; }; then
    install -m 0664 "$src" "$units_dir/$name.service"
    [ -f "$timer" ] && install -m 0664 "$timer" "$units_dir/$name.timer"
    "${admin[@]}" unit install "$name"
    changed+=("$name")
  fi
done
[ ${#changed[@]} -gt 0 ] && echo "installed units: ${changed[*]}"

if [ "$mode" != --rollback ]; then
  if ! "${admin[@]}" svc start steelcore-migrate; then
    echo "migrations failed, switching back" >&2
    "${admin[@]}" logs steelcore-migrate 30 >&2 || true
    [ -n "$previous" ] && switch "$previous"
    exit 1
  fi
  "${admin[@]}" logs steelcore-migrate 20 | grep -E 'applied|unchanged' | tail -n 4 || true
fi

for svc in steelcore steelcore-test; do
  "${admin[@]}" svc enable "$svc" >/dev/null 2>&1 || true
  "${admin[@]}" svc restart "$svc"
done
for timer in steelcore-backup.timer steelcore-status.timer; do
  "${admin[@]}" svc enable "$timer" >/dev/null 2>&1 || true
  "${admin[@]}" svc start "$timer"
done

# Refresh the collected status for the check below.
"${admin[@]}" svc start steelcore-status >/dev/null || true
sleep 2
bash "$dir/deploy/server/status.sh" || true

# Keep the newest releases (and always the active and the previous one).
ls -1t "$root" | grep -E '^[0-9a-f]{7,40}$' | tail -n +"$((keep + 1))" | while read -r old; do
  [ "$old" = "$rel" ] || [ "$old" = "$previous" ] || { rm -rf "${root:?}/$old"; echo "removed old release $old"; }
done
