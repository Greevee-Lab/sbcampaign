// SPDX-License-Identifier: AGPL-3.0-only
//! The campaign core: the upstream campaign logic behind [`CampaignCore`].
//!
//! [`SbCampaignCore`] is an adapter. Every campaign command delegates to the
//! unchanged upstream implementation in [`crate::command`]; only the identity
//! operations are new ([`identity`]). This is the one fork module that may
//! access the database.

#![allow(dead_code)] // Called by the Steelbound gateway, which is the next step.

pub mod gateway;
pub mod identity;
pub mod profile;
pub mod turn;

#[cfg(test)]
mod tests;
#[cfg(test)]
mod profile_tests;
#[cfg(test)]
mod gateway_tests;
#[cfg(test)]
mod shop_tests;
#[cfg(test)]
mod trade_tests;
#[cfg(test)]
mod reset_tests;

use crate::{
    client::{Client, ClientError, Faction, Language},
    command::{self, RankCategory, VTEntryList, VTIDList},
    core_api::{
        CampaignCore, CommandResult, IdentityError, PilotData, Provider, ResolvedIdentity, VerifiedBy,
    },
};
use sqlx::PgPool;

/// Schema of the core tables (players, identities, display names, provider
/// accesses, Core API audit), see `sql/fork/core_split.sql`. The campaign
/// tables stay in `public`.
pub const CORE_SCHEMA: &str = "core";

/// Most connections the core pool opens (the core sees a few calls a minute).
const CORE_POOL_MAX: u32 = 4;

/// The campaign core of this server.
///
/// Two pools on the instance database:
/// - `pool`: the campaign (schema `public`). The upstream commands use the
///   server's global pool (`crate::get_pool()`), which is the same pool in the
///   running server.
/// - `core`: the core (schema [`CORE_SCHEMA`] only, see [`connect_core`]). An
///   unqualified table name on this pool can only mean a core table, and the
///   campaign pool cannot see core tables by accident.
///
/// The two parts share no foreign key and no transaction: the campaign refers
/// to the core by value (player_id = account UUID, identity_id).
#[derive(Clone)]
pub struct SbCampaignCore {
    pool: PgPool,
    core: PgPool,
}

impl SbCampaignCore {
    pub fn new(pool: PgPool, core: PgPool) -> Self {
        Self { pool, core }
    }

    /// The campaign pool (schema `public`).
    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// The core pool (schema [`CORE_SCHEMA`]).
    pub fn core_pool(&self) -> &PgPool {
        &self.core
    }
}

/// Opens the core pool: the same server, database and login as `campaign`,
/// with `search_path` set to the core schema only.
pub async fn connect_core(campaign: &PgPool) -> Result<PgPool, sqlx::Error> {
    let options = (*campaign.connect_options())
        .clone()
        .options([("search_path", CORE_SCHEMA)]);
    sqlx::postgres::PgPoolOptions::new()
        .max_connections(CORE_POOL_MAX)
        .connect_with(options)
        .await
}

impl CampaignCore for SbCampaignCore {
    async fn resolve_identity(
        &self,
        provider: Provider,
        external_id: &str,
        verified_by: VerifiedBy,
        source: &str,
    ) -> Result<ResolvedIdentity, IdentityError> {
        identity::resolve_identity(&self.core, provider, external_id, verified_by, source).await
    }

    async fn open_session(&self, client: &mut Client, resolved: &ResolvedIdentity) -> Result<bool, ClientError> {
        identity::open_session(&self.pool, &self.core, client, resolved).await
    }

    // War
    async fn turn_days(&self, client: &mut Client) -> CommandResult {
        command::turn_days(client).await
    }
    async fn campaign_period(&self, client: &mut Client) -> CommandResult {
        command::campaign_period(client).await
    }
    async fn war_tide(&self, client: &mut Client, round: u16, turn: u16, map: u8) -> CommandResult {
        command::war_tide(client, round, turn, map).await
    }
    async fn war_influence(&self, client: &mut Client, round: u16, turn: u16) -> CommandResult {
        command::war_influence(client, round, turn).await
    }
    async fn round_points(&self, client: &mut Client, round: u16) -> CommandResult {
        command::round_points(client, round).await
    }

    // Missions
    async fn analyze(&self, client: &mut Client, data: [u32; 10]) -> CommandResult {
        command::analyze(client, data).await
    }
    async fn report_user_points(
        &self,
        client: &mut Client,
        round: u16,
        turn: u16,
        map: u8,
        points: [u32; 10],
    ) -> CommandResult {
        command::add_user_point(client, round, turn, map, points).await
    }
    async fn report_mission_points(
        &self,
        client: &mut Client,
        round: u16,
        turn: u16,
        map: u8,
        points: u32,
        faction: Faction,
    ) -> CommandResult {
        command::add_mission_point(client, round, turn, map, points, faction).await
    }

    // Messages
    async fn free_message(&self, client: &mut Client, language: Language) -> CommandResult {
        command::free_message(client, language).await
    }
    async fn fixed_message(&self, client: &mut Client) -> CommandResult {
        command::fixed_message(client).await
    }
    async fn message_count(&self, client: &mut Client) -> CommandResult {
        command::message_count(client).await
    }
    async fn message_post(&self, client: &mut Client) -> CommandResult {
        command::message_post(client).await
    }

    // Pilot
    async fn get_pilot(&self, client: &mut Client) -> CommandResult {
        command::get_user_info(client).await
    }
    async fn create_pilot(
        &self,
        client: &mut Client,
        pilot_name: String,
        xlive_name: String,
        faction: Faction,
        pilot_data: PilotData,
    ) -> CommandResult {
        command::new_user_info(client, pilot_name, xlive_name, faction, pilot_data).await
    }
    async fn update_pilot(&self, client: &mut Client, pilot_data: PilotData) -> CommandResult {
        command::set_user_info(client, pilot_data).await
    }
    async fn delete_pilot(&self, client: &mut Client) -> CommandResult {
        command::del_user_info(client).await
    }
    async fn set_faction(&self, client: &mut Client, faction: Faction) -> CommandResult {
        command::set_user_faction(client, faction).await
    }

    // Garage
    async fn list_vts(&self, client: &mut Client) -> CommandResult {
        command::vt_owned(client).await
    }
    async fn add_vts(&self, client: &mut Client, vt_list: VTIDList) -> CommandResult {
        command::vt_add(client, vt_list).await
    }
    async fn remove_vts(&self, client: &mut Client, vt_list: VTEntryList) -> CommandResult {
        command::vt_del(client, vt_list).await
    }
    async fn capture_vts(&self, client: &mut Client, pilot_name: String, vt_list: VTEntryList) -> CommandResult {
        command::vt_capture(client, pilot_name, vt_list).await
    }
    async fn sortie_vts(&self, client: &mut Client, vt_list: VTEntryList) -> CommandResult {
        command::vt_sortie(client, vt_list).await
    }
    async fn return_vts(&self, client: &mut Client, vt_list: VTEntryList) -> CommandResult {
        command::vt_return(client, vt_list).await
    }

    // Shop
    async fn list_shop(&self, client: &mut Client) -> CommandResult {
        command::shop_list(client).await
    }
    async fn vt_potential_cost(&self, client: &mut Client) -> CommandResult {
        command::vt_potential_cost(client).await
    }

    // Leaderboard
    async fn rank_values(
        &self,
        client: &mut Client,
        round: u16,
        turn: u16,
        faction: Faction,
        category: RankCategory,
        start: u32,
        count: u16,
    ) -> CommandResult {
        command::rank_values(client, round, turn, faction, category, start, count).await
    }
    async fn rank_count_global(
        &self,
        client: &mut Client,
        round: u16,
        turn: u16,
        category: RankCategory,
    ) -> CommandResult {
        command::rank_count_global(client, round, turn, category).await
    }
    async fn rank_count_faction(
        &self,
        client: &mut Client,
        round: u16,
        turn: u16,
        category: RankCategory,
        faction: Faction,
    ) -> CommandResult {
        command::rank_count_faction(client, round, turn, category, faction).await
    }
    async fn rank_count_categories(&self, client: &mut Client, round: u16, turn: u16) -> CommandResult {
        command::rank_count_categories(client, round, turn).await
    }

    // Awards
    async fn award_available(&self, client: &mut Client) -> CommandResult {
        command::award_available(client).await
    }
    async fn award_points(&self, client: &mut Client, is_turn: bool) -> CommandResult {
        command::award_points(client, is_turn).await
    }
    async fn award_ranks(&self, client: &mut Client, round: u16, turn: u16, faction: Faction) -> CommandResult {
        command::award_ranks(client, round, turn, faction).await
    }
    async fn claim_award(
        &self,
        client: &mut Client,
        round: u16,
        turn: u16,
        vt_list: VTIDList,
        pilot_data: PilotData,
    ) -> CommandResult {
        command::award_claim(client, round, turn, vt_list, pilot_data).await
    }

    // Trading
    async fn create_trade_token(&self, client: &mut Client, pilot_name: String) -> CommandResult {
        command::trade_token(client, pilot_name).await
    }
    async fn trade_pilot(
        &self,
        client: &mut Client,
        pilot_name: String,
        token: u32,
        pilot_data: PilotData,
    ) -> CommandResult {
        command::trade_user_info(client, pilot_name, token, pilot_data).await
    }
    async fn trade_vts(
        &self,
        client: &mut Client,
        token: u32,
        pilot_name: String,
        rx_vt_list: VTEntryList,
        tx_vt_list: VTEntryList,
    ) -> CommandResult {
        command::trade_vt(client, token, pilot_name, rx_vt_list, tx_vt_list).await
    }
}
