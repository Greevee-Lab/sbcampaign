#!/usr/bin/env bash
# SPDX-License-Identifier: AGPL-3.0-only
#
# Small certificate authority for the Core API (mutual TLS), using openssl.
# Run it on the operator's machine. The CA key never goes to a server.
#
#   steelcore-ca.sh init   <dir>                      new CA (EC P-256, 5 years)
#   steelcore-ca.sh server <dir> <name> [days]        server certificate for
#                                                     localhost/127.0.0.1 (default 825 days)
#   steelcore-ca.sh client <dir> <access> [days]      client certificate of one provider
#                                                     access (default 397 days); prints
#                                                     the cert_sha256 line for [[core_api.access]]
#   steelcore-ca.sh fingerprint <cert.pem>            cert_sha256 of a certificate
#
# Files: <dir>/ca.key (secret, mode 600), <dir>/ca.crt (public), and per name
# <dir>/<name>.key (secret) + <dir>/<name>.crt. Renewal = issue again with the
# same name, add the new fingerprint to the configuration, restart, remove the
# old fingerprint after the provider switched.
set -euo pipefail
umask 077

cmd=${1:-}
dir=${2:-}
[ -n "$cmd" ] && [ -n "$dir" ] || { sed -n '4,17p' "$0"; exit 2; }

fingerprint() {
    openssl x509 -in "$1" -outform DER | openssl dgst -sha256 -r | cut -d' ' -f1
}

name_ok() {
    [[ "$1" =~ ^[a-z0-9][a-z0-9_.-]{1,63}$ ]] || { echo "invalid name: $1 (a-z 0-9 _ . -)" >&2; exit 2; }
}

issue() { # name days extfile-content
    local name=$1 days=$2 ext=$3
    [ -f "$dir/ca.key" ] || { echo "no CA in $dir (run init)" >&2; exit 2; }
    openssl ecparam -name prime256v1 -genkey -noout -out "$dir/$name.key.tmp"
    openssl pkcs8 -topk8 -nocrypt -in "$dir/$name.key.tmp" -out "$dir/$name.key"
    rm -f "$dir/$name.key.tmp"
    openssl req -new -key "$dir/$name.key" -subj "/CN=$name" -out "$dir/$name.csr"
    printf '%s\n' "$ext" > "$dir/$name.ext"
    openssl x509 -req -in "$dir/$name.csr" -CA "$dir/ca.crt" -CAkey "$dir/ca.key" -CAcreateserial \
        -days "$days" -sha256 -extfile "$dir/$name.ext" -out "$dir/$name.crt" 2>/dev/null
    rm -f "$dir/$name.csr" "$dir/$name.ext"
    chmod 644 "$dir/$name.crt"
}

case "$cmd" in
init)
    mkdir -p "$dir"
    [ -f "$dir/ca.key" ] && { echo "$dir/ca.key exists" >&2; exit 2; }
    openssl ecparam -name prime256v1 -genkey -noout -out "$dir/ca.key"
    openssl req -x509 -new -key "$dir/ca.key" -sha256 -days 1826 -subj "/CN=steelcore core api CA" \
        -addext "basicConstraints=critical,CA:TRUE" -addext "keyUsage=critical,keyCertSign,cRLSign" \
        -out "$dir/ca.crt"
    chmod 644 "$dir/ca.crt"
    echo "CA: $dir/ca.crt (keep $dir/ca.key offline)"
    ;;
server)
    name=${3:?name}; name_ok "$name"
    issue "$name" "${4:-825}" "basicConstraints=critical,CA:FALSE
keyUsage=critical,digitalSignature
extendedKeyUsage=serverAuth
subjectAltName=DNS:localhost,IP:127.0.0.1"
    echo "server: $dir/$name.crt $dir/$name.key"
    ;;
client)
    name=${3:?access name}; name_ok "$name"
    issue "$name" "${4:-397}" "basicConstraints=critical,CA:FALSE
keyUsage=critical,digitalSignature
extendedKeyUsage=clientAuth"
    echo "client: $dir/$name.crt $dir/$name.key"
    echo "cert_sha256 = \"$(fingerprint "$dir/$name.crt")\""
    ;;
fingerprint)
    fingerprint "$dir"
    ;;
*)
    sed -n '4,17p' "$0"; exit 2
    ;;
esac
