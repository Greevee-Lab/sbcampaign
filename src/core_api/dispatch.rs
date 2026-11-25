// SPDX-License-Identifier: AGPL-3.0-only
//! Maps a received original-protocol command to the matching core call.
//!
//! Same mapping as the upstream client loop, but through [`CampaignCore`],
//! so a gateway can serve the unchanged protocol without touching the
//! database.

use super::{CampaignCore, CommandResult};
use crate::client::{Client, ClientError};
use crate::command::rx::RxCommand;

/// Answer one command of a session that has passed the handshake.
///
/// Handshake commands (`Pong`, `XUID`) are not campaign operations and are
/// answered with `BadResponse`, like the upstream loop does.
pub async fn dispatch<C: CampaignCore>(core: &C, client: &mut Client, rx: RxCommand) -> CommandResult {
    match rx {
        // War
        RxCommand::TurnDays => core.turn_days(client).await,
        RxCommand::CampaignPeriod => core.campaign_period(client).await,
        RxCommand::WarTide { round, turn, map } => core.war_tide(client, round, turn, map).await,
        RxCommand::WarInfluence { round, turn } => core.war_influence(client, round, turn).await,
        RxCommand::RoundPoints { round } => core.round_points(client, round).await,

        // Missions
        RxCommand::Analyze { data } => core.analyze(client, data).await,
        RxCommand::AddUserPoint { round, turn, map, points } => {
            core.report_user_points(client, round, turn, map, points).await
        }
        RxCommand::AddMissionPoint { round, turn, map, points, faction } => {
            core.report_mission_points(client, round, turn, map, points, faction).await
        }

        // Message of the day
        RxCommand::FreeMessage { language } => core.free_message(client, language).await,
        RxCommand::FixedMessage => core.fixed_message(client).await,

        // Pilot
        RxCommand::GetUserInfo => core.get_pilot(client).await,
        RxCommand::NewUserInfo { pilot_name, xlive_name, faction, pilot_data } => {
            core.create_pilot(client, pilot_name, xlive_name, faction, pilot_data).await
        }
        RxCommand::SetUserInfo { pilot_data } => core.update_pilot(client, pilot_data).await,
        RxCommand::DelUserInfo => core.delete_pilot(client).await,
        RxCommand::SetUserFaction { faction } => core.set_faction(client, faction).await,

        // Garage
        RxCommand::VtOwned => core.list_vts(client).await,
        RxCommand::VtAdd { vt_list } => core.add_vts(client, vt_list).await,
        RxCommand::VtDel { vt_list } => core.remove_vts(client, vt_list).await,
        RxCommand::VtCapture { pilot_name, vt_list } => core.capture_vts(client, pilot_name, vt_list).await,
        RxCommand::VtSortie { vt_list } => core.sortie_vts(client, vt_list).await,
        RxCommand::VtReturn { vt_list } => core.return_vts(client, vt_list).await,

        // Shop
        RxCommand::ShopList => core.list_shop(client).await,
        RxCommand::VtPotentialCost => core.vt_potential_cost(client).await,

        // Message board
        RxCommand::MessageCount => core.message_count(client).await,
        RxCommand::MessagePost => core.message_post(client).await,

        // Leaderboard
        RxCommand::RankValues { round, turn, faction, category, start, count } => {
            core.rank_values(client, round, turn, faction, category, start, count).await
        }
        RxCommand::RankCountGlobal { round, turn, category } => {
            core.rank_count_global(client, round, turn, category).await
        }
        RxCommand::RankCountFaction { round, turn, category, faction } => {
            core.rank_count_faction(client, round, turn, category, faction).await
        }
        RxCommand::RankCountCategories { round, turn } => core.rank_count_categories(client, round, turn).await,

        // Awards
        RxCommand::AwardAvailable => core.award_available(client).await,
        RxCommand::AwardPoints { is_turn } => core.award_points(client, is_turn).await,
        RxCommand::AwardRanks { round, turn, faction } => core.award_ranks(client, round, turn, faction).await,
        RxCommand::AwardClaim { round, turn, vt_list, pilot_data } => {
            core.claim_award(client, round, turn, vt_list, pilot_data).await
        }

        // Trading
        RxCommand::TradeToken { pilot_name } => core.create_trade_token(client, pilot_name).await,
        RxCommand::TradeUserInfo { pilot_name, token, pilot_data } => {
            core.trade_pilot(client, pilot_name, token, pilot_data).await
        }
        RxCommand::TradeVT { token, pilot_name, rx_vt_list, tx_vt_list } => {
            core.trade_vts(client, token, pilot_name, rx_vt_list, tx_vt_list).await
        }

        RxCommand::Pong | RxCommand::XUID(_) => Err(ClientError::BadResponse),
    }
}
