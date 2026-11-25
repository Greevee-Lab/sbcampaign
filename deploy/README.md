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
| `/etc/steelcore/gateway/` | Steelbound gateway TLS: `server.crt`, `server.key` (0640) | deploy user, service |
| `/etc/steelcore/annul/` | request file of `steelcore-annul` (normally empty) | deploy user, service |
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
| `steelcore-annul` | oneshot, by hand | annul the reported results of one identity (request file) |
| `steelcore-status` + `.timer` | oneshot, every 10 min | database ping, migrations, newest backup, fingerprints |

The original protocol listens on loopback only (`bind_ip = "127.0.0.1"`): it
trusts the XUID the game reports. Public access goes through the Steelbound
gateway (TLS + ticket, production port 47100, test 47103, see below), which
refuses to start unless `bind_ip` is a loopback address. Because of the public
gateway port the units no longer carry `IPAddressDeny=any`; the Core API stays
on loopback through its bind address.

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
hours). Where a config has `[gateway_steelbound]`, it also checks the gateway
port: listener up, TLS with the key of `/etc/steelcore/gateway/server.crt`
(pin), and a request without ticket is refused (no data touched). Prints
`ok`, or `FAIL` with exit code 1.

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

## Steelbound gateway

Public TLS access for Steelbound players (`FORK.md`, "Steelbound gateway").
It starts only if the config has a `[gateway_steelbound]` section, a `[ticket]`
section with at least one public key, and `[network] bind_ip` on loopback.

| Instance | Port | Ticket campaign | Ticket key |
|---|---|---|---|
| `steelcore` | 47100/tcp public | `steelbound-s1` | production key of the issuer (steelnet) |
| `steelcore-test` | 47103/tcp public | `steelbound-test` | separate test key |

The test instance has its own public port so the Steelbound client and testers
can use the test season from outside while production runs; its database only
holds synthetic or test data.

**Key and certificate** (on the operator's machine; one key for both ports =
one pin for clients):

```sh
tools/gateway-cert.sh new ~/.steelcore/gateway     # prints the pin
```

Copy `server.crt` and `server.key` to `/etc/steelcore/gateway/` (mode 0640,
group `steelcore`). The pin goes into the client configuration. `renew`
issues a new certificate for the same key (pin unchanged); a new key needs a
client update first (ship both pins for a while).

**Ticket keys:** the issuer's public key (64 hex digits) goes into `[ticket]
public_keys`; private keys never reach this service. For the test instance
use a separate test key pair:

```sh
python3 -c "import os; print(os.urandom(32).hex())" > ~/.steelcore/ticket-test.key   # keep private
python3 tools/gateway_client.py --key-file ~/.steelcore/ticket-test.key --print-public-key
```

**Open the port** once the listener runs: `sudo steelnet-admin port open 47100
tcp` (47103 for the test instance).

**Test from outside** (synthetic account ids only):

```sh
python3 tools/gateway_client.py --host <server> --port 47103 --pin <pin> \n    --key-file ~/.steelcore/ticket-test.key --account 900001 --case all
```

`valid` registers, loads and saves a pilot and chooses a faction (reporting a
fake `qwUserID` on purpose); `expired`, `forged`, `tampered` and `missing` must
be refused. Exit code 0 = every case as expected. Against production use a
ticket from the issuer (`--ticket-file`, `--case valid`) or only the refusal
cases (no key needed).

**Limits** (`[gateway_steelbound.limits]`, all optional):

| Key | Default | Meaning |
|---|---|---|
| `max_connections` | 256 | open connections in total |
| `max_connections_per_source` | 8 | open connections per IPv4 address / IPv6 /64 |
| `source_connect_rate`, `source_connect_burst` | 2/s, 30 | new connections per source |
| `source_failure_rate`, `source_failure_burst` | 0.1/s, 10 | refused tickets per source before it is dropped before TLS |
| `identity_connect_rate`, `identity_connect_burst` | 2/s, 30 | accepted connections per identity (answer: busy) |
| `identity_command_rate`, `identity_command_burst` | 5/s, 40 | commands per identity (answer: `0x3015`) |
| `results_per_hour`, `identity_result_burst` | 60, 10 | applied results per identity; more are answered but not applied |
| `max_commands_per_connection` | 8 | the game sends one per connection |
| `tls_handshake_timeout_secs`, `prelude_timeout_secs`, `connection_lifetime_secs` | 10, 5, 60 | time limits |
| `max_user_stat` | 100000 | each `AddUserPoint` stat per report |
| `max_mission_points` | 10000 | `AddMissionPoint` points per report |
| `max_map` | 3 | highest map number within a turn |
| `turn_grace` | 1 | results may name the current turn or this many before it (same round) |
| `max_vt_list` | 64 | entries of a VT list |

The result bounds are deliberately rough; tighten them once real values are
known. Every write command leaves an audit row:

```sql
SELECT at, method, outcome, identity_id FROM audit_log
WHERE source = 'gateway-steelbound' ORDER BY at DESC LIMIT 20;
```

Log lines of the gateway carry a session number and the account id, never
addresses, tickets or game data.

## Annul results

Takes back the results an identity reported through the gateway (leaderboard
stats and war tide points from the ledger `gateway_result`), optionally
suspends the identity, and writes an audit row. Runs only on the server, as
the oneshot `steelcore-annul` reading one request line. From the operator's
machine:

```sh
bash deploy/deploy.sh --annul steelcore steelbound 4242            # dry run: counts only
bash deploy/deploy.sh --annul steelcore steelbound 4242 apply suspend
```

(`deploy.sh` writes `/etc/steelcore/annul/request`, starts the unit and shows
its log.) Running it again takes nothing back twice. Awards already handed
out at a turn or round end are not recomputed. Reinstate a suspended identity
with the Core API (`AdminService.SetIdentitySuspended`).

## Campaign off (emergency)

`sudo steelnet-admin svc stop steelcore` stops the campaign; clients cannot
connect until it is started again. To keep it off across reboots also run
`sudo steelnet-admin svc disable steelcore`. Data stays untouched.

## Logs

`sudo steelnet-admin logs steelcore [lines]` (also `steelcore-test`,
`steelcore-migrate`, `steelcore-backup`, `steelcore-restore`,
`steelcore-annul`, `steelcore-status`). Client log lines carry the XUID and name of the account
(upstream behaviour); keep the journal retention of the host short.
