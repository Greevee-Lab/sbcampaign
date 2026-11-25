#!/bin/bash
# SPDX-License-Identifier: AGPL-3.0-only
#
# Status check for the operator, readable over ssh without database access:
#
#   ssh <deploy-user>@<host> /opt/steelcore/releases/current/deploy/server/status.sh
#
# Checks: main service active, campaign listener bound to loopback only,
# protocol ping against the main service (handshake up to the XUID request, no
# database write), full handshake against the test instance (database round
# trip), the Steelbound gateways where configured (TLS, pin, ticket required),
# the collected status of steelcore-status.service (database ping) and the age
# of the newest backup. Prints one line per check and "ok" at the end,
# or "FAIL" and exits 1.
set -uo pipefail

release=$(cd "$(dirname "$0")/../.." && pwd -P)
status_file=${STATUS_FILE:-/var/lib/steelcore/status/status.txt}
port=${STEELCORE_PORT:-23457}
test_port=${STEELCORE_TEST_PORT:-23458}
test_xuid=${STEELCORE_TEST_XUID:-0x0009FFFF00000001}
max_status_age=${MAX_STATUS_AGE:-1800}
max_backup_age=${MAX_BACKUP_AGE:-93600}

failed=0
pass() { printf '  ok    %s\n' "$*"; }
fail() { printf '  FAIL  %s\n' "$*"; failed=1; }
info() { printf '  info  %s\n' "$*"; }

info "release $(cat "$release/COMMIT" 2>/dev/null | cut -c1-12)"

for unit in steelcore.service steelcore-backup.timer steelcore-status.timer; do
  state=$(systemctl is-active "$unit" 2>/dev/null)
  [ "$state" = active ] && pass "$unit active" || fail "$unit $state"
done
state=$(systemctl is-active steelcore-test.service 2>/dev/null)
info "steelcore-test.service $state"

# Listener: every socket on the campaign port must be bound to loopback.
listen=$(ss -Hltn "sport = :$port" | awk '{print $4}')
if [ -z "$listen" ]; then
  fail "no listener on port $port"
elif echo "$listen" | grep -Evq '^(127\.0\.0\.1|\[::1\]):'; then
  fail "listener on port $port is not loopback only: $(echo $listen)"
else
  pass "listener on port $port loopback only"
fi

client=$release/tools/handshake_client.py
if python3 "$client" --port "$port" --ping-only >/dev/null 2>&1; then
  pass "protocol ping on port $port"
else
  fail "protocol ping on port $port"
fi

if [ "$state" = active ]; then
  if python3 "$client" --port "$test_port" --xuid "$test_xuid" --period >/dev/null 2>&1; then
    pass "test instance handshake on port $test_port"
  else
    fail "test instance handshake on port $test_port"
  fi
fi

# Steelbound gateway (public TLS), where configured: listener up, TLS with the
# expected key (pin from the certificate), a request without ticket is refused.
# Touches no data.
gw_cert=${GATEWAY_CERT:-/etc/steelcore/gateway/server.crt}
check_gateway() {
  local config=$1 port=$2 pin
  [ -r "$config" ] || { info "gateway: $config not readable, skipped"; return; }
  grep -q '^\[gateway_steelbound\]' "$config" || { info "gateway: not configured in $(basename "$config")"; return; }
  if ! ss -Hltn "sport = :$port" | grep -q .; then
    fail "gateway: no listener on port $port"
    return
  fi
  pin=$(openssl x509 -in "$gw_cert" -pubkey -noout 2>/dev/null | openssl pkey -pubin -outform DER 2>/dev/null |
    sha256sum | cut -d' ' -f1)
  if python3 "$release/tools/gateway_client.py" --port "$port" --pin "$pin" --case missing --quiet >/dev/null 2>&1; then
    pass "gateway on port $port: TLS, pin ${pin:0:16}..., ticket required"
  else
    fail "gateway on port $port: TLS/pin/ticket check"
  fi
}
check_gateway /etc/steelcore/steelcore.toml "${GATEWAY_PORT:-47100}"
[ "$state" = active ] && check_gateway /etc/steelcore/steelcore-test.toml "${GATEWAY_TEST_PORT:-47103}"

if [ -r "$status_file" ]; then
  now=$(date +%s)
  get() { sed -n "s/^$1=//p" "$status_file" | head -n1; }
  age=$((now - $(get time)))
  [ "$age" -le "$max_status_age" ] && pass "collected status is ${age}s old" || fail "collected status is ${age}s old"
  for db in steelcore steelcore_test; do
    [ "$(get "db\.$db")" = ok ] &&
      pass "database $db reachable, $(get "db\.$db\.migrations") migrations, $(get "db\.$db\.procedures") procedures, round/turn $(get "db\.$db\.period")" ||
      fail "database $db not reachable"
  done
  bt=$(get 'backup\.steelcore\.time')
  if [ -n "$bt" ]; then
    bage=$((now - bt))
    [ "$bage" -le "$max_backup_age" ] && pass "newest backup $(get 'backup\.steelcore') is $((bage / 3600))h old" ||
      fail "newest backup is $((bage / 3600))h old"
  else
    fail "no backup of steelcore yet"
  fi
else
  fail "no collected status at $status_file"
fi

mem=$(systemctl show -P MemoryCurrent steelcore.slice 2>/dev/null)
[[ "$mem" =~ ^[0-9]+$ ]] && info "steelcore.slice memory $((mem / 1048576)) MiB of 512 MiB"

if [ "$failed" = 0 ]; then echo ok; else echo FAIL; exit 1; fi
