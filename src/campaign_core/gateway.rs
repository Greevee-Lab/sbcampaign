// SPDX-License-Identifier: AGPL-3.0-only
//! PostgreSQL implementation of [`GatewayCore`] (schema `sql/fork/gateway.sql`,
//! annulment in `sql/fork/gateway_annul.sql`).
//!
//! Game audit and results ledger are campaign data (campaign pool, schema
//! `public`): a season reset empties them together with the leaderboard they
//! explain. The annulment function looks the identity up in the core itself.

use super::SbCampaignCore;
use crate::core_api::{
    Provider,
    gateway::{AnnulReport, GameWrite, GatewayCore, ResultKind, ResultRecord, SORTIE_WINDOW_MINUTES, SortieRecord},
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
        let mut tx = self.pool().begin().await?;
        let result_id: i64 = sqlx::query_scalar(
            "INSERT INTO gateway_result (identity_id, player_id, kind, round, turn, \"map\", faction, points)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8) RETURNING result_id",
        )
        .bind(r.identity_id)
        .bind(r.player_id)
        .bind(r.kind.as_str())
        .bind(r.round as i16)
        .bind(r.turn as i16)
        .bind(i16::from(r.map))
        .bind(r.faction)
        .bind(points)
        .fetch_one(&mut *tx)
        .await?;
        if r.kind == ResultKind::UserPoints {
            // W531: the newest open sortie of this player on this side names
            // the VT of the battle. None (no sortie, too old, other side):
            // the battle simply has no VT. Each sortie belongs to one result.
            sqlx::query(
                "UPDATE battle_vt SET result_id = $1, linked_at = now()
                 WHERE battle_vt_id = (
                     SELECT battle_vt_id FROM battle_vt
                     WHERE player_id = $2 AND faction = $3 AND result_id IS NULL
                       AND sortied_at > now() - make_interval(mins => $4)
                     ORDER BY sortied_at DESC, battle_vt_id DESC
                     LIMIT 1
                     FOR UPDATE SKIP LOCKED)",
            )
            .bind(result_id)
            .bind(r.player_id)
            .bind(r.faction)
            .bind(SORTIE_WINDOW_MINUTES)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    async fn record_sortie(&self, r: SortieRecord) -> Result<(), StoreError> {
        // Only VTs the player owns on this side count (the game sends the
        // garage serials; anything else is ignored), in the game's order.
        let types: Vec<i16> = r.vts.iter().map(|&(t, _)| t as i16).collect();
        let serials: Vec<i64> = r.vts.iter().map(|&(_, s)| i64::from(s)).collect();
        sqlx::query(
            "INSERT INTO battle_vt (identity_id, player_id, faction, vt_type, vt_types)
             SELECT $1, $2, $3, s.t[1], s.t
             FROM (SELECT array_agg(v.type ORDER BY v.ord) AS t
                   FROM unnest($4::smallint[], $5::bigint[]) WITH ORDINALITY AS v(type, serial, ord)
                   WHERE EXISTS (SELECT 1 FROM garage g
                                 WHERE g.\"type\" = v.type AND g.\"serial\" = v.serial
                                   AND g.xuid = $2 AND g.faction = $3)) s
             WHERE s.t IS NOT NULL",
        )
        .bind(r.identity_id)
        .bind(r.player_id)
        .bind(r.faction)
        .bind(types)
        .bind(serials)
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
