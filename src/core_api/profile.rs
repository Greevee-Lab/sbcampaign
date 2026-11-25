// SPDX-License-Identifier: AGPL-3.0-only
//! The core boundary for the network Core API (steelcore.v1): identities,
//! players, display names and the API's own bookkeeping (idempotency, audit,
//! provider accesses).
//!
//! Like [`super::CampaignCore`], this trait is the only way the API layer
//! (`core_api::net`) reaches the campaign data. The PostgreSQL implementation
//! lives in `campaign_core::profile`; tests use an in-memory one.

use super::{
    Provider,
    names::{NamePolicy, NameStatus, Verdict},
};
use std::future::Future;
use thiserror::Error;
use uuid::Uuid;

#[derive(Error, Debug)]
pub enum StoreError {
    #[error("invalid external id")]
    InvalidExternalId,
    #[error("provider {0} is not enabled")]
    ProviderDisabled(Provider),
    #[error("storage error: {0}")]
    Storage(String),
}

impl From<super::IdentityError> for StoreError {
    fn from(err: super::IdentityError) -> Self {
        match err {
            super::IdentityError::InvalidExternalId { .. } => StoreError::InvalidExternalId,
            super::IdentityError::ProviderDisabled(p) => StoreError::ProviderDisabled(p),
            super::IdentityError::Storage(s) => StoreError::Storage(s),
        }
    }
}

/// An identity as seen by the API.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdentityView {
    pub identity_id: Uuid,
    pub player_id: Uuid,
    pub external_id: String,
    pub created: bool,
    pub display_name: Option<String>,
    pub suspended: bool,
}

/// A player as seen by one provider.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlayerView {
    pub player_id: Uuid,
    pub display_name: Option<String>,
    /// Unix seconds of the last name change.
    pub display_name_changed_at: Option<i64>,
    /// External ids of the player at the asking provider (empty for callers
    /// without a provider and for players without an identity there).
    pub external_ids: Vec<String>,
}

/// Result of a display name change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SetNameOutcome {
    pub verdict: Verdict,
    /// The player after the call.
    pub player: PlayerView,
}

impl SetNameOutcome {
    pub fn changed(&self) -> bool {
        self.verdict.status == NameStatus::Ok
    }
}

/// Outcome of claiming an idempotency key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IdempotencyClaim {
    /// First use: run the call, then [`ProfileCore::idempotency_complete`].
    New,
    /// Seen before with the same request: the stored response.
    Replay(Vec<u8>),
    /// Seen before with a different request or method.
    Mismatch,
    /// The first call with this key is still running.
    InProgress,
}

/// One line of the write audit: who, which identity, which call, when, how it
/// ended. Never request contents (no names, no game data).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditEntry {
    pub access: String,
    pub provider: Option<Provider>,
    pub method: &'static str,
    pub identity_id: Option<Uuid>,
    pub player_id: Option<Uuid>,
    /// Short outcome code, for example "ok", "taken", "permission_denied".
    pub outcome: String,
}

/// A configured provider access (mirrored into the database for audit).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccessRecord {
    pub name: String,
    pub provider: Option<Provider>,
    pub cert_sha256: String,
    pub scopes: Vec<String>,
}

pub trait ProfileCore: Send + Sync + 'static {
    /// Find or create the player behind `(provider, external_id)`.
    fn resolve(
        &self,
        provider: Provider,
        external_id: &str,
        source: &str,
    ) -> impl Future<Output = Result<IdentityView, StoreError>> + Send;

    /// The player, or None if unknown. `provider` selects which external ids
    /// are listed.
    fn get_player(
        &self,
        player_id: Uuid,
        provider: Option<Provider>,
    ) -> impl Future<Output = Result<Option<PlayerView>, StoreError>> + Send;

    /// Verdict for a name, optionally on behalf of a player.
    fn check_display_name(
        &self,
        name: &str,
        player_id: Option<Uuid>,
        now: i64,
        policy: &NamePolicy,
    ) -> impl Future<Output = Result<Verdict, StoreError>> + Send;

    /// Set the name if [`super::names::decide`] allows it (atomically).
    /// None if the player does not exist.
    fn set_display_name(
        &self,
        player_id: Uuid,
        name: &str,
        provider: Option<Provider>,
        now: i64,
        policy: &NamePolicy,
    ) -> impl Future<Output = Result<Option<SetNameOutcome>, StoreError>> + Send;

    /// Suspend or reinstate an identity. Returns (identity_id, player_id), or
    /// None if the identity does not exist.
    fn set_identity_suspended(
        &self,
        provider: Provider,
        external_id: &str,
        suspended: bool,
    ) -> impl Future<Output = Result<Option<(Uuid, Uuid)>, StoreError>> + Send;

    fn idempotency_claim(
        &self,
        access: &str,
        key: &str,
        method: &str,
        request_hash: &[u8],
        now: i64,
    ) -> impl Future<Output = Result<IdempotencyClaim, StoreError>> + Send;

    fn idempotency_complete(
        &self,
        access: &str,
        key: &str,
        response: &[u8],
    ) -> impl Future<Output = Result<(), StoreError>> + Send;

    /// Forget a claim whose call failed, so the client may retry.
    fn idempotency_release(&self, access: &str, key: &str) -> impl Future<Output = Result<(), StoreError>> + Send;

    fn audit(&self, entry: AuditEntry) -> impl Future<Output = Result<(), StoreError>> + Send;

    /// Mirror the configured accesses (startup). Accesses no longer configured
    /// are marked revoked, never deleted (audit rows keep their name).
    fn sync_accesses(&self, accesses: &[AccessRecord]) -> impl Future<Output = Result<(), StoreError>> + Send;
}
