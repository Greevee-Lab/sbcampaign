// SPDX-License-Identifier: AGPL-3.0-only
//! Players and identities (steelcore ADR-0002), schema in `sql/fork/identity.sql`.
//!
//! The player_id is the account UUID the campaign server already uses: the
//! XUID as UUID (`Uuid::from_u64_pair(0, xuid)`, as in the upstream handshake).
//! A Steelbound account gets the same derivation applied to its pseudo-XUID,
//! so pilot, garage, awards and leaderboard work unchanged for it.

use crate::{
    client::{Client, ClientError, Faction, Language},
    core_api::{IdentityError, Provider, ResolvedIdentity, STEELBOUND_XUID_PREFIX, VerifiedBy},
};
use sqlx::PgPool;
use uuid::Uuid;

impl From<sqlx::Error> for IdentityError {
    fn from(err: sqlx::Error) -> Self {
        IdentityError::Storage(err.to_string())
    }
}

/// The account UUID of an XUID, exactly as the upstream handshake derives it.
pub fn player_id_from_xuid(xuid: u64) -> Uuid {
    Uuid::from_u64_pair(0, xuid)
}

/// Pseudo-XUID of a Steelbound account (0 is not an account).
pub fn steelbound_pseudo_xuid(account_id: u32) -> Option<u64> {
    (account_id != 0).then(|| (u64::from(STEELBOUND_XUID_PREFIX) << 32) | u64::from(account_id))
}

/// The XUID the game reports for an identity, if the provider has one.
pub fn xuid_of(provider: Provider, canonical_external_id: &str) -> Option<u64> {
    match provider {
        Provider::Steelbound => canonical_external_id.parse::<u32>().ok().and_then(steelbound_pseudo_xuid),
        Provider::Insignia => u64::from_str_radix(canonical_external_id, 16).ok().filter(|&x| x != 0),
    }
}

/// The player_id a new identity gets.
pub fn derive_player_id(provider: Provider, canonical_external_id: &str) -> Option<Uuid> {
    xuid_of(provider, canonical_external_id).map(player_id_from_xuid)
}

fn clip(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}

/// See [`crate::core_api::CampaignCore::resolve_identity`].
pub async fn resolve_identity(
    pool: &PgPool,
    provider: Provider,
    external_id: &str,
    verified_by: VerifiedBy,
    source: &str,
) -> Result<ResolvedIdentity, IdentityError> {
    let canonical = provider.canonical_external_id(external_id)?;
    let resolved = |identity_id, player_id, created| ResolvedIdentity {
        identity_id,
        player_id,
        provider,
        external_id: canonical.clone(),
        created,
    };

    // Known identity: refresh last_seen_at. Unknown or disabled provider: refuse.
    let known: Option<(bool, Option<Uuid>, Option<Uuid>)> = sqlx::query_as(
        "SELECT p.enabled, i.identity_id, i.player_id
         FROM provider p
         LEFT JOIN identity i ON i.provider_id = p.provider_id AND i.external_id = $2
         WHERE p.provider_id = $1",
    )
    .bind(provider.id())
    .bind(&canonical)
    .fetch_optional(pool)
    .await?;

    match known {
        None | Some((false, _, _)) => return Err(IdentityError::ProviderDisabled(provider)),
        Some((true, Some(identity_id), Some(player_id))) => {
            sqlx::query("UPDATE identity SET last_seen_at = now() WHERE identity_id = $1")
                .bind(identity_id)
                .execute(pool)
                .await?;
            return Ok(resolved(identity_id, player_id, false));
        }
        Some((true, _, _)) => {}
    }

    // First contact: account (the player's campaign record), player, identity.
    let player_id = derive_player_id(provider, &canonical).ok_or_else(|| IdentityError::InvalidExternalId {
        provider,
        external_id: canonical.clone(),
    })?;

    let mut tx = pool.begin().await?;

    sqlx::query("INSERT INTO accounts (xuid) VALUES ($1) ON CONFLICT (xuid) DO NOTHING")
        .bind(player_id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("INSERT INTO player (player_id) VALUES ($1) ON CONFLICT (player_id) DO NOTHING")
        .bind(player_id)
        .execute(&mut *tx)
        .await?;

    let inserted: Option<(Uuid, Uuid)> = sqlx::query_as(
        "INSERT INTO identity (provider_id, external_id, player_id, verified_by)
         VALUES ($1, $2, $3, $4)
         ON CONFLICT (provider_id, external_id) DO NOTHING
         RETURNING identity_id, player_id",
    )
    .bind(provider.id())
    .bind(&canonical)
    .bind(player_id)
    .bind(verified_by.as_str())
    .fetch_optional(&mut *tx)
    .await?;

    let result = match inserted {
        Some((identity_id, player_id)) => {
            sqlx::query(
                "INSERT INTO audit_log (source, action, identity_id, player_id, detail)
                 VALUES ($1, 'identity_created', $2, $3, $4)",
            )
            .bind(clip(source, 64))
            .bind(identity_id)
            .bind(player_id)
            .bind(format!("{} verified_by={}", provider.id(), verified_by.as_str()))
            .execute(&mut *tx)
            .await?;
            resolved(identity_id, player_id, true)
        }
        // Created concurrently by another session: use that row.
        None => {
            let (identity_id, player_id): (Uuid, Uuid) = sqlx::query_as(
                "SELECT identity_id, player_id FROM identity WHERE provider_id = $1 AND external_id = $2",
            )
            .bind(provider.id())
            .bind(&canonical)
            .fetch_one(&mut *tx)
            .await?;
            resolved(identity_id, player_id, false)
        }
    };

    tx.commit().await?;
    Ok(result)
}

/// See [`crate::core_api::CampaignCore::open_session`]. Same account lookup
/// as the upstream handshake; also refuses suspended identities.
pub async fn open_session(pool: &PgPool, client: &mut Client, resolved: &ResolvedIdentity) -> Result<bool, ClientError> {
    #[derive(sqlx::FromRow)]
    struct Account {
        xname: Option<String>,
        faction: Faction,
        language: Option<Language>,
        banned: bool,
    }

    let account: Account = sqlx::query_as(
        "WITH inserted AS (
            INSERT INTO accounts (xuid) VALUES ($1) ON CONFLICT (xuid) DO NOTHING
            RETURNING xname, faction, \"language\", banned
        ) SELECT * FROM inserted
        UNION ALL
        SELECT xname, faction, \"language\", banned FROM accounts WHERE xuid = $1",
    )
    .bind(resolved.player_id)
    .fetch_one(pool)
    .await?;

    client.xuid = resolved.player_id;
    client.xname = account.xname.unwrap_or_default();
    client.faction = account.faction;
    client.language = account.language;
    client.banned = account.banned;

    // A suspended identity may not play, even with a still valid ticket.
    let suspended: bool = sqlx::query_scalar("SELECT suspended FROM identity WHERE identity_id = $1")
        .bind(resolved.identity_id)
        .fetch_optional(pool)
        .await?
        .unwrap_or(false);

    Ok(!account.banned && !suspended)
}
