# Deploying sbcampaign

How this fork runs on its Linux server. The server is shared with other
services, so everything here works without root: a host admin tool installs
the unit files and forces user, group, cgroup slice and the file system
sandbox. No Rust toolchain is needed on the server; binaries come from the CI
artifact of a commit.

Host names and addresses are never stored in this repository. The deploy
script takes the ssh target from the environment.

## Layout on the server

| Path | Content | Writable by |
|---|---|---|
| `/opt/steelcore/releases/<commit12>/` | one release: `sbcampaign`, `sql/`, `deploy/`, `tools/`, `COMMIT` | deploy user |
| `/opt/steelcore/releases/current` | symlink to the active release | deploy user |
| `/opt/steelcore/units/` | unit files, installed by the admin tool | deploy user |
| `/etc/steelcore/steelcore.toml` | production config (database `steelcore`, port 23457) | deploy user, service |
| `/etc/steelcore/steelcore-test.toml` | test config (database `steelcore_test`, port 23458) | deploy user, service |
| `/etc/steelcore/core-api/` | Core API TLS: `ca.crt`, `server.crt`, `server.key` (0640) | deploy user, service |
| `/etc/steelcore/backup-pubkey.asc` | OpenPGP public key for backups | deploy user |
| `/etc/steelcore/restore/` | drop folder for a decrypted dump (normally empty) | deploy user, service |
| `/var/lib/steelcore/backups/` | encrypted backups (`<db>-<UTC time>.dump.gpg`) | service |
| `/var/lib/steelcore/status/` | collected status and table fingerprints | service |

The services run as user `steelcore` in `steelcore.slice` (one core, 512 MB in
total) and may only write `/var/lib/steelcore` and `/etc/steelcore`. The
database is a local PostgreSQL with peer login over the Unix socket (Linux user
`steelcore` = role `steelcore`). The deploy user has no database access; all
database work runs inside the services below.

## Units (`deploy/units/`)

| Unit | Type | What it does |
|---|---|---|
| `steelcore` | notify | campaign service, database `steelcore`, `127.0.0.1:23457` |
| `steelcore-test` | notify | test campaign, database `steelcore_test` (synthetic data only), `127.0.0.1:23458` |
| `steelcore-migrate` | oneshot | `sql/fork/migrations.list` on both databases, plus a one-time seed each |
| `steelcore-backup` + `.timer` | oneshot, daily 04:30 UTC | encrypted `pg_dump` of both databases, 14 days retention |
| `steelcore-restore` | oneshot, by hand | restore a decrypted dump from the drop folder |
| `steelcore-status` + `.timer` | oneshot, every 10 min | database ping, migrations, newest backup, fingerprints |

Both campaign services listen on loopback only: `bind_ip = "127.0.0.1"` in the
config and `IPAddressDeny=any` / `IPAddressAllow=localhost` in the unit.
Public access will go through a separate gateway.

## Deploy

Requirements on the operator machine: `gh` (logged in), `ssh`, `tar`, bash.

```sh
export STEELCORE_SSH=<deploy-user>@<host>
export STEELCORE_SSH_KEY=~/.ssh/<key>          # optional
deploy/deploy.sh <commit>                       # e.g. deploy/deploy.sh 47f73d3
```

The script finds the successful CI run of the commit, downloads the artifact
`sbcampaign-x86_64-<sha>`, uploads it to `releases/<commit12>/` and runs
`deploy/server/activate.sh` on the server, which

1. points `releases/current` at the new release,
2. copies changed unit files to `/opt/steelcore/units/` and installs them with
   the admin tool,
3. runs `steelcore-migrate` (on failure `current` is switched back and nothing
   is restarted),
4. restarts `steelcore` and `steelcore-test`, starts both timers,
5. prints the status check,
6. keeps the newest five releases.

Other commands:

```sh
deploy/deploy.sh --status                 # status check
deploy/deploy.sh --releases               # releases on the server, * = active
deploy/deploy.sh --rollback <commit12>    # back to an older release
```

A rollback only switches the symlink, reinstalls that release's units and
restarts the services. It never touches the database, so roll back only to
releases whose code understands the current schema.

### First setup

Once per server, before the first deploy (all as the deploy user):

```sh
cp deploy/config/steelcore.toml.example      /etc/steelcore/steelcore.toml
cp deploy/config/steelcore-test.toml.example /etc/steelcore/steelcore-test.toml
chmod 0640 /etc/steelcore/*.toml
mkdir -m 2770 /etc/steelcore/restore
# public key of the backup key pair, see "Backups"
cp backup-pubkey.asc /etc/steelcore/backup-pubkey.asc
```

## Migrations

`deploy/server/migrate.sh <database> [seed.sql]` applies the files listed in
`sql/fork/migrations.list` in order:

- `once`: applied exactly once. Changing such a file later is an error; add a
  new file instead.
- `repeatable`: applied again when the content changes (the upstream
  `CREATE OR REPLACE PROCEDURE` files).

Each file runs in one transaction together with its record in the table
`fork_migrations` (name, kind, sha256, time). A second run prints
`0 applied, N unchanged`. The seed runs once per database:
`sql/fork/preseason.sql` on production (one `settings` row so the service has a
campaign period; no player data), `sql/fork/test_seed.sql` on the test
database (synthetic data).

### Season start

The production database starts with a pre-season row (round 1, turn 1, one
week) that the service rolls over weekly. On launch day the operator sets the
real start, inside the service context, e.g. with a one-off migration file
that runs `TRUNCATE settings; INSERT INTO settings VALUES (1, 1, <end of turn 1>);`.

## Backups

`steelcore-backup` pipes `pg_dump --format=custom` straight into
`gpg --encrypt --recipient-file /etc/steelcore/backup-pubkey.asc`. Why gpg:
it is installed on the server, encrypts a stream to a public key without a
keyring or trust setup, and authenticates the ciphertext. (`openssl cms` would
need an X.509 certificate and buffers the whole file on decryption.) The
private key never goes to the server: the server can write backups but cannot
read them, which matters because the backup folder is readable for the deploy
group.

Create the key pair on the operator machine (once):

```sh
export GNUPGHOME=~/.steelcore/gnupg; mkdir -p -m 700 "$GNUPGHOME"
gpg --quick-gen-key 'steelcore backup' ed25519 cert never
gpg --quick-add-key <fingerprint> cv25519 encr never
gpg --armor --export <fingerprint>             > backup-pubkey.asc       # to the server
gpg --armor --export-secret-keys <fingerprint> > backup-SECRET.asc       # keep offline
```

Keep the secret key safe and offline (password manager, encrypted storage);
without it the backups are useless. Optionally protect it with a passphrase:
`gpg --passwd <fingerprint>`.

Run a backup now: `sudo steelnet-admin svc start steelcore-backup`.

## Restore

1. Fetch and decrypt a backup on the operator machine:
   ```sh
   ssh "$STEELCORE_SSH" cat /var/lib/steelcore/backups/<db>-<time>.dump.gpg > b.dump.gpg
   GNUPGHOME=~/.steelcore/gnupg gpg --decrypt -o <db>.dump b.dump.gpg
   ```
2. Stop the service that uses the database
   (`steelcore` for `steelcore`, `steelcore-test` for `steelcore_test`):
   `sudo steelnet-admin svc stop steelcore-test`
3. Upload the plain dump as `<db>.dump` into the drop folder:
   `ssh "$STEELCORE_SSH" 'umask 027; cat > /etc/steelcore/restore/steelcore_test.dump' < steelcore_test.dump`
   For the production database also create `/etc/steelcore/restore/CONFIRM_PRODUCTION`.
4. `sudo steelnet-admin svc start steelcore-restore` - empties the database
   (drops and recreates schema `public`), runs `pg_restore`, and deletes the
   plain dump and the confirmation in every case.
5. Start the service again, delete the local plain dump.
6. Compare: `sudo steelnet-admin svc start steelcore-status`, then
   `/var/lib/steelcore/status/fingerprint-<db>.txt` (row count and md5 per
   table) against the copy taken before the backup.

## Status check

```sh
ssh "$STEELCORE_SSH" bash /opt/steelcore/releases/current/deploy/server/status.sh
```

Checks the main service and both timers, that port 23457 is bound to loopback
only, a protocol ping on the main service (`tools/handshake_client.py
--ping-only`: Ping/Pong and the XUID request, no database write), a full
handshake with the period request on the test instance, the collected database
status (at most 30 minutes old) and the age of the newest backup (at most 26
hours). Prints `ok`, or `FAIL` with exit code 1.

## Core API

The network Core API (`steelcore.v1`, see `FORK.md`) starts only if the config
has a `[core_api]` section. It listens on loopback only (production
`127.0.0.1:47101`, test `127.0.0.1:47102`); the units additionally allow only
localhost. A public port follows only when a provider on another host is
connected.

**Certificates** (on the operator's machine; the CA key never goes to a server):

```sh
tools/steelcore-ca.sh init   ~/.steelcore/core-api-ca
tools/steelcore-ca.sh server ~/.steelcore/core-api-ca core-api-server
tools/steelcore-ca.sh client ~/.steelcore/core-api-ca steelnet-eu      # prints cert_sha256
```

Copy `ca.crt`, `core-api-server.crt` and `core-api-server.key` to
`/etc/steelcore/core-api/` as `ca.crt`, `server.crt`, `server.key` (mode 0640,
group `steelcore`). The provider gets its own `<access>.crt`/`.key` plus
`ca.crt`, and its fingerprint goes into `[[core_api.access]]` with the scopes
it needs (see the config examples). Restart the service after config changes.

- **Renew** (client certificates default to 397 days, server 825): issue a
  new certificate, add its fingerprint as a second access (other name),
  restart, switch the provider, remove the old access, restart.
- **Revoke:** remove the access and restart. Revoked accesses stay in the
  table `api_access` with `revoked_at` (audit rows keep their name).
- **Server certificate:** replace the files and restart; providers trust the
  CA, not the certificate.
- **Lost or leaked CA key:** create a new CA, reissue all certificates,
  replace `ca.crt` on both sides.

Smoke test on the server (as a user who can read the provider key):

```sh
python3 tools/core_api_client.py --port 47102 --ca ca.crt --cert steelnet-test.crt --key steelnet-test.key \
    steelcore.v1.PlayerService/CheckDisplayName '{"name": "Test Pilot"}'
```

Audit: `SELECT at, access_id, method, outcome FROM audit_log WHERE access_id IS NOT NULL ORDER BY at DESC LIMIT 20;`

## Campaign off (emergency)

`sudo steelnet-admin svc stop steelcore` stops the campaign; clients cannot
connect until it is started again. To keep it off across reboots also run
`sudo steelnet-admin svc disable steelcore`. Data stays untouched.

## Logs

`sudo steelnet-admin logs steelcore [lines]` (also `steelcore-test`,
`steelcore-migrate`, `steelcore-backup`, `steelcore-restore`,
`steelcore-status`). Client log lines carry the XUID and name of the account
(upstream behaviour); keep the journal retention of the host short.
