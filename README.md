<!-- SPDX-License-Identifier: AGPL-3.0-only -->
# sbcampaign (fork)

> **Fork of [Quantx/sbcampaign](https://github.com/Quantx/sbcampaign).**
> The campaign server, its protocol work, rules, clock and database are the
> work of **QuantX**. Thank you for building it, for keeping the
> *Line of Contact* campaign alive on Insignia, and for allowing this fork.

## What this fork is

`sbcampaign` is a server for the campaign of *Steel Battalion: Line of
Contact*: accounts and pilots, factions, rounds and turns, war tide,
leaderboards, awards, garage and shop. It is written in Rust (tokio, sqlx)
and stores its state in PostgreSQL.

This fork is the code base of the campaign core of
**[steelcore](https://github.com/Greevee-Lab/steelcore)**, the plan for one
shared campaign across clients. It is meant to bring the campaign to
**Steelbound** players. Player identity, a core boundary and a Steelbound
gateway will be added here as separate modules on top of the upstream logic.

- The **Insignia campaign is not affected.** It keeps running from the
  upstream code on its own server.
- Changes stay additive so upstream can be merged regularly, and general fixes
  are offered back to QuantX. See **[FORK.md](FORK.md)** for the rules, the
  list of changes and the license status.

## Building

Requirements: a current stable Rust toolchain (edition 2024).

```sh
# Production build: logs to the systemd journal and notifies systemd.
# Needs libsystemd (e.g. package libsystemd-dev) and pkg-config.
cargo build --release

# Build without systemd, e.g. for local testing on any desktop OS.
# Logs go to stderr instead.
cargo build --no-default-features

cargo test --no-default-features
```

CI builds a release binary on every push and pull request, runs the tests,
plays a handshake against a test database and uploads the binary together
with `sql/` as an artifact.

## Running locally

1. Start a PostgreSQL server (16 or newer) and create a database and role,
   for example `loc_campaign` owned by `sbcampaign`.
2. Load the schema, the stored procedures and the synthetic seed:

   ```sh
   for f in sql/fork/base_schema.sql sql/set_award_rank.sql sql/give_turn_award.sql \
            sql/give_round_award.sql sql/shop_drop.sql sql/fork/test_seed.sql; do
     psql -v ON_ERROR_STOP=1 -U sbcampaign -d loc_campaign -f "$f"
   done
   ```

   `sql/fork/base_schema.sql` is reconstructed from the queries in the code;
   upstream has not published its schema yet.
3. Copy `loc.toml` and point it at your database. Without `host` the server
   uses the Unix socket (`socket`, default `/var/run/postgresql`). With
   `host` it connects over TCP:

   ```toml
   [network]
   bind_port = 23457
   bind_ip = "127.0.0.1"

   [database]
   username = "sbcampaign"
   app_name = "LoC Campaign"
   database = "loc_campaign"
   host = "127.0.0.1"
   port = 5432
   # password = "..."   # only if your server requires one
   ```

4. Start the server and run the handshake test client:

   ```sh
   cargo run --no-default-features -- my-loc.toml
   python3 tools/handshake_client.py --period
   ```

   The client plays `0x1010 Ping → 0x2010 Pong → 0x1020 XUID → 0x2020 XUID →
   0x3013 Accept` with a synthetic XUID and then asks for the campaign period.

## Upstream

There is no upstream README yet. Upstream repository:
<https://github.com/Quantx/sbcampaign> (default branch `master`).
