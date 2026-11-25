// SPDX-License-Identifier: AGPL-3.0-only
//! PostgreSQL implementation of [`ProfileCore`] (schema `sql/fork/core_api.sql`).

use super::{SbCampaignCore, identity};
use crate::core_api::{
    Provider, VerifiedBy,
    names::{self, BlockedTerm, NameFacts, NamePolicy, NameStatus, PlayerNameState, Verdict},
    profile::{
        AccessRecord, AuditEntry, IdempotencyClaim, IdentityView, PlayerView, ProfileCore, SetNameOutcome,
        StoreError,
    },
};
use sqlx::{PgConnection, PgPool};
use uuid::Uuid;

impl From<sqlx::Error> for StoreError {
    fn from(err: sqlx::Error) -> Self {
        StoreError::Storage(err.to_string())
    }
}

/// Serializes display name changes (one change at a time is plenty).
const NAME_LOCK: i64 = 0x5374_6c43_6f72_654e; // "StlCoreN"

async fn player_state(conn: &mut PgConnection, player_id: Uuid) -> Result<PlayerNameState, sqlx::Error> {
    let row: Option<(String, i64)> = sqlx::query_as(
        "SELECT name, extract(epoch FROM changed_at)::bigint FROM display_name WHERE player_id = $1",
    )
    .bind(player_id)
    .fetch_optional(&mut *conn)
    .await?;
    Ok(PlayerNameState {
        player_id,
        current: row.as_ref().map(|r| r.0.clone()),
        changed_at: row.map(|r| r.1),
    })
}

async fn name_facts(conn: &mut PgConnection, name: &str, now: i64) -> Result<NameFacts, sqlx::Error> {
    let key = names::name_key(name);
    let skel = names::skeleton(name);
    let owners: Vec<(Uuid, bool)> =
        sqlx::query_as("SELECT player_id, name_key = $1 FROM display_name WHERE name_key = $1 OR skeleton = $2")
            .bind(&key)
            .bind(&skel)
            .fetch_all(&mut *conn)
            .await?;
    let reservations: Vec<(Uuid, bool)> = sqlx::query_as(
        "SELECT player_id, name_key = $1 FROM display_name_reservation
         WHERE skeleton = $2 AND until > to_timestamp($3::double precision)",
    )
    .bind(&key)
    .bind(&skel)
    .bind(now as f64)
    .fetch_all(&mut *conn)
    .await?;
    let terms: Vec<(String, bool)> = sqlx::query_as("SELECT term, contains FROM display_name_blocklist")
        .fetch_all(&mut *conn)
        .await?;
    let terms: Vec<BlockedTerm> = terms
        .into_iter()
        .map(|(t, contains)| BlockedTerm { skeleton: names::skeleton(&t), contains })
        .collect();
    Ok(NameFacts {
        exact_owners: owners.iter().filter(|o| o.1).map(|o| o.0).collect(),
        similar_owners: owners.iter().map(|o| o.0).collect(),
        reservations,
        blocked: names::is_blocked(&skel, &terms),
    })
}

async fn player_view(
    conn: &mut PgConnection,
    player_id: Uuid,
    provider: Option<Provider>,
) -> Result<Option<PlayerView>, sqlx::Error> {
    let row: Option<(Option<String>, Option<i64>)> = sqlx::query_as(
        "SELECT d.name, extract(epoch FROM d.changed_at)::bigint
         FROM player p LEFT JOIN display_name d USING (player_id) WHERE p.player_id = $1",
    )
    .bind(player_id)
    .fetch_optional(&mut *conn)
    .await?;
    let Some((display_name, changed_at)) = row else {
        return Ok(None);
    };
    let external_ids: Vec<String> = match provider {
        Some(p) => sqlx::query_scalar(
            "SELECT external_id FROM identity WHERE player_id = $1 AND provider_id = $2 ORDER BY created_at",
        )
        .bind(player_id)
        .bind(p.id())
        .fetch_all(&mut *conn)
        .await?,
        None => Vec::new(),
    };
    Ok(Some(PlayerView { player_id, display_name, display_name_changed_at: changed_at, external_ids }))
}

fn pool(core: &SbCampaignCore) -> &PgPool {
    core.pool()
}

impl ProfileCore for SbCampaignCore {
    async fn resolve(&self, provider: Provider, external_id: &str, source: &str) -> Result<IdentityView, StoreError> {
        let r = identity::resolve_identity(pool(self), provider, external_id, VerifiedBy::FirstLogin, source).await?;
        let (display_name, suspended): (Option<String>, bool) = sqlx::query_as(
            "SELECT d.name, i.suspended FROM identity i
             LEFT JOIN display_name d ON d.player_id = i.player_id WHERE i.identity_id = $1",
        )
        .bind(r.identity_id)
        .fetch_one(pool(self))
        .await?;
        Ok(IdentityView {
            identity_id: r.identity_id,
            player_id: r.player_id,
            external_id: r.external_id,
            created: r.created,
            display_name,
            suspended,
        })
    }

    async fn get_player(&self, player_id: Uuid, provider: Option<Provider>) -> Result<Option<PlayerView>, StoreError> {
        let mut conn = pool(self).acquire().await?;
        Ok(player_view(&mut conn, player_id, provider).await?)
    }

    async fn check_display_name(
        &self,
        name: &str,
        player_id: Option<Uuid>,
        now: i64,
        policy: &NamePolicy,
    ) -> Result<Verdict, StoreError> {
        let syntax = names::check_syntax(name);
        if syntax != NameStatus::Ok {
            return Ok(names::decide(name, &NameFacts::default(), None, now, policy));
        }
        let mut conn = pool(self).acquire().await?;
        let facts = name_facts(&mut conn, name, now).await?;
        let state = match player_id {
            Some(id) => Some(player_state(&mut conn, id).await?),
            None => None,
        };
        Ok(names::decide(name, &facts, state.as_ref(), now, policy))
    }

    async fn set_display_name(
        &self,
        player_id: Uuid,
        name: &str,
        provider: Option<Provider>,
        now: i64,
        policy: &NamePolicy,
    ) -> Result<Option<SetNameOutcome>, StoreError> {
        let mut tx = pool(self).begin().await?;
        sqlx::query("SELECT pg_advisory_xact_lock($1)").bind(NAME_LOCK).execute(&mut *tx).await?;
        let exists: Option<i32> = sqlx::query_scalar("SELECT 1 FROM player WHERE player_id = $1")
            .bind(player_id)
            .fetch_optional(&mut *tx)
            .await?;
        if exists.is_none() {
            return Ok(None);
        }
        let state = player_state(&mut tx, player_id).await?;
        let verdict = if names::check_syntax(name) == NameStatus::Ok {
            let facts = name_facts(&mut tx, name, now).await?;
            names::decide(name, &facts, Some(&state), now, policy)
        } else {
            names::decide(name, &NameFacts::default(), Some(&state), now, policy)
        };

        if verdict.status == NameStatus::Ok {
            let at = now as f64;
            if let Some(old) = &state.current {
                sqlx::query(
                    "INSERT INTO display_name_reservation (player_id, name_key, skeleton, until)
                     VALUES ($1, $2, $3, to_timestamp($4::double precision))
                     ON CONFLICT (player_id, name_key) DO UPDATE SET skeleton = EXCLUDED.skeleton, until = EXCLUDED.until",
                )
                .bind(player_id)
                .bind(names::name_key(old))
                .bind(names::skeleton(old))
                .bind((now + policy.reservation) as f64)
                .execute(&mut *tx)
                .await?;
            }
            sqlx::query(
                "DELETE FROM display_name_reservation
                 WHERE (player_id = $1 AND skeleton = $2) OR until <= to_timestamp($3::double precision)",
            )
            .bind(player_id)
            .bind(names::skeleton(name))
            .bind(at)
            .execute(&mut *tx)
            .await?;
            sqlx::query(
                "INSERT INTO display_name (player_id, name, name_key, skeleton, changed_at)
                 VALUES ($1, $2, $3, $4, to_timestamp($5::double precision))
                 ON CONFLICT (player_id) DO UPDATE SET name = EXCLUDED.name, name_key = EXCLUDED.name_key,
                     skeleton = EXCLUDED.skeleton, changed_at = EXCLUDED.changed_at",
            )
            .bind(player_id)
            .bind(name)
            .bind(names::name_key(name))
            .bind(names::skeleton(name))
            .bind(at)
            .execute(&mut *tx)
            .await?;
        }
        let player = player_view(&mut tx, player_id, provider).await?.ok_or_else(|| {
            StoreError::Storage("player vanished".into())
        })?;
        tx.commit().await?;
        Ok(Some(SetNameOutcome { verdict, player }))
    }

    async fn set_identity_suspended(
        &self,
        provider: Provider,
        external_id: &str,
        suspended: bool,
    ) -> Result<Option<(Uuid, Uuid)>, StoreError> {
        let canonical = provider.canonical_external_id(external_id)?;
        Ok(sqlx::query_as(
            "UPDATE identity SET suspended = $3 WHERE provider_id = $1 AND external_id = $2
             RETURNING identity_id, player_id",
        )
        .bind(provider.id())
        .bind(canonical)
        .bind(suspended)
        .fetch_optional(pool(self))
        .await?)
    }

    async fn idempotency_claim(
        &self,
        access: &str,
        key: &str,
        method: &str,
        request_hash: &[u8],
        _now: i64,
    ) -> Result<IdempotencyClaim, StoreError> {
        let p = pool(self);
        sqlx::query("DELETE FROM api_idempotency WHERE created_at < now() - interval '24 hours'")
            .execute(p)
            .await?;
        let inserted: Option<i32> = sqlx::query_scalar(
            "INSERT INTO api_idempotency (access_id, key, method, request_hash) VALUES ($1, $2, $3, $4)
             ON CONFLICT (access_id, key) DO NOTHING RETURNING 1",
        )
        .bind(access)
        .bind(key)
        .bind(method)
        .bind(request_hash)
        .fetch_optional(p)
        .await?;
        if inserted.is_some() {
            return Ok(IdempotencyClaim::New);
        }
        let row: Option<(String, Vec<u8>, Option<Vec<u8>>)> = sqlx::query_as(
            "SELECT method, request_hash, response FROM api_idempotency WHERE access_id = $1 AND key = $2",
        )
        .bind(access)
        .bind(key)
        .fetch_optional(p)
        .await?;
        Ok(match row {
            // Expired and deleted in between: treat as in progress, the client retries.
            None => IdempotencyClaim::InProgress,
            Some((m, h, _)) if m != method || h != request_hash => IdempotencyClaim::Mismatch,
            Some((_, _, None)) => IdempotencyClaim::InProgress,
            Some((_, _, Some(resp))) => IdempotencyClaim::Replay(resp),
        })
    }

    async fn idempotency_complete(&self, access: &str, key: &str, response: &[u8]) -> Result<(), StoreError> {
        sqlx::query("UPDATE api_idempotency SET response = $3 WHERE access_id = $1 AND key = $2")
            .bind(access)
            .bind(key)
            .bind(response)
            .execute(pool(self))
            .await?;
        Ok(())
    }

    async fn idempotency_release(&self, access: &str, key: &str) -> Result<(), StoreError> {
        sqlx::query("DELETE FROM api_idempotency WHERE access_id = $1 AND key = $2 AND response IS NULL")
            .bind(access)
            .bind(key)
            .execute(pool(self))
            .await?;
        Ok(())
    }

    async fn audit(&self, e: AuditEntry) -> Result<(), StoreError> {
        sqlx::query(
            "INSERT INTO audit_log (source, action, identity_id, player_id, access_id, provider_id, method, outcome)
             VALUES ($1, 'api_write', $2, $3, $1, $4, $5, $6)",
        )
        .bind(&e.access)
        .bind(e.identity_id)
        .bind(e.player_id)
        .bind(e.provider.map(|p| p.id()))
        .bind(e.method)
        .bind(&e.outcome)
        .execute(pool(self))
        .await?;
        Ok(())
    }

    async fn sync_accesses(&self, accesses: &[AccessRecord]) -> Result<(), StoreError> {
        let mut tx = pool(self).begin().await?;
        for a in accesses {
            sqlx::query(
                "INSERT INTO api_access (access_id, provider_id, cert_sha256, scopes) VALUES ($1, $2, $3, $4)
                 ON CONFLICT (access_id) DO UPDATE SET provider_id = EXCLUDED.provider_id,
                     cert_sha256 = EXCLUDED.cert_sha256, scopes = EXCLUDED.scopes, updated_at = now(),
                     revoked_at = NULL",
            )
            .bind(&a.name)
            .bind(a.provider.map(|p| p.id()))
            .bind(&a.cert_sha256)
            .bind(&a.scopes)
            .execute(&mut *tx)
            .await?;
        }
        let names: Vec<String> = accesses.iter().map(|a| a.name.clone()).collect();
        sqlx::query("UPDATE api_access SET revoked_at = now() WHERE revoked_at IS NULL AND NOT (access_id = ANY($1))")
            .bind(&names)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }
}
