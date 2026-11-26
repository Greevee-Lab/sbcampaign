#!/bin/bash
# SPDX-License-Identifier: AGPL-3.0-only
#
# Checks deploy/server/activate.sh in a scratch layout with a stub instead of
# the host admin tool: a normal deploy switches both instances, a test deploy
# (--test) touches only the test instance (link, units, migration, restart),
# a failed test migration switches the test link back. No database, no
# systemd, nothing outside a temporary directory. Needs real symlinks (Linux).
#
#   tools/test_activate.sh
set -euo pipefail
src=$(cd "$(dirname "$0")/.." && pwd -P)
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

mkdir -p "$work/releases" "$work/units" "$work/etc"
touch "$work/etc/steelcore.toml" "$work/etc/steelcore-test.toml" "$work/etc/backup-pubkey.asc"
cat >"$work/admin.sh" <<'EOF'
#!/bin/bash
echo "$*" >>"$WORK/calls"
[ "$*" = "svc start steelcore-test-migrate" ] && [ "${FAIL_MIGRATE:-}" = 1 ] && exit 1
exit 0
EOF
export WORK=$work STEELCORE_RELEASES=$work/releases STEELCORE_UNITS=$work/units STEELCORE_ETC=$work/etc \
  STEELCORE_ADMIN="bash $work/admin.sh" STEELCORE_SKIP_STATUS=1

# mkrel <name> [marker]: a release; the marker changes the unit files.
mkrel() {
  local d=$work/releases/$1
  mkdir -p "$d/deploy"
  cp -r "$src/deploy/server" "$src/deploy/units" "$d/deploy/"
  printf '#!/bin/sh\n' >"$d/sbcampaign"
  echo "$1" >"$d/COMMIT"
  if [ -n "${2:-}" ]; then
    for u in "$d"/deploy/units/*.service; do echo "# $2" >>"$u"; done
  fi
}
link() { readlink "$work/releases/$1"; }
fail() { echo "FAIL: $*" >&2; cat "$work/calls" >&2 || true; exit 1; }
called() { grep -qx "$1" "$work/calls"; }

mkrel aaaaaaa1
bash "$work/releases/aaaaaaa1/deploy/server/activate.sh" aaaaaaa1 >/dev/null
[ "$(link current)" = aaaaaaa1 ] && [ "$(link test)" = aaaaaaa1 ] || fail "normal deploy: links"
called "svc start steelcore-migrate" || fail "normal deploy: migrate"
called "svc restart steelcore" && called "svc restart steelcore-test" || fail "normal deploy: restarts"
called "unit install steelcore" && called "unit install steelcore-test-maint" || fail "normal deploy: units"

# Test deploy: production keeps release, units, database and process.
mkrel bbbbbbb2 v2
: >"$work/calls"
bash "$work/releases/bbbbbbb2/deploy/server/activate.sh" bbbbbbb2 --test >/dev/null
[ "$(link current)" = aaaaaaa1 ] || fail "test deploy changed current"
[ "$(link test)" = bbbbbbb2 ] || fail "test deploy: test link"
called "svc start steelcore-test-migrate" || fail "test deploy: test migration"
called "svc restart steelcore-test" || fail "test deploy: test restart"
! called "svc start steelcore-migrate" || fail "test deploy ran the migration of both databases"
! called "svc restart steelcore" || fail "test deploy restarted production"
called "unit install steelcore-test" || fail "test deploy: test unit"
! grep -Eq '^unit install steelcore(-maint|-migrate|-backup|-status|-restore|-annul)?$' "$work/calls" ||
  fail "test deploy installed a production unit"
grep -q '^# v2$' "$work/units/steelcore-test.service" || fail "test unit not copied"
! grep -q '^# v2$' "$work/units/steelcore.service" || fail "production unit copied"

# Failed test migration: the test link goes back, production untouched.
mkrel ccccccc3 v3
: >"$work/calls"
if FAIL_MIGRATE=1 bash "$work/releases/ccccccc3/deploy/server/activate.sh" ccccccc3 --test >/dev/null 2>&1; then
  fail "failed test migration reported success"
fi
[ "$(link test)" = bbbbbbb2 ] && [ "$(link current)" = aaaaaaa1 ] || fail "failed test migration: links"
! called "svc restart steelcore-test" || fail "failed test migration restarted the test instance"

# Normal deploy after the test: both on the new release.
: >"$work/calls"
bash "$work/releases/bbbbbbb2/deploy/server/activate.sh" bbbbbbb2 >/dev/null
[ "$(link current)" = bbbbbbb2 ] && [ "$(link test)" = bbbbbbb2 ] || fail "promote: links"
called "unit install steelcore" || fail "promote: production unit"

echo "activate.sh: normal, test-only, failed test migration and promote ok"
