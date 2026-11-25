// SPDX-License-Identifier: AGPL-3.0-only
//! Turn length of the campaign.
//!
//! Upstream ends every turn one week after the previous one (`settings.period
//! + INTERVAL '1 WEEK'`). A test campaign needs short turns, so `[campaign]
//! turn_minutes` (optional) overrides the length. Without it nothing changes.

use sqlx::PgPool;
use std::time::Duration;

/// Upstream turn length: one week.
pub const WEEK: Duration = Duration::from_secs(7 * 24 * 3600);
/// Shortest accepted turn (a mission must fit into a turn).
pub const MIN_TURN_MINUTES: u32 = 10;

/// Length of one turn for `[campaign] turn_minutes`.
pub fn turn_length(turn_minutes: Option<u32>) -> Duration {
    match turn_minutes {
        Some(m) => Duration::from_secs(u64::from(m.max(MIN_TURN_MINUTES)) * 60),
        None => WEEK,
    }
}

/// Parse `turn_minutes` from the `[campaign]` table: missing = upstream
/// behaviour, anything that is not a positive integer is refused.
pub fn parse_turn_minutes(campaign: &toml::Table) -> Option<u32> {
    let v = campaign.get("turn_minutes")?;
    let m = v
        .as_integer()
        .and_then(|i| u32::try_from(i).ok())
        .filter(|&m| m > 0)
        .expect("campaign.turn_minutes must be a positive integer");
    Some(m.max(MIN_TURN_MINUTES))
}

/// With a short turn length, end the running turn no later than one turn
/// from now (a test database seeded with a one-week turn would otherwise wait
/// for that week). Returns true when the period was moved.
pub async fn clamp_running_turn(pg: &PgPool, turn: Duration) -> Result<bool, sqlx::Error> {
    if turn >= WEEK {
        return Ok(false);
    }
    let moved = sqlx::query(
        "UPDATE settings SET period = now() + make_interval(secs => $1)
         WHERE (round, turn) = (SELECT round, turn FROM settings ORDER BY round DESC, turn DESC LIMIT 1)
           AND period > now() + make_interval(secs => $1)",
    )
    .bind(turn.as_secs() as f64)
    .execute(pg)
    .await?
    .rows_affected();
    Ok(moved > 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_one_week() {
        assert_eq!(turn_length(None), WEEK);
        assert_eq!(parse_turn_minutes(&toml::Table::new()), None);
    }

    #[test]
    fn short_turns_have_a_floor() {
        assert_eq!(turn_length(Some(60)), Duration::from_secs(3600));
        assert_eq!(turn_length(Some(1)), Duration::from_secs(600));
        let t: toml::Table = "turn_minutes = 60".parse().unwrap();
        assert_eq!(parse_turn_minutes(&t), Some(60));
        let t: toml::Table = "turn_minutes = 3".parse().unwrap();
        assert_eq!(parse_turn_minutes(&t), Some(MIN_TURN_MINUTES));
    }

    #[test]
    #[should_panic(expected = "positive integer")]
    fn invalid_values_are_refused() {
        let t: toml::Table = "turn_minutes = -5".parse().unwrap();
        parse_turn_minutes(&t);
    }
}

#[cfg(test)]
mod db_tests {
    use super::*;
    use crate::campaign_core::tests::{global_pool, globals_runtime};

    #[test]
    fn db_short_turn_clamps_the_running_turn() {
        globals_runtime().block_on(async {
            let Some(pool) = global_pool().await else { return };
            let latest = "SELECT round, turn, period FROM settings ORDER BY round DESC, turn DESC LIMIT 1";
            let (round, turn, before): (i16, i16, chrono::DateTime<chrono::Utc>) =
                sqlx::query_as(latest).fetch_one(&pool).await.unwrap();

            // Upstream length: never touched.
            assert!(!clamp_running_turn(&pool, WEEK).await.unwrap());

            // Push the turn far away, then a 10-minute campaign pulls it in.
            sqlx::query("UPDATE settings SET period = now() + INTERVAL '6 DAYS' WHERE round = $1 AND turn = $2")
                .bind(round)
                .bind(turn)
                .execute(&pool)
                .await
                .unwrap();
            assert!(clamp_running_turn(&pool, turn_length(Some(10))).await.unwrap());
            let left: f64 = sqlx::query_scalar(
                "SELECT EXTRACT(epoch FROM period - now())::float8 FROM settings WHERE round = $1 AND turn = $2",
            )
            .bind(round)
            .bind(turn)
            .fetch_one(&pool)
            .await
            .unwrap();
            assert!(left > 500.0 && left <= 601.0, "{left}");
            // A turn that already ends sooner stays as it is.
            assert!(!clamp_running_turn(&pool, turn_length(Some(60))).await.unwrap());

            sqlx::query("UPDATE settings SET period = $3 WHERE round = $1 AND turn = $2")
                .bind(round)
                .bind(turn)
                .bind(before)
                .execute(&pool)
                .await
                .unwrap();
        });
    }
}
