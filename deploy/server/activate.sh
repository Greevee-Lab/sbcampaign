#!/bin/bash
# SPDX-License-Identifier: AGPL-3.0-only
#
# Activate an unpacked release on the server. Runs as the deploy user (member
# of group steelcore), called by deploy/deploy.sh:
#
#   bash /opt/steelcore/releases/<release>/deploy/server/activate.sh <release>
#   bash /opt/steelcore/releases/<release>/deploy/server/activate.sh <release> --rollback
#   bash /opt/steelcore/releases/<release>/deploy/server/activate.sh <release> --test
#
# Steps: point releases/current and releases/test at <release>, install
# changed unit files through the host's admin tool, run the migrations
# (steelcore-migrate, both databases), restart the services, run the status
# check, keep the newest KEEP_RELEASES releases. If the migrations fail, the
# links are switched back and the services keep the previous release.
# --rollback skips the migrations (a rollback never changes the database).
#
# --test touches the test instance only: it points releases/test (not
# current) at <release>, installs only the steelcore-test* units, migrates
# only the test database (steelcore-test-migrate) and restarts only
# steelcore-test. Production keeps its release, units and database.
#
# Environment (tests only): STEELCORE_RELEASES, STEELCORE_UNITS, STEELCORE_ETC,
# STEELCORE_ADMIN (command instead of the host admin tool), STEELCORE_SKIP_STATUS=1.
set -euo pipefail

root=${STEELCORE_RELEASES:-/opt/steelcore/releases}
units_dir=${STEELCORE_UNITS:-/opt/steelcore/units}
etc=${STEELCORE_ETC:-/etc/steelcore}
read -r -a admin <<<"${STEELCORE_ADMIN:-sudo -n /usr/local/sbin/steelnet-admin}"
keep=${KEEP_RELEASES:-5}

rel=${1:?usage: activate.sh <release> [--rollback|--test]}
mode=${2:-}
case "$mode" in '' | --rollback | --test) ;; *) echo "unknown mode: $mode" >&2; exit 2 ;; esac
[[ "$rel" =~ ^[0-9a-f]{7,40}$ ]] || { echo "invalid release name: $rel" >&2; exit 2; }
dir=$root/$rel
[ -x "$dir/sbcampaign" ] || chmod 0755 "$dir/sbcampaign"
[ -f "$dir/COMMIT" ] || { echo "$dir/COMMIT missing" >&2; exit 2; }

for f in "$etc/steelcore.toml" "$etc/steelcore-test.toml" "$etc/backup-pubkey.asc"; do
  [ -r "$f" ] || { echo "missing $f (see deploy/README.md, first setup)" >&2; exit 2; }
done

previous=$(readlink "$root/current" 2>/dev/null || true)
previous_test=$(readlink "$root/test" 2>/dev/null || true)
# Before the first test deploy the test instance ran the release of current.
previous_test=${previous_test:-$previous}
# switch <link> <release>
switch() {
  ln -sfn "$2" "$root/.$1.new"
  mv -T "$root/.$1.new" "$root/$1"
  echo "$1 -> $2"
}
switch_back() {
  if [ "$mode" != --test ]; then
    if [ -n "$previous" ]; then switch current "$previous"; fi
  fi
  if [ -n "$previous_test" ]; then switch test "$previous_test"; fi
}
if [ "$mode" = --test ]; then
  switch test "$rel"
  unit_glob=steelcore-test*.service
else
  switch current "$rel"
  switch test "$rel"
  unit_glob=steelcore*.service
fi

# Unit files: copy and install only what changed. A timer is installed together
# with its service.
changed=()
for src in "$dir"/deploy/units/$unit_glob; do
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

if [ "$mode" = --test ]; then
  migrate=steelcore-test-migrate
  services=(steelcore-test)
else
  migrate=steelcore-migrate
  services=(steelcore steelcore-test)
fi
if [ "$mode" != --rollback ]; then
  if ! "${admin[@]}" svc start "$migrate"; then
    echo "migrations failed, switching back" >&2
    "${admin[@]}" logs "$migrate" 30 >&2 || true
    switch_back
    exit 1
  fi
  "${admin[@]}" logs "$migrate" 40 | grep -E "applied|unchanged" | tail -n 4 || true
fi

for svc in "${services[@]}"; do
  "${admin[@]}" svc enable "$svc" >/dev/null 2>&1 || true
  "${admin[@]}" svc restart "$svc"
done
if [ "$mode" != --test ]; then
  for timer in steelcore-backup.timer steelcore-status.timer; do
    "${admin[@]}" svc enable "$timer" >/dev/null 2>&1 || true
    "${admin[@]}" svc start "$timer"
  done
fi

# Refresh the collected status for the check below.
if [ "${STEELCORE_SKIP_STATUS:-}" != 1 ]; then
  "${admin[@]}" svc start steelcore-status >/dev/null || true
  sleep 2
  bash "$dir/deploy/server/status.sh" || true
fi

# Keep the newest releases (and always the ones current and test point at
# and the previous ones).
current_now=$(readlink "$root/current" 2>/dev/null || true)
ls -1t "$root" | grep -E '^[0-9a-f]{7,40}$' | tail -n +"$((keep + 1))" | while read -r old; do
  case " $rel $current_now $previous $previous_test " in
  *" $old "*) ;;
  *) rm -rf "${root:?}/$old"; echo "removed old release $old" ;;
  esac
done
