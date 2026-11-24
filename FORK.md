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

## Planned additions

Following the steelcore concept, new modules will be added here for player
identity, the core boundary (gateways never touch the database directly) and
a gateway for Steelbound players. They are developed as separate modules on
top of the upstream campaign logic.
