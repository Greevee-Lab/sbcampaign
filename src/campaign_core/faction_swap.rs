// SPDX-License-Identifier: AGPL-3.0-only
//! Changing sides at any time.
//!
//! With few players a campaign evening is only balanced when pilots can move
//! to the other side whenever they like. The game itself allows a transfer in
//! the Enlistment Office (SetUserFaction, `0x5025`); the Steelbound client keeps
//! that office open in every turn. Two optional `[campaign]` keys decide what
//! the server does around it:
//!
//! - `transfer_in_place` (default `true`): the answer to SetUserFaction is
//!   status 0, the game's own success path. The game then stores the new side
//!   in the pilot, saves the pilot (`0x5024`) and returns to the campaign main
//!   menu. Every garage screen asks for `VtOwned` again, so the garage of the
//!   new side shows up without leaving the campaign. Upstream always answers
//!   status 1, which the game treats as an error: it signs the player out, and
//!   the garage reloads at the next sign-in. `false` restores that.
//! - `rank_per_player` (default `true`): one statistic per player over all
//!   sides. A faction ranking lists the pilots who are in that faction now,
//!   each with the sum of all their leaderboard rows. Upstream counts only the
//!   points a pilot scored for that faction. `false` restores that.
//!
//! Garages stay per side (`garage.faction`) and a battle still counts for the
//! side it was fought for: the leaderboard row keeps the faction of the
//! result, and the war tide uses the faction the game reports.

use crate::client::Faction;
use sqlx::{Postgres, QueryBuilder};

/// The two `[campaign]` keys of this module.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FactionSwap {
    pub transfer_in_place: bool,
    pub rank_per_player: bool,
}

impl Default for FactionSwap {
    fn default() -> Self {
        Self { transfer_in_place: true, rank_per_player: true }
    }
}

fn flag(campaign: &toml::Table, name: &str, default: bool) -> bool {
    match campaign.get(name) {
        None => default,
        Some(v) => v
            .as_bool()
            .unwrap_or_else(|| panic!("campaign.{name} must be true or false")),
    }
}

/// Parse the keys from the `[campaign]` table; missing keys use the defaults.
pub fn parse(campaign: &toml::Table) -> FactionSwap {
    let d = FactionSwap::default();
    FactionSwap {
        transfer_in_place: flag(campaign, "transfer_in_place", d.transfer_in_place),
        rank_per_player: flag(campaign, "rank_per_player", d.rank_per_player),
    }
}

/// Status of the SetUserFaction answer: 0 = success (the game stays in the
/// campaign), 1 = upstream (the game signs the player out).
pub fn transfer_status(transfer_in_place: bool) -> u8 {
    if transfer_in_place { 0 } else { 1 }
}

/// Faction filter of a leaderboard query (`... FROM leaderboard WHERE ...`).
/// Faction::None means all players and adds nothing.
pub fn push_faction_filter(query: &mut QueryBuilder<Postgres>, faction: Faction, rank_per_player: bool) {
    if matches!(faction, Faction::None) {
        return;
    }
    if rank_per_player {
        query.push(" AND xuid IN (SELECT xuid FROM accounts WHERE faction = ");
        query.push_bind(faction);
        query.push(")");
    } else {
        query.push(" AND faction = ");
        query.push_bind(faction);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_on() {
        assert_eq!(parse(&toml::Table::new()), FactionSwap { transfer_in_place: true, rank_per_player: true });
        let t: toml::Table = "transfer_in_place = false\nrank_per_player = false".parse().unwrap();
        assert_eq!(parse(&t), FactionSwap { transfer_in_place: false, rank_per_player: false });
    }

    #[test]
    #[should_panic(expected = "true or false")]
    fn invalid_values_are_refused() {
        let t: toml::Table = "rank_per_player = 1".parse().unwrap();
        parse(&t);
    }

    #[test]
    fn transfer_status_matches_the_game() {
        // The game (FUN_0011FF70) maps status 0 to success and any other
        // unknown status to its campaign error handler (sign-out).
        assert_eq!(transfer_status(true), 0);
        assert_eq!(transfer_status(false), 1);
    }

    #[test]
    fn faction_filter_sql() {
        let sql = |faction, per_player| {
            let mut q: QueryBuilder<Postgres> = QueryBuilder::new("SELECT 1 FROM leaderboard WHERE round = 1");
            push_faction_filter(&mut q, faction, per_player);
            q.sql().to_owned()
        };
        assert_eq!(sql(Faction::None, true), "SELECT 1 FROM leaderboard WHERE round = 1");
        assert_eq!(sql(Faction::None, false), "SELECT 1 FROM leaderboard WHERE round = 1");
        assert_eq!(sql(Faction::Prf, false), "SELECT 1 FROM leaderboard WHERE round = 1 AND faction = $1");
        assert_eq!(
            sql(Faction::Prf, true),
            "SELECT 1 FROM leaderboard WHERE round = 1 AND xuid IN (SELECT xuid FROM accounts WHERE faction = $1)"
        );
    }
}
