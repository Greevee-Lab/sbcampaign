use chrono::{DateTime, Utc};
use std::time::Duration;
use tokio::time::sleep;
use tokio_util::sync::CancellationToken;

#[derive(sqlx::FromRow)]
struct RoundTurnTime {
    round: i16,
    turn: i16,
    remaining: i32,
    period: DateTime<Utc>,
}

async fn main_inner() -> Result<Duration, sqlx::Error> {
    let _ = systemd_log!(7, "Service Task: Executing");

    let pg = crate::get_pool();
    let cfg = crate::get_config();

    let mut round_turn_time: RoundTurnTime = match sqlx::query_as("SELECT round, turn, EXTRACT(epoch FROM (period - now()))::int AS remaining, period FROM settings ORDER BY round DESC, turn DESC LIMIT 1").fetch_one(pg).await {
        Ok(rtt) => rtt,
        Err(sqlx::Error::RowNotFound) => {
            let _ = systemd_log!(1, "Service Task: No round data found in settings table. Insert a new row to begin the campaign");

            return Ok(Duration::from_mins(1));
        },
        err => err?,
    };

    let remaining = if round_turn_time.remaining > 0 {
        Duration::from_secs(round_turn_time.remaining as u64)
    } else {
        Duration::ZERO
    };

    let turn_length = crate::campaign_core::turn::turn_length(cfg.turn_minutes);
    let shop_drop_interval = match cfg.turn_minutes {
        Some(_) => turn_length / cfg.shop_drop_count,
        None => Duration::from_hours(cfg.turn_days as u64 * 24) / cfg.shop_drop_count,
    };

    if remaining > shop_drop_interval {
        sqlx::query("CALL shop_drop($1, $2)")
            .bind(round_turn_time.turn)
            .bind(1.0 / cfg.shop_drop_count as f32)
            .execute(pg)
            .await?;

        let _ = systemd_log!(7, "Service Task: Randomly adding VT drops to the shop");
        return Ok(shop_drop_interval);
    }

    // Wait for the turn to finish
    if remaining > Duration::ZERO {
        return Ok(remaining + Duration::from_secs(1));
    }

    let _ = systemd_log!(
        7,
        "Service Task: Restocking shop and distributing turn awards"
    );

    // Restock the shop
    sqlx::query("UPDATE shop SET vtstock = vtlimit")
        .execute(pg)
        .await?;

    sqlx::query("CALL give_turn_award($1, $2)")
        .bind(round_turn_time.round)
        .bind(round_turn_time.turn)
        .execute(pg)
        .await?;

    let faction_reset;
    if round_turn_time.turn < 8 {
        round_turn_time.turn += 1;

        faction_reset = round_turn_time.turn == 3 || round_turn_time.turn == 5
    } else {
        sqlx::query("CALL give_round_award($1)")
            .bind(round_turn_time.round)
            .execute(pg)
            .await?;

        // Reset to defaults for new round
        faction_reset = true;
        round_turn_time.round += 1;
        round_turn_time.turn = 1;
    }

    if faction_reset {
        let _ = systemd_log!(7, "Service Task: Resetting factions");
        sqlx::query("UPDATE accounts SET faction = 'none'")
            .execute(pg)
            .await?;
    }

    let _ = systemd_log!(
        7,
        "Service Task: Updating settings to Round {}, Turn {}",
        round_turn_time.round,
        round_turn_time.turn
    );
    sqlx::query(
        "INSERT INTO settings (round, turn, period) VALUES ($1, $2, $3 + make_interval(secs => $4))",
    )
    .bind(round_turn_time.round)
    .bind(round_turn_time.turn)
    .bind(round_turn_time.period)
    .bind(turn_length.as_secs() as f64)
    .execute(pg)
    .await?;

    return Ok(shop_drop_interval);
}

pub async fn main(cancel_token: CancellationToken) {
    let _ = systemd_log!(7, "Service Task: Started");

    let turn_length = crate::campaign_core::turn::turn_length(crate::get_config().turn_minutes);
    match crate::campaign_core::turn::clamp_running_turn(crate::get_pool(), turn_length).await {
        Ok(true) => {
            let _ = systemd_log!(5, "Service Task: turn length {} min, running turn shortened", turn_length.as_secs() / 60);
        }
        Ok(false) => {}
        Err(err) => {
            let _ = systemd_log!(3, "Service Task (Error): {}", err);
        }
    }

    loop {
        let sleep_duration = match main_inner().await {
            Ok(d) => d,
            Err(err) => {
                let _ = systemd_log!(3, "Service Task (Error): {}", err);
                Duration::from_mins(1)
            }
        };

        let _ = systemd_log!(
            7,
            "Service Task: Sleeping until {}",
            chrono::Local::now() + sleep_duration
        );

        tokio::select! {
            biased;

            _ = cancel_token.cancelled() => {
                break;
            }

            _ = sleep(sleep_duration) => {}
        }
    }

    let _ = systemd_log!(7, "Service Task: Stopped");
}
