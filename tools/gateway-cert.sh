#!/usr/bin/env bash
# SPDX-License-Identifier: AGPL-3.0-only
#
# Key and self-signed certificate of the Steelbound gateway (TLS), using openssl.
# Clients pin the SHA-256 of the public key (SPKI), not the certificate, so the
# certificate may be self-signed and renewed with the same key without a client
# update.
#
#   gateway-cert.sh new   <dir> [days]    new key + certificate (EC P-256,
#                                         default 3650 days); prints the pin
#   gateway-cert.sh renew <dir> [days]    new certificate for the existing key
#                                         (pin unchanged)
#   gateway-cert.sh pin   <cert.pem>      SPKI pin of a certificate
#
# Files: <dir>/server.key (secret, mode 600) and <dir>/server.crt. A new key
# means a new pin: clients need it before the server switches (ship both pins
# for a while).
set -euo pipefail
umask 077
# Keep "/CN=..." intact under Git Bash on Windows (no effect elsewhere).
export MSYS_NO_PATHCONV=1

cmd=${1:-}
arg=${2:-}
[ -n "$cmd" ] && [ -n "$arg" ] || { sed -n '4,17p' "$0"; exit 2; }

pin() {
    openssl x509 -in "$1" -pubkey -noout | openssl pkey -pubin -outform DER | openssl dgst -sha256 -r | cut -d' ' -f1
}

certify() {
    local dir=$1 days=$2
    openssl req -new -x509 -key "$dir/server.key" -out "$dir/server.crt" -days "$days" \
        -subj "/CN=steelcore-gateway" -sha256
    chmod 644 "$dir/server.crt"
    echo "pin = $(pin "$dir/server.crt")"
}

case "$cmd" in
new)
    days=${3:-3650}
    mkdir -p "$arg"
    [ -e "$arg/server.key" ] && { echo "$arg/server.key exists (use renew)" >&2; exit 1; }
    openssl ecparam -name prime256v1 -genkey -noout | openssl pkcs8 -topk8 -nocrypt -out "$arg/server.key"
    chmod 600 "$arg/server.key"
    certify "$arg" "$days"
    ;;
renew)
    certify "$arg" "${3:-3650}"
    ;;
pin)
    pin "$arg"
    ;;
*)
    sed -n '4,17p' "$0"
    exit 2
    ;;
esac
