<!-- SPDX-License-Identifier: AGPL-3.0-only -->
# How this fork works

This repository is a fork of [Quantx/sbcampaign](https://github.com/Quantx/sbcampaign),
the Line of Contact campaign server written by QuantX. It is the implementation
of the campaign core of [steelcore](https://github.com/Greevee-Lab/steelcore).

## License

**Upstream license pending; permission to fork and develop granted by the author.**

- The upstream code has no license file yet. This fork does not add one for
  the upstream code and does not relicense it.
- New files written for this fork carry an
  `SPDX-License-Identifier: AGPL-3.0-only` header, the license of steelcore.
- Once upstream adds a license, this section is updated.

## Rules

1. **Add, don't rewrite.** New features go into new modules or crates and new
   SQL files (`sql/fork/` and later migrations). Upstream files are changed
   only where a hook is unavoidable, and as little as possible, so that
   merging upstream stays easy.
2. **Stay in sync with upstream.** Upstream's default branch is `master`.

   ```sh
   git remote add upstream https://github.com/Quantx/sbcampaign   # once
   git fetch upstream
   git merge upstream/master
   ```

3. **Give back.** General fixes and improvements that are useful upstream are
   prepared on their own branch, based on `upstream/master`, and offered to
   QuantX as a pull request. Nothing is ever pushed to the upstream repository
   directly.
4. **Synthetic data only.** Tests, seeds and examples never contain real
   player data (XUIDs, gamertags, pilot data, IP addresses).
5. **The live Insignia campaign is not affected.** It keeps running on the
   original server from the upstream code.

## Changes compared to upstream

| Change | Files | Upstream PR candidate |
|---|---|---|
| Optional systemd: Cargo feature `journal` (default on). `--no-default-features` logs to stderr, skips sd_notify, stops on Ctrl+C where Unix signals are missing | `Cargo.toml`, `src/main.rs`, `src/client.rs` | yes, branch `optional-systemd` |
| Optional PostgreSQL connection over TCP (`database.host`, `database.port`, `database.password`); Unix socket stays the default | `src/main.rs` | yes, branch `optional-systemd` |
| Fix: decoder waited for `length` instead of `4 + length` bytes, so a frame split across reads could panic the client task; with unit test | `src/command/rx.rs` | yes, branch `fix-partial-frame` |
| Reconstructed base schema and synthetic seed for local tests and CI | `sql/fork/` | possibly (an official schema dump would replace it) |
| Handshake test client | `tools/handshake_client.py` | possibly |
| CI: build, test, handshake test, artifact | `.github/workflows/ci.yml` | no |
| Fork documentation | `README.md`, `FORK.md` | no |
| Players and identities: tables `provider`, `player`, `identity`, `audit_log`; backfill of existing accounts (ADR-0002) | `sql/fork/identity.sql`, `sql/fork/migrations.list`, `tools/test_identity_backfill.sh` | no |
| Core boundary: trait `CampaignCore` with every campaign operation, dispatcher for the original protocol, no database access outside the core (ADR-0001, enforced by a test) | `src/core_api/` | no |
| Campaign core: adapter that puts the upstream commands behind `CampaignCore`, identity resolution | `src/campaign_core/` | no |
| Hook: the client stream is boxed (`BoxedTransport`) so the protocol code also runs over TLS or in tests; `codec`/`session_id` visible in the crate; module declarations | `src/client.rs`, `src/main.rs` | no |
| CI: database tests against a migrated test database, identity backfill check | `.github/workflows/ci.yml` | no |
| Core API `steelcore.v1`: Protobuf contract, gRPC and Connect-JSON bindings on one mutual-TLS listener (`[core_api]`), provider accesses with scopes, rate limit, idempotency keys, write audit; display names (gamertag rules, look-alike check, blocklist, change limit, reservation); identity suspension; `ProfileCore` boundary with PostgreSQL store | `proto/`, `build.rs`, `src/core_api/names.rs`, `src/core_api/profile.rs`, `src/core_api/net/`, `src/campaign_core/profile.rs`, `sql/fork/core_api.sql`, `tools/steelcore-ca.sh`, `tools/core_api_client.py`, `src/main.rs` (start the listener), `Cargo.toml` | no |
| Sign-in tickets: verification of Ed25519 tickets from an identity provider (signature, issuer, campaign, lifetime, key rotation), `[ticket]` configuration section, shared test vectors from the issuer | `src/ticket/`, `testdata/`, `deploy/config/*.example`, `Cargo.toml` (`ed25519-dalek`) | no |
| Steelbound gateway: public TLS listener (`[gateway_steelbound]`), prelude with the sign-in ticket, then the unchanged original protocol for the identity from the ticket (reported `qwUserID` ignored); rate limits per source and identity, connection and time limits, plausibility bounds for results, audit row per write, results ledger and annulment per identity; `GatewayCore` boundary; suspended identities refused in `open_session` | `src/gateway_steelbound/`, `src/core_api/gateway.rs`, `src/campaign_core/gateway.rs`, `src/campaign_core/identity.rs`, `sql/fork/gateway.sql`, `sql/fork/gateway_annul.sql`, `deploy/server/annul.sh`, `deploy/units/steelcore-annul.service`, `tools/gateway_client.py`, `tools/gateway-cert.sh`, `src/main.rs` (start the listener) | no |
| Shop catalog: the 31 VT types of the reinforcement shop (faction, cost, limit, turn of release) as a `once` migration with full stock and serials from 1; without it a new pilot cannot buy a VT and MISSION SELECT stays disabled. Upstream fills `shop` with a one-off import from its old database (`sql/shop_migration.sql`) | `sql/fork/shop_catalog.sql`, `sql/fork/migrations.list`, `src/campaign_core/shop_tests.rs` | maybe: a seed file for new installs (see below) |
| Optional short turns: `[campaign] turn_minutes` (at least 10) replaces the fixed one-week turn and scales the shop drops; at start the running turn is shortened to at most one turn from now. Without the key nothing changes. Meant for the test campaign | `src/campaign_core/turn.rs`, `src/service.rs` (turn length), `src/main.rs` (config field), `deploy/config/steelcore-test.toml.example` | maybe (configurable turn length) |
| Fix: buying an unlimited VT (`vtstock` NULL) set its stock to 0 (`GREATEST(NULL - n, 0)` is 0 in PostgreSQL), so the VT vanished from every shop until the next restock | `src/command.rs` (`vt_add`) | **yes** (see below) |
| Changing sides at any time: `SetUserFaction` answers status 0 (the game stays in the campaign, no sign-out), faction rankings count one statistic per player over all sides; `[campaign] transfer_in_place` / `rank_per_player` (default `true`, `false` = upstream) | `src/campaign_core/faction_swap.rs`, `src/campaign_core/faction_swap_tests.rs`, `src/command.rs` (`set_user_faction`, `rank_values`, `rank_count_internal`), `src/main.rs` (config field), `deploy/config/*.example` | maybe (as an option) |
| Core split: players, identities, display names, accesses, idempotency keys and the Core API audit move into schema `core` (takeover with row count and md5 check, way back, own connection pool with `search_path = core`); the campaign keeps schema `public` and refers to the core by value only | `sql/fork/core_split.sql`, `sql/fork/core_unsplit.sql`, `sql/fork/gateway_annul.sql` (schema-qualified), `src/campaign_core/mod.rs` (`connect_core`), `src/campaign_core/identity.rs`, `src/campaign_core/profile.rs`, `src/main.rs` (open the core pool), `tools/test_core_split.sh` | no |
| Season reset that keeps the core: `campaign_reset()` empties every campaign table, refills the shop, starts round 1 / turn 1 and checks that the core did not change; maintenance units with backup first; restore keeps the core unless asked; test-first deploy (`releases/test`, `deploy.sh --test`) | `sql/fork/season_reset.sql`, `deploy/server/maint.sh`, `deploy/server/restore.sh`, `deploy/server/activate.sh`, `deploy/deploy.sh`, `deploy/units/steelcore-maint.service`, `deploy/units/steelcore-test-*.service`, `src/campaign_core/reset_tests.rs`, `tools/test_activate.sh` | no |
| Trades in the chat room: the token is bound to the identity that asked for it and to the named partner pilot, expires, is used once per command and is never 0; exactly the issuer and the partner meet; VTs move only if both report the same swap and the giver owns every VT, both sides in one transaction (upstream paired the commands with a barrier only) | `src/command.rs`, `src/campaign_core/trade_tests.rs` | yes, after review (see "Trades") |

## Players, identities and the core boundary

Following the [steelcore concept](https://github.com/Greevee-Lab/steelcore/blob/main/docs/concept.md):

- **Player and identity** ([ADR-0002](https://github.com/Greevee-Lab/steelcore/blob/main/docs/adr/0002-player-id-from-existing-uuid.md)).
  A player owns pilot, garage, awards and leaderboard rows. An identity is one
  provider account, `(provider, external_id)`, unique. The `player_id` is the
  existing account UUID, so no campaign row changes:
  - `insignia`: the XUID as UUID, exactly as the handshake derives it
    (`Uuid::from_u64_pair(0, xuid)`, in SQL `lpad(to_hex(xuid), 32, '0')::uuid`).
    Prepared, disabled until a gateway uses it.
  - `steelbound`: the account id `n` becomes the pseudo-XUID `0x53424C01_nnnnnnnn`
    ("SBL", version 1), then the same derivation. Real XUIDs start with
    `0x0009` or `0xE000`, so the ranges never overlap.
- **Core boundary** ([ADR-0001](https://github.com/Greevee-Lab/steelcore/blob/main/docs/adr/0001-core-api-only.md)).
  `src/core_api/` defines the trait `CampaignCore` with every campaign
  operation, grouped like the original protocol, and a dispatcher from
  received commands to core calls. `src/campaign_core/` implements it: the
  campaign commands delegate to the unchanged upstream code; identity
  resolution (`resolve_identity`, which creates player and identity on first
  contact and writes an audit row) is new. In this version the boundary is an
  in-process Rust trait; a network API can expose the same calls later.
- **Rule:** new code outside `src/campaign_core/` never touches the database.
  The test `core_api::rules::new_code_has_no_sql` fails on database access in
  any source file that is neither upstream nor part of the core. When upstream
  adds a source file, add it to the list in `src/core_api/rules.rs`.

## Sign-in tickets

The campaign server does not believe the XUID a game reports, and it has no
access to the account database of an identity provider. A provider vouches
for a player with a **ticket** instead (steelcore concept, trust model):

- After its own sign-in the provider (steelnet, for Steelbound players)
  issues a short-lived ticket signed with **Ed25519**: issuer, provider,
  account id, pseudo-XUID, display name, campaign id, issue and expiry time
  (default lifetime 30 minutes), random ticket id, key id. No password,
  e-mail or address. The binary format is documented in `src/ticket/mod.rs`.
- `src/ticket/` checks structure, version, key, signature (strict RFC 8032
  verification: no malleable signatures, no weak keys), issuer, provider,
  campaign, identity and lifetime with a small clock tolerance. The result is
  `(provider, external_id)` for `CampaignCore::resolve_identity`.
- A ticket may be used several times while it is valid: every campaign
  transaction of the game is a separate short connection.
- **Configuration:** section `[ticket]` in `/etc/steelcore/steelcore.toml`
  with the issuer name, the campaign id of this instance and the issuer's
  public keys. Only public keys ever reach this server.
- **Key rotation:** the key id (first 8 bytes of the public key) is part of
  each ticket, and the verifier holds a list of keys. Add the new key, switch
  the issuer to it, remove the old key once the last ticket signed with it
  has expired. Removing a key stops its tickets at once (key loss).
- **Shared test vectors:** `testdata/campaign_ticket_vectors.toml` is
  generated by the issuer from test keys and kept byte-identical in both
  projects; `ticket::tests::shared_vectors` must accept and refuse exactly
  the listed cases.
- **Crate:** `ed25519-dalek` 2.x (pure Rust, part of the widely reviewed
  dalek-cryptography project, `verify_strict`, no OpenSSL or C toolchain
  needed on Linux or Windows).

## Core API (steelcore.v1)

Every provider – Steelbound's steelnet today, other clients later – reaches
players, identities and display names only through one versioned network API
(steelcore ADR-0001, ADR-0007). Nobody gets a database login or a private
shortcut.

- **Contract:** `proto/steelcore/v1/*.proto` is the single source of truth
  (`IdentityService`, `PlayerService`, `AdminService`, see `proto/README.md`).
  Campaign commands stay in-process for now (`CampaignCore`) and can be added
  later as further services in the same package. `build.rs` generates the
  code with `protox` (no `protoc` needed), `tonic`/`prost` and `pbjson`.
- **Two bindings, one listener:** gRPC (HTTP/2) and Connect-JSON
  (`POST /steelcore.v1.<Service>/<Method>`, `application/json`, proto3 JSON
  mapping). Both call the same functions in `src/core_api/net/service.rs`;
  a test runs the same call sequence through both and requires identical
  results. Clients that should stay dependency-free (Python standard
  library) use Connect-JSON.
- **Mutual TLS 1.3:** the server only accepts client certificates of its own
  CA (`tools/steelcore-ca.sh`), and the certificate's SHA-256 fingerprint
  must be listed as an access in `[core_api]`. An access has a provider (or
  none, for read-only or admin tools), scopes (`identity:write`,
  `profile:write`, `read:public`, `admin`) and a rate limit. The provider
  of an identity always comes from the access, never from the request.
- **Writes** carry an idempotency key (required for `SetDisplayName` and
  admin calls, optional for the naturally idempotent `ResolveIdentity`); a
  repeated key returns the first response, a reused key with another
  request is refused. Every write of a known access, refused or not, leaves
  one audit row (access, provider, method, identity/player, outcome) – never
  request contents.
- **Display names** belong to the player, not to a provider: unique across
  providers, classic gamertag rules (1–15 characters, letters, digits,
  single spaces, starting with a letter), look-alike check (case, I/l/1,
  O/0, rn/m, vv/w, spaces), blocklist table, one change per 7 days, the old
  name stays reserved for its owner for 30 days (both configurable).
  A provider may only rename players it has an identity for.
- **Operation:** `deploy/README.md`, section "Core API".

## Core and campaign

A season ends; players and their display names stay. The data of one
instance is therefore split into two parts (`sql/fork/core_split.sql`):

| Part | Schema | Tables | Emptied by a season reset |
|---|---|---|---|
| Core | `core` | `provider`, `player`, `identity`, `display_name`, `display_name_reservation`, `display_name_blocklist`, `api_access`, `api_idempotency`, `audit_log` (identity and Core API rows), `split_report` | never |
| Campaign | `public` | `accounts`, `settings`, `shop`, `garage`, `graveyard`, `leaderboard`, `wartide`, `awards`, `gateway_result`, `audit_log` (game writes, annulments, reset) | yes (`shop` is refilled) |

- **No foreign key between the parts.** The campaign refers to the core by
  value: `accounts.xuid` = `player_id`, `gateway_result.identity_id`. Either
  part can be emptied or restored on its own, and the core could later move
  to a database of its own without a schema change.
- **Two pools.** `SbCampaignCore` holds the campaign pool (the server's
  global pool) and a core pool on the same database with `search_path = core`
  (`campaign_core::connect_core`, at most 4 connections). Core code
  (`identity.rs`, `profile.rs`) uses the core pool, so an unqualified table
  name there can only be a core table, and the campaign pool cannot see core
  tables by accident. `resolve_identity` now only touches the core; the
  account row (the player's campaign record) is created by `open_session`
  when the player plays, e.g. for the first time after a reset.
- **Why a schema and not a database of its own:** the service role may not
  create databases or roles, and both instances run as the same operating
  system user with peer authentication, so a second role would not separate
  anything. In one database the takeover runs in one transaction (copy,
  compare, drop) and a backup is one consistent snapshot of both parts. A
  season reset no longer needs a database restore.
- **One core per instance.** The test instance has its own core in its own
  database: test players and names can never change production names.
- **Takeover** (`once`, idempotent): creates the core tables, copies each
  table, compares row count and an md5 over all rows with the source (the
  fingerprint formula of `collect-status.sh`, so the values can be checked
  against the fingerprint file taken before the deploy), records them in
  `core.split_report` (`deploy.sh --report <db>` prints it), and only then
  drops the foreign keys of campaign tables to the old tables, deletes the
  moved audit rows and drops the old tables. Any difference rolls the whole
  transaction back.
- **Way back:** `sql/fork/core_unsplit.sql` (not in the migration list; run
  through `maint.sh unsplit` right before rolling back to a release from
  before the split) puts everything back as before, compared the same way;
  core audit rows get new ids. Or restore the backup taken before the
  deploy with `RESTORE_CORE` (see `deploy/README.md`).
- **Season reset:** `campaign_reset(apply, turn1_end, source)` in
  `sql/fork/season_reset.sql` empties every table in `public` except
  `fork_migrations`, refills the shop (stock = limit, serials from 1), starts
  round 1 / turn 1 and writes one `campaign_reset` audit row; it fingerprints
  every core table before and after and aborts on any difference, and it
  refuses to run on a database that is not split. A campaign ban
  (`accounts.banned`) goes with the reset; a ban that must outlast a season
  is an identity suspension (core). Operation: `deploy/README.md`, "Season
  reset".
- **Annulment** stays one SQL function, now schema-qualified: the identity in
  `core`, ledger, leaderboard and war tide in `public`. It works on the open
  ledger rows it locked at the start; a suspension also writes a core audit
  row (`identity_suspended`).
- **Tests:** `db_core_survives_season_reset` (own database
  `SBCAMPAIGN_TEST_RESET_DATABASE_URL`: names, 7-day limit, 30-day
  reservation and identities unchanged after a reset, a new account on the
  next session), `tools/test_core_split.sh` (takeover with synthetic season
  data, second run, annulment, reset through `maint.sh` with backup, restore
  of the campaign only, refusal of a pre-split dump, way back, split again,
  way back through a pre-split backup), `tools/test_activate.sh` (test-first
  deploy). All other tests run unchanged against the split schema.

## Steelbound gateway

The upstream listener trusts the XUID the game reports in `0x2020`; it is
meant for a trusted service in front of it and stays on loopback here. Public
access for Steelbound players goes through a second listener,
`src/gateway_steelbound/`:

1. **TLS only** (rustls; TLS 1.2 with ECDHE + AEAD, or 1.3 only with
   `tls_min = "1.3"`). Players have no client certificates. The client pins
   the server key: SHA-256 of the certificate's DER `SubjectPublicKeyInfo`,
   logged at startup (`tools/gateway-cert.sh pin <cert>` prints it too). The
   certificate may be self-signed; renewing it with the same key keeps the pin.
2. **Prelude** before the original protocol, big-endian:
   `"SBGW"`, version `1`, flags `0`, `u16` ticket length, ticket (format v1 of
   `src/ticket/`). The server answers 6 bytes `"SBGW"`, `1`, status:
   `0` ok (the original protocol follows), `1` refused, `2` busy (rate
   limit), `3` unsupported. Every ticket problem (missing, malformed,
   expired, forged, wrong campaign) is the same `1`: no oracle. The reason is
   logged on the server only.
3. The gateway resolves `(steelbound, <account id>)` through
   `CampaignCore::resolve_identity` and `open_session` (banned players and
   **suspended identities** are rejected with `0x3014`), then serves the
   **unchanged original protocol** through `core_api::dispatch`. The
   `qwUserID` of `0x2020` is read and ignored. When a pilot is registered
   (`0x5022`), the stored Xbox Live name is the display name from the ticket,
   not the one the game sends.
4. **No database code** in the gateway: everything goes through
   `CampaignCore` and the new `GatewayCore` (`src/core_api/gateway.rs`:
   audit, results ledger, annulment); `core_api::rules` enforces it.

**Protection** (`[gateway_steelbound.limits]`, defaults in `deploy/README.md`):

- Before TLS: total connections, open connections per source, new
  connections per source (token bucket), and a lockout after too many
  refused tickets. A source is an IPv4 address or an IPv6 /64. Addresses are
  kept in memory only, never logged or stored.
- Per identity: accepted connections, commands, reported results (token
  buckets); commands per connection; timeouts for the TLS handshake, the
  prelude and the whole connection.
- **Plausibility** of reported results: `AddUserPoint` and `AddMissionPoint`
  must name the current round and the current or previous turn, a map up to
  `max_map`, at most `max_user_stat` per stat and `max_mission_points` per
  report, mission points only for HSD/PRF/RB; VT lists at most `max_vt_list`
  entries. An implausible result is answered like a success (the game cannot
  tell) but not applied; other implausible commands are rejected.
- **Audit:** one row in `audit_log` per write command (source
  `gateway-steelbound`, identity, player, command, outcome `ok`, `error`,
  `implausible` or `rate_limited`, time). No pilot data, names or points.
- **Results ledger** `gateway_result`: every applied result with its
  identity, because leaderboard and war tide only hold sums.
- **Annulment** `annul_identity_results()` (`sql/fork/gateway_annul.sql`):
  subtracts an identity's ledger rows from leaderboard and war tide (never
  below zero), marks them annulled, optionally suspends the identity, writes
  an audit row; dry run possible. Awards already handed out at a turn or
  round end are not recomputed. Operation: `deploy/README.md`, "Annul results".

**Hooks in upstream files:** `src/main.rs` starts the listener (one module
line, starting and awaiting the listener in `main`). Nothing else upstream
changes.

**Tests:** prelude, tickets (valid, expired, forged, tampered, missing),
admission and rate limits, plausibility, configuration (the gateway refuses
to start unless `[network] bind_ip` is loopback), TLS end to end with pin,
and with a database: pilot register/load/save and faction through the
gateway with a foreign `qwUserID`, audit rows, results ledger, annulment and
the suspended identity being refused. `tools/gateway_client.py` runs the same
cases against a running server (CI does this against the release binary).

## Shop catalog

A fresh database has an empty `shop` table, because upstream imports the
catalog once from its old MySQL database (`sql/shop_migration.sql`, not in
the migration list). The game then receives an empty ShopList (0x2041): a
new pilot owns no VT and cannot buy one, so MISSION SELECT stays disabled.
With an empty garage *and* an empty shop the game also loops forever when
the player moves the cursor in the Reinforcements screen (it searches the
next set bit of an empty selection mask).

`sql/fork/shop_catalog.sql` inserts the same 31 rows (`ON CONFLICT DO
NOTHING`) with `vtstock = vtlimit` and serial numbers from 1. A new pilot
starts with VT points from the pilot data and buys the first VTs in the
shop; in turn 1 HSD gets Vitzh and Vortex, PRF Decider (unlimited) and
Falchion (limited). Guerrilla (RB) and Jaralaccs VTs are released from turn 3
and 5, so pilots of those factions have nothing to buy before that.

Wire format checked by `db_new_pilot_can_buy_a_starting_vt`: `u8 status,
u16 count`, then per entry `u16 type, u32 limit, u16 cost, u8 pad`
(limit `0xFFFFFFFF` = unlimited; the game hides entries with limit 0).

Note for upstream: a new installation needs this catalog as a seed; a
small `sql/shop_seed.sql` would let others run the server from scratch.

## Turn length

Upstream ends every turn one week after the previous one. `[campaign]
turn_minutes = N` (optional, at least 10) uses N minutes instead; the shop
drops are spread over that length. When the service starts with a short
turn length and the running turn ends later than one turn from now, the
running turn is shortened (a seeded test database would otherwise wait a
week). A round still has 8 turns; factions are reset at turns 3 and 5 and
at the start of a round (upstream rules). The production season does not
set the key. Use at least 120 minutes: the game does not apply combat results
to the force map in the last hour of a turn (its news text says so), so with
one-hour turns no result would count.

## Changing sides at any time

With few players a campaign evening is only balanced when pilots can move to
the other side whenever they like. The game transfers a pilot in the
Enlistment Office (`SetUserFaction`, `0x5025`), but it opens that office for a
pilot with a side only in turns 3 to 6. The Steelbound client keeps the office
open in every turn and, after a transfer, opens the campaign menu instead of
the main menu; that menu loads the pilot and the garage of the new side from
the server again (`VtOwned` + `GetUserInfo`, the same way as after signing in).
Two small patches, switch `loc_camp_faction_swap`, on by default. There is no
cooldown and no limit. Module `src/campaign_core/faction_swap.rs`, two
optional `[campaign]` keys:

| Key | Default | Meaning |
|---|---|---|
| `transfer_in_place` | `true` | The answer to `SetUserFaction` is status 0, the game's own success path: it stores the new side in the pilot, saves the pilot (`0x5024`), shows "Pilot transferred to the ..." and leaves the Enlistment Office (Steelbound client: to the campaign menu, which reloads pilot and garage). `false` = upstream: status 1, which the game treats as an error and signs the player out (the garage then reloads at the next sign-in). An unpatched game returns to the main menu with the old garage still in its pilot record until a garage screen asks for `VtOwned` again. |
| `rank_per_player` | `true` | One statistic per player over all sides. A faction ranking (`RankValues`, `RankCountFaction` with a faction) lists the pilots who are in that faction now, each with the sum of all their leaderboard rows. The global ranking and the awards already sum per player. `false` = upstream: a faction ranking counts only the points scored for that faction. |

What stays per side: the garage (`garage.faction`; every side keeps its own
VTs, a transfer moves nothing) and the shop. A battle counts for the side it
was fought for: the leaderboard row keeps the faction of the result
(`AddUserPoint` uses the faction at the time of the result) and the war tide
uses the faction the game reports (`AddMissionPoint`). The pilot record
(name, rank, combat points, decorations) is one per player anyway.

How the game answers `0x6025` (read from the Line of Contact XBE, function
`0x112C70` and `0x11FF70`): status 0 = success, 2/3/4 = "could not be
transferred" messages (limit of transfers reached, transfer period ended,
force not recruiting), 5 = another message, anything else = campaign error
with sign-out. Upstream answers 1 on purpose ("to kick the user out of
Campaign so that their garage reloads correctly"). The tests in
`src/campaign_core/faction_swap_tests.rs` check the garages per side over
two transfers, the status, one statistic per player and the points per side.

## Note for upstream: unlimited VT stock

`vt_add` updated the stock with `vtstock = GREATEST(vtstock - $3, 0)`. For an
unlimited VT (`vtstock IS NULL`) PostgreSQL's `GREATEST` ignores the NULL
and returns 0, so after the first purchase the VT disappears from every
shop (`COALESCE(vtstock, 1) > 0`) until the turn restock. MySQL returns
NULL there, which is probably why it worked before the port. The fix is
`vtstock = vtstock - LEAST(vtstock, $3)` (NULL stays NULL, otherwise the
same as before). Candidate for a small pull request.

## Trades

The game trades in a chat room of the Neutral Zone. The two players agree over
their peer-to-peer link. Then the side with the lower player index sends
TradeToken (`0x104A`) with the partner's **pilot name** and passes the token
to the partner over the same link (a token of 0 means "no token yet" to the
game). Both sides send TradeUserInfo (`0x104B`: partner name, token, their
pilot data after the trade) and TradeVT (`0x1045`: token, partner name, VTs
received, VTs given).

Upstream paired the two commands of a trade with a barrier only. Any client
could claim any VT by type and serial (the partner did not have to own or give
it), a client could pair a trade with itself over two connections, and the
first token was 0. The fork keeps the protocol and adds the checks:

- A token is bound to the identity that asked for it and to the named partner
  pilot. The pilot name comes from the stored pilot data (offset 0x28, compared
  without case and trailing padding), not from `xname`, which holds the
  gamertag. Tokens expire after 180 s, are used once per command, are never 0;
  one open token per identity, at most 4096 at once.
- Exactly the issuer and the named partner meet: two different identities with
  a faction that name each other. The first side waits up to 10 s.
- TradeVT: both sides must report the same swap (what one gives, the other
  receives), no VT twice, at most 16 per side, and the giver must own every VT
  it gives (not sortied). The second side applies both directions in one
  transaction; any failure rolls everything back and both sides get an error
  ("Trade failed." in the game).
- TradeUserInfo: paired the same way; both pilot records are written in one
  transaction.

Tests: `command::trade_tests` (token, names, pairing, swap check, rendezvous)
and `campaign_core::trade_tests::db_trade_swaps_owned_vts_and_refuses_theft`
(a real swap, a refused theft with an accomplice, a refused one-sided claim,
a token used twice, a trade with oneself). Verified with two game clients
against a local instance: a Vitzh traded for a Decider ("Trade successful."),
both VTs moved with their faction, two audit rows each for TradeUserInfo and
TradeVT.

