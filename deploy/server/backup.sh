#!/bin/bash
# SPDX-License-Identifier: AGPL-3.0-only
#
# Encrypted database backup, run by steelcore-backup.service (daily timer).
#
# For every database in BACKUP_DBS: pg_dump (custom format, compressed) piped
# straight into gpg, encrypted to the public key in BACKUP_PUBKEY. The private
# key never touches the server, so the server can write backups but not read
# them. Files older than BACKUP_KEEP_DAYS are removed.
#
# Environment (defaults in brackets):
#   BACKUP_DBS        databases to dump            [steelcore]
#   BACKUP_DIR        target directory             [/var/lib/steelcore/backups]
#   BACKUP_PUBKEY     armored OpenPGP public key   [/etc/steelcore/backup-pubkey.asc]
#   BACKUP_KEEP_DAYS  retention in days            [14]
set -euo pipefail

dbs=${BACKUP_DBS:-steelcore}
dir=${BACKUP_DIR:-/var/lib/steelcore/backups}
pubkey=${BACKUP_PUBKEY:-/etc/steelcore/backup-pubkey.asc}
keep=${BACKUP_KEEP_DAYS:-14}

[ -r "$pubkey" ] || { echo "missing public key $pubkey" >&2; exit 2; }
[[ "$keep" =~ ^[0-9]+$ ]] || { echo "BACKUP_KEEP_DAYS must be a number" >&2; exit 2; }
umask 027
mkdir -p "$dir"

# gpg needs a home for its keybox; a throwaway one keeps the service stateless.
GNUPGHOME=$(mktemp -d)
export GNUPGHOME
trap 'rm -rf "$GNUPGHOME"; rm -f "$dir"/*.part' EXIT

stamp=$(date -u +%Y%m%dT%H%M%SZ)
for db in $dbs; do
  [[ "$db" =~ ^[a-z_][a-z0-9_]*$ ]] || { echo "invalid database name: $db" >&2; exit 2; }
  out=$dir/$db-$stamp.dump.gpg
  pg_dump --format=custom --no-owner --no-privileges -d "$db" |
    gpg --batch --quiet --yes --no-tty --trust-model always \
      --recipient-file "$pubkey" --compress-algo none \
      --output "$out.part" --encrypt
  mv "$out.part" "$out"
  echo "backup $db -> $(basename "$out") ($(stat -c %s "$out") bytes)"
done

find "$dir" -maxdepth 1 -type f -name '*.dump.gpg' -mtime +"$keep" -print -delete |
  sed 's|^.*/|removed old backup |'
