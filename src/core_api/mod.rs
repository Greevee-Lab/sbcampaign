// SPDX-License-Identifier: AGPL-3.0-only
//! The core boundary (steelcore ADR-0001).
//!
//! Protocol code (the upstream listener today, gateways such as the Steelbound
//! gateway next) talks to the campaign only through [`CampaignCore`]. The
//! campaign logic and all database access live behind it, in
//! [`crate::campaign_core`].
//!
//! **Rule:** code outside `src/campaign_core/` and the upstream files never
//! touches the database: no `sqlx`, no `get_pool()`, no SQL text. A unit test
//! (`rules::new_code_has_no_sql`) enforces this for every new source file.
//!
//! In this version the boundary is an in-process Rust trait. Its methods follow
//! the command groups of the original protocol one to one, so a later network
//! API (`steelcore.v1`) can expose the same calls without changing either side.
//!
//! Session state: the methods take the connected client (`&mut Client`) as the
//! session handle. The core reads `xuid` (the player_id), `faction` and
//! `language` from it and updates `faction` after pilot and faction changes.
//! It never reads from or writes to the client's connection. Over a network
//! API this handle becomes a session token carrying the same fields.

#![allow(dead_code)] // Called by the Steelbound gateway, which is the next step.

pub mod dispatch;
pub mod transport;

#[cfg(test)]
mod rules;
#[cfg(test)]
mod tests;

use crate::{
    client::{Client, ClientError, Faction, Language},
    command::{PILOT_DATA_LENGTH, RankCategory, VTEntryList, VTIDList, tx::TxCommand},
};
use std::{fmt, future::Future};
use thiserror::Error;
use uuid::Uuid;

/// Result of one campaign command: the original response command.
pub type CommandResult = Result<TxCommand, ClientError>;

/// The opaque pilot blob of the game.
pub type PilotData = [u8; PILOT_DATA_LENGTH as usize];

/// High 32 bits of a Steelbound pseudo-XUID: "SBL" + version 1.
///
/// A Steelbound account `n` (1 ..= 2^32-1) is shown to the game as the
/// pseudo-XUID `0x53424C01_nnnnnnnn`. Real XUIDs start with `0x0009` (online
/// accounts) or `0xE000` (offline), so the ranges never overlap.
pub const STEELBOUND_XUID_PREFIX: u32 = 0x5342_4C01;

/// An identity provider: who vouches for an external account id.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Provider {
    /// Steelbound accounts. External id: the account id in decimal, 1 ..= 2^32-1.
    Steelbound,
    /// Xbox Live accounts as seen by the Insignia service. External id: the
    /// XUID as 16 lowercase hex digits. Prepared; no gateway uses it yet.
    Insignia,
}

impl Provider {
    pub const ALL: [Provider; 2] = [Provider::Steelbound, Provider::Insignia];

    /// The `provider.provider_id` value.
    pub fn id(self) -> &'static str {
        match self {
            Provider::Steelbound => "steelbound",
            Provider::Insignia => "insignia",
        }
    }

    pub fn from_id(id: &str) -> Option<Provider> {
        Provider::ALL.into_iter().find(|p| p.id() == id)
    }

    /// Validate an external id and return its canonical text form, the one
    /// stored in `identity.external_id`.
    pub fn canonical_external_id(self, external_id: &str) -> Result<String, IdentityError> {
        let invalid = || IdentityError::InvalidExternalId {
            provider: self,
            external_id: external_id.chars().take(32).collect(),
        };
        let s = external_id.trim();
        match self {
            Provider::Steelbound => {
                if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) || s.starts_with('0') {
                    return Err(invalid());
                }
                let account: u32 = s.parse().map_err(|_| invalid())?;
                Ok(account.to_string())
            }
            Provider::Insignia => {
                let hex = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")).unwrap_or(s);
                if hex.is_empty() || hex.len() > 16 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
                    return Err(invalid());
                }
                let xuid = u64::from_str_radix(hex, 16).map_err(|_| invalid())?;
                // Zero is "no account"; the pseudo-XUID range belongs to Steelbound.
                if xuid == 0 || (xuid >> 32) as u32 == STEELBOUND_XUID_PREFIX {
                    return Err(invalid());
                }
                Ok(format!("{xuid:016x}"))
            }
        }
    }
}

impl fmt::Display for Provider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.id())
    }
}

/// How an identity was confirmed (`identity.verified_by`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerifiedBy {
    /// The provider's client reported it (today's XUID handshake).
    FirstLogin,
    /// A signed ticket of the provider.
    Ticket,
    /// Linked to an existing player with a link code.
    LinkCode,
    /// Set by a campaign admin.
    Admin,
}

impl VerifiedBy {
    pub fn as_str(self) -> &'static str {
        match self {
            VerifiedBy::FirstLogin => "first_login",
            VerifiedBy::Ticket => "ticket",
            VerifiedBy::LinkCode => "link_code",
            VerifiedBy::Admin => "admin",
        }
    }
}

/// Outcome of [`CampaignCore::resolve_identity`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedIdentity {
    pub identity_id: Uuid,
    pub player_id: Uuid,
    pub provider: Provider,
    /// Canonical external id.
    pub external_id: String,
    /// True if this call created the identity (first contact).
    pub created: bool,
}

#[derive(Error, Debug)]
pub enum IdentityError {
    #[error("invalid external id {external_id:?} for provider {provider}")]
    InvalidExternalId { provider: Provider, external_id: String },

    #[error("provider {0} is not enabled")]
    ProviderDisabled(Provider),

    #[error("storage error: {0}")]
    Storage(String),
}

impl From<IdentityError> for ClientError {
    fn from(err: IdentityError) -> Self {
        ClientError::Runtime(err.to_string())
    }
}

/// Every campaign operation a protocol layer may call.
///
/// Grouped like the original protocol; each method names the command it
/// answers. Methods without a session (`&mut Client`) do not depend on the
/// player. All methods return the original response command, so a gateway
/// speaking the original protocol forwards it unchanged.
pub trait CampaignCore: Send + Sync {
    // ---- Identity -------------------------------------------------------

    /// Find the player behind `(provider, external_id)`. On first contact a
    /// player (with its campaign account) and the identity are created and an
    /// audit row is written. `source` names the caller (for example the
    /// gateway) in the audit log.
    fn resolve_identity(
        &self,
        provider: Provider,
        external_id: &str,
        verified_by: VerifiedBy,
        source: &str,
    ) -> impl Future<Output = Result<ResolvedIdentity, IdentityError>> + Send;

    /// Bind a resolved identity to a session: sets `client.xuid` to the
    /// player_id and loads name, faction, language and ban state, like the
    /// upstream handshake does after `0x2020`. Returns false if the player is
    /// banned (the session must then be rejected).
    fn open_session(
        &self,
        client: &mut Client,
        identity: &ResolvedIdentity,
    ) -> impl Future<Output = Result<bool, ClientError>> + Send;

    // ---- War (0x1030 ..) ------------------------------------------------

    /// TurnDays.
    fn turn_days(&self, client: &mut Client) -> impl Future<Output = CommandResult> + Send;
    /// CampaignPeriod.
    fn campaign_period(&self, client: &mut Client) -> impl Future<Output = CommandResult> + Send;
    /// WarTide.
    fn war_tide(&self, client: &mut Client, round: u16, turn: u16, map: u8)
    -> impl Future<Output = CommandResult> + Send;
    /// WarInfluence.
    fn war_influence(&self, client: &mut Client, round: u16, turn: u16)
    -> impl Future<Output = CommandResult> + Send;
    /// RoundPoints.
    fn round_points(&self, client: &mut Client, round: u16) -> impl Future<Output = CommandResult> + Send;

    // ---- Missions: report results ----------------------------------------

    /// Analyze.
    fn analyze(&self, client: &mut Client, data: [u32; 10]) -> impl Future<Output = CommandResult> + Send;
    /// AddUserPoint: the player's stats of one mission.
    fn report_user_points(
        &self,
        client: &mut Client,
        round: u16,
        turn: u16,
        map: u8,
        points: [u32; 10],
    ) -> impl Future<Output = CommandResult> + Send;
    /// AddMissionPoint: points of one mission for a faction.
    fn report_mission_points(
        &self,
        client: &mut Client,
        round: u16,
        turn: u16,
        map: u8,
        points: u32,
        faction: Faction,
    ) -> impl Future<Output = CommandResult> + Send;

    // ---- Message of the day and message board ---------------------------

    /// FreeMessage.
    fn free_message(&self, client: &mut Client, language: Language) -> impl Future<Output = CommandResult> + Send;
    /// FixedMessage.
    fn fixed_message(&self, client: &mut Client) -> impl Future<Output = CommandResult> + Send;
    /// MessageCount.
    fn message_count(&self, client: &mut Client) -> impl Future<Output = CommandResult> + Send;
    /// MessagePost.
    fn message_post(&self, client: &mut Client) -> impl Future<Output = CommandResult> + Send;

    // ---- Pilot ----------------------------------------------------------

    /// GetUserInfo: load the pilot.
    fn get_pilot(&self, client: &mut Client) -> impl Future<Output = CommandResult> + Send;
    /// NewUserInfo: register a pilot and pick a faction.
    fn create_pilot(
        &self,
        client: &mut Client,
        pilot_name: String,
        xlive_name: String,
        faction: Faction,
        pilot_data: PilotData,
    ) -> impl Future<Output = CommandResult> + Send;
    /// SetUserInfo: save the pilot.
    fn update_pilot(&self, client: &mut Client, pilot_data: PilotData) -> impl Future<Output = CommandResult> + Send;
    /// DelUserInfo: the pilot died (moves to the graveyard).
    fn delete_pilot(&self, client: &mut Client) -> impl Future<Output = CommandResult> + Send;
    /// SetUserFaction: change faction.
    fn set_faction(&self, client: &mut Client, faction: Faction) -> impl Future<Output = CommandResult> + Send;

    // ---- Garage ---------------------------------------------------------

    /// VtOwned.
    fn list_vts(&self, client: &mut Client) -> impl Future<Output = CommandResult> + Send;
    /// VtAdd.
    fn add_vts(&self, client: &mut Client, vt_list: VTIDList) -> impl Future<Output = CommandResult> + Send;
    /// VtDel.
    fn remove_vts(&self, client: &mut Client, vt_list: VTEntryList) -> impl Future<Output = CommandResult> + Send;
    /// VtCapture.
    fn capture_vts(
        &self,
        client: &mut Client,
        pilot_name: String,
        vt_list: VTEntryList,
    ) -> impl Future<Output = CommandResult> + Send;
    /// VtSortie.
    fn sortie_vts(&self, client: &mut Client, vt_list: VTEntryList) -> impl Future<Output = CommandResult> + Send;
    /// VtReturn.
    fn return_vts(&self, client: &mut Client, vt_list: VTEntryList) -> impl Future<Output = CommandResult> + Send;

    // ---- Shop -----------------------------------------------------------

    /// ShopList.
    fn list_shop(&self, client: &mut Client) -> impl Future<Output = CommandResult> + Send;
    /// VtPotentialCost.
    fn vt_potential_cost(&self, client: &mut Client) -> impl Future<Output = CommandResult> + Send;

    // ---- Leaderboard ----------------------------------------------------

    /// RankValues.
    #[allow(clippy::too_many_arguments)]
    fn rank_values(
        &self,
        client: &mut Client,
        round: u16,
        turn: u16,
        faction: Faction,
        category: RankCategory,
        start: u32,
        count: u16,
    ) -> impl Future<Output = CommandResult> + Send;
    /// RankCountGlobal.
    fn rank_count_global(
        &self,
        client: &mut Client,
        round: u16,
        turn: u16,
        category: RankCategory,
    ) -> impl Future<Output = CommandResult> + Send;
    /// RankCountFaction.
    fn rank_count_faction(
        &self,
        client: &mut Client,
        round: u16,
        turn: u16,
        category: RankCategory,
        faction: Faction,
    ) -> impl Future<Output = CommandResult> + Send;
    /// RankCountCategories.
    fn rank_count_categories(&self, client: &mut Client, round: u16, turn: u16)
    -> impl Future<Output = CommandResult> + Send;

    // ---- Awards ---------------------------------------------------------

    /// AwardAvailable.
    fn award_available(&self, client: &mut Client) -> impl Future<Output = CommandResult> + Send;
    /// AwardPoints.
    fn award_points(&self, client: &mut Client, is_turn: bool) -> impl Future<Output = CommandResult> + Send;
    /// AwardRanks.
    fn award_ranks(
        &self,
        client: &mut Client,
        round: u16,
        turn: u16,
        faction: Faction,
    ) -> impl Future<Output = CommandResult> + Send;
    /// AwardClaim.
    fn claim_award(
        &self,
        client: &mut Client,
        round: u16,
        turn: u16,
        vt_list: VTIDList,
        pilot_data: PilotData,
    ) -> impl Future<Output = CommandResult> + Send;

    // ---- Trading --------------------------------------------------------

    /// TradeToken.
    fn create_trade_token(&self, client: &mut Client, pilot_name: String)
    -> impl Future<Output = CommandResult> + Send;
    /// TradeUserInfo.
    fn trade_pilot(
        &self,
        client: &mut Client,
        pilot_name: String,
        token: u32,
        pilot_data: PilotData,
    ) -> impl Future<Output = CommandResult> + Send;
    /// TradeVT.
    fn trade_vts(
        &self,
        client: &mut Client,
        token: u32,
        pilot_name: String,
        rx_vt_list: VTEntryList,
        tx_vt_list: VTEntryList,
    ) -> impl Future<Output = CommandResult> + Send;
}
