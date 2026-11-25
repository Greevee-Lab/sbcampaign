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

## Planned additions

A gateway for Steelbound players (signed sign-in ticket, TLS listener next to
the upstream listener) built on the core boundary.
