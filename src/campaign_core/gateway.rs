// SPDX-License-Identifier: AGPL-3.0-only
//! PostgreSQL implementation of [`GatewayCore`] (schema `sql/fork/gateway.sql`,
//! annulment in `sql/fork/gateway_annul.sql`).

use super::SbCampaignCore;
use crate::core_api::{
    Provider,
    gateway::{AnnulReport, GameWrite, GatewayCore, ResultRecord},
    profile::StoreError,
};
use uuid::Uuid;

impl GatewayCore for SbCampaignCore {
    async fn audit_game_write(&self, w: GameWrite) -> Result<(), StoreError> {
        sqlx::query(
            "INSERT INTO audit_log (source, action, identity_id, player_id, provider_id, method, outcome)
             VALUES ($1, 'game_write', $2, $3, $4, $5, $6)",
        )
        .bind(w.source)
        .bind(w.identity_id)
        .bind(w.player_id)
        .bind(w.provider.id())
        .bind(w.command)
        .bind(w.outcome)
        .execute(self.pool())
        .await?;
        Ok(())
    }

    async fn record_result(&self, r: ResultRecord) -> Result<(), StoreError> {
        let points: Vec<i64> = r.points.iter().map(|&p| i64::from(p)).collect();
        sqlx::query(
            "INSERT INTO gateway_result (identity_id, player_id, kind, round, turn, \"map\", faction, points)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
        )
        .bind(r.identity_id)
        .bind(r.player_id)
        .bind(r.kind.as_str())
        .bind(r.round as i16)
        .bind(r.turn as i16)
        .bind(i16::from(r.map))
        .bind(r.faction)
        .bind(points)
        .execute(self.pool())
        .await?;
        Ok(())
    }

    async fn annul_identity_results(
        &self,
        provider: Provider,
        external_id: &str,
        suspend: bool,
        dry_run: bool,
        source: &str,
    ) -> Result<Option<AnnulReport>, StoreError> {
        let canonical = provider.canonical_external_id(external_id).map_err(StoreError::from)?;
        let row: Option<(Uuid, Uuid, i64, i64, bool)> =
            sqlx::query_as("SELECT * FROM annul_identity_results($1, $2, $3, $4, $5)")
                .bind(provider.id())
                .bind(&canonical)
                .bind(suspend)
                .bind(dry_run)
                .bind(source.chars().take(64).collect::<String>())
                .fetch_optional(self.pool())
                .await?;
        Ok(row.map(|(identity_id, player_id, user_reports, mission_reports, suspended)| AnnulReport {
            identity_id,
            player_id,
            user_reports,
            mission_reports,
            suspended,
        }))
    }
}
