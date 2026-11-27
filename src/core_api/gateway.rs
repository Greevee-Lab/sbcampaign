// SPDX-License-Identifier: AGPL-3.0-only
//! The core boundary for gateways: write audit, the results ledger and
//! annulment per identity.
//!
//! A gateway (today the Steelbound gateway, `crate::gateway_steelbound`)
//! serves the original protocol through [`super::CampaignCore`]. What it adds
//! on top - an audit row per write command, a record of every reported
//! result so it can be taken back, annulment - goes through this trait, so
//! the gateway itself never touches the database (ADR-0001).

use super::{Provider, profile::StoreError};
use crate::client::Faction;
use std::future::Future;
use uuid::Uuid;

/// Which kind of reported result a ledger row holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResultKind {
    /// `AddUserPoint`: the ten leaderboard stats of one mission.
    UserPoints,
    /// `AddMissionPoint`: war tide points of one mission for a faction.
    MissionPoints,
}

impl ResultKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ResultKind::UserPoints => "user_points",
            ResultKind::MissionPoints => "mission_points",
        }
    }
}

/// One reported result, as it was applied to the campaign.
#[derive(Clone)]
pub struct ResultRecord {
    pub identity_id: Uuid,
    pub player_id: Uuid,
    pub kind: ResultKind,
    pub round: u16,
    pub turn: u16,
    /// The map as reported (within the turn).
    pub map: u8,
    /// User points: the player's faction (leaderboard row). Mission points:
    /// the faction the points were reported for (war tide column).
    pub faction: Faction,
    /// User points: 10 values. Mission points: 1 value.
    pub points: Vec<u32>,
}

/// One row of the write audit. Who (identity, player), which command, when
/// (set by the store), how it ended. Never contents: no names, no pilot data,
/// no points.
/// W531: a result is linked to a sortie of at most this age. A campaign
/// mission lasts at most 60 minutes after the sortie (mission clock 60:00);
/// the rest is headroom for the debriefing and slow reports. An older open
/// sortie belongs to an aborted mission and is never linked.
pub const SORTIE_WINDOW_MINUTES: i32 = 120;

/// W531: the VTs of an accepted sortie (VtSortie 0x1047), in the game's order.
/// The core keeps only VTs the player owns on that side and links the sortie
/// to the player's next user_points result (`record_result`).
#[derive(Clone)]
pub struct SortieRecord {
    pub identity_id: Uuid,
    pub player_id: Uuid,
    pub faction: Faction,
    /// (type, serial) per VT.
    pub vts: Vec<(u16, u32)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GameWrite {
    /// The gateway, for example "gateway-steelbound".
    pub source: &'static str,
    pub provider: Provider,
    pub identity_id: Uuid,
    pub player_id: Uuid,
    /// Protocol command name, for example "SetUserInfo".
    pub command: &'static str,
    /// "ok", "error", "implausible" or "rate_limited".
    pub outcome: &'static str,
}

/// Result of [`GatewayCore::annul_identity_results`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnnulReport {
    pub identity_id: Uuid,
    pub player_id: Uuid,
    /// Ledger rows taken back (or that would be, on a dry run).
    pub user_reports: i64,
    pub mission_reports: i64,
    /// Suspension state of the identity after the call.
    pub suspended: bool,
}

pub trait GatewayCore: Send + Sync + 'static {
    /// Write one audit row for a write command.
    fn audit_game_write(&self, write: GameWrite) -> impl Future<Output = Result<(), StoreError>> + Send;

    /// Remember a result that was applied, so it can be annulled later.
    /// W531: a user_points result is linked to the player's newest open
    /// sortie of the same side (if any, within `SORTIE_WINDOW_MINUTES`).
    fn record_result(&self, record: ResultRecord) -> impl Future<Output = Result<(), StoreError>> + Send;

    /// W531: remember the VTs of an accepted sortie (statistics only).
    fn record_sortie(&self, record: SortieRecord) -> impl Future<Output = Result<(), StoreError>> + Send;

    /// Take back every not yet annulled result of an identity: subtract its
    /// leaderboard stats and war tide points (never below zero), mark the
    /// ledger rows annulled, optionally suspend the identity, write an audit
    /// row. With `dry_run` only count. None if the identity does not exist.
    fn annul_identity_results(
        &self,
        provider: Provider,
        external_id: &str,
        suspend: bool,
        dry_run: bool,
        source: &str,
    ) -> impl Future<Output = Result<Option<AnnulReport>, StoreError>> + Send;
}
