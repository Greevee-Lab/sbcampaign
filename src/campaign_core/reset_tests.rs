// SPDX-License-Identifier: AGPL-3.0-only
//! A season reset (`campaign_reset`, `sql/fork/season_reset.sql`) empties the
//! campaign and keeps the core: players, identities, display names, the
//! 30-day reservation and the 7-day change limit. Synthetic data only.
//!
//! The reset empties every campaign table of its database, so this test needs
//! a database of its own: `SBCAMPAIGN_TEST_RESET_DATABASE_URL` (migrated with
//! `deploy/server/migrate.sh`). Without it the test is skipped, unless
//! `SBCAMPAIGN_REQUIRE_DB_TESTS=1`.

use super::{
    SbCampaignCore,
    tests::{core_pool, session, synthetic_account},
};
use crate::core_api::{
    CampaignCore, Provider, VerifiedBy,
    names::{NamePolicy, NameStatus},
    profile::ProfileCore,
};
use sqlx::{PgPool, postgres::PgPoolOptions};

async fn reset_pool() -> Option<PgPool> {
    match std::env::var("SBCAMPAIGN_TEST_RESET_DATABASE_URL") {
        Ok(url) if !url.is_empty() => Some(
            PgPoolOptions::new()
                .max_connections(4)
                .connect(&url)
                .await
                .expect("connect to SBCAMPAIGN_TEST_RESET_DATABASE_URL"),
        ),
        _ => {
            if std::env::var("SBCAMPAIGN_REQUIRE_DB_TESTS").is_ok_and(|v| v == "1") {
                panic!("SBCAMPAIGN_REQUIRE_DB_TESTS=1 but SBCAMPAIGN_TEST_RESET_DATABASE_URL is not set");
            }
            eprintln!("skipped: SBCAMPAIGN_TEST_RESET_DATABASE_URL not set");
            None
        }
    }
}

async fn count(pool: &PgPool, sql: &'static str, player: uuid::Uuid) -> i64 {
    sqlx::query_scalar(sql).bind(player).fetch_one(pool).await.unwrap()
}

async fn core_fingerprint(pool: &PgPool) -> String {
    sqlx::query_scalar("SELECT campaign_reset_core_fingerprint()").fetch_one(pool).await.unwrap()
}

#[tokio::test]
async fn db_core_survives_season_reset() {
    let Some(pool) = reset_pool().await else { return };
    let core = SbCampaignCore::new(pool.clone(), core_pool(&pool).await);
    let policy = NamePolicy::default();
    let now = chrono::Utc::now().timestamp();
    let day = 86_400;

    let a = synthetic_account(20);
    let b = synthetic_account(21);
    let name1 = format!("Rs{}", a % 10_000_000);
    let name2 = format!("Rt{}", a % 10_000_000);

    // Core: two players; A named 10 days ago, renamed yesterday (old name
    // reserved for 30 days, next change in 6 days).
    let ia = core.resolve(Provider::Steelbound, &a.to_string(), "test").await.unwrap();
    let ib = core.resolve(Provider::Steelbound, &b.to_string(), "test").await.unwrap();
    let first = core.set_display_name(ia.player_id, &name1, Some(Provider::Steelbound), now - 10 * day, &policy);
    assert!(first.await.unwrap().unwrap().changed());
    let second = core.set_display_name(ia.player_id, &name2, Some(Provider::Steelbound), now - day, &policy);
    assert!(second.await.unwrap().unwrap().changed());

    // Campaign: A plays (account row), owns a VT, has stats.
    let (ours, _theirs) = tokio::io::duplex(64);
    let mut client = session(Box::new(ours));
    let resolved = core
        .resolve_identity(Provider::Steelbound, &a.to_string(), VerifiedBy::Ticket, "test")
        .await
        .unwrap();
    assert!(core.open_session(&mut client, &resolved).await.unwrap());
    sqlx::query("INSERT INTO garage (\"type\", \"serial\", xuid, faction) VALUES (1, 900001, $1, 'hsd')")
        .bind(ia.player_id)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO leaderboard (xuid, faction, round, turn, \"map\", stat1) VALUES ($1, 'hsd', 1, 1, 1, 42)",
    )
    .bind(ia.player_id)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query("UPDATE shop SET vtstock = 0, \"serial\" = 77 WHERE vtlimit IS NOT NULL")
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(count(&pool, "SELECT count(*) FROM accounts WHERE xuid = $1", ia.player_id).await, 1);

    let before = core_fingerprint(&pool).await;

    // Dry run: reports, changes nothing.
    let report: Vec<(String, String, i64)> =
        sqlx::query_as("SELECT * FROM campaign_reset(false)").fetch_all(&pool).await.unwrap();
    assert!(report.iter().any(|r| r.0 == "campaign" && r.1 == "accounts" && r.2 >= 1));
    assert!(report.iter().any(|r| r.0 == "core" && r.1 == "display_name" && r.2 >= 1));
    assert!(report.iter().any(|r| r.0 == "shop" && r.1 == "shop"));
    assert!(!report.iter().any(|r| r.1 == "fork_migrations"));
    assert_eq!(count(&pool, "SELECT count(*) FROM accounts WHERE xuid = $1", ia.player_id).await, 1);

    // Apply.
    sqlx::query("SELECT * FROM campaign_reset(true, '2030-01-08 18:00+00', 'test')")
        .fetch_all(&pool)
        .await
        .unwrap();

    // The campaign is empty and starts at round 1, turn 1.
    for table in ["accounts", "garage", "leaderboard", "wartide", "awards", "graveyard", "gateway_result"] {
        // Table names from the fixed list above.
        let sql = sqlx::AssertSqlSafe(format!("SELECT count(*) FROM {table}"));
        let n: i64 = sqlx::query_scalar(sql).fetch_one(&pool).await.unwrap();
        assert_eq!(n, 0, "{table} not empty");
    }
    let settings: Vec<(i16, i16, i64)> =
        sqlx::query_as("SELECT round, turn, extract(epoch FROM \"period\")::bigint FROM settings")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(settings, vec![(1, 1, 1_894_125_600)]);
    let audit: Vec<(String, String)> =
        sqlx::query_as("SELECT source, action FROM audit_log").fetch_all(&pool).await.unwrap();
    assert_eq!(audit, vec![("test".to_string(), "campaign_reset".to_string())]);
    let (shop, refilled): (i64, i64) = sqlx::query_as(
        "SELECT count(*), count(*) FILTER (WHERE vtstock IS NOT DISTINCT FROM vtlimit AND \"serial\" = 1) FROM shop",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!((shop, refilled), (31, 31));

    // The core is unchanged, row for row.
    assert_eq!(core_fingerprint(&pool).await, before);

    // Same players, same names, same rules.
    let again = core.resolve(Provider::Steelbound, &a.to_string(), "test").await.unwrap();
    assert!(!again.created);
    assert_eq!((again.player_id, again.display_name.as_deref()), (ia.player_id, Some(name2.as_str())));
    let view = core.get_player(ia.player_id, Some(Provider::Steelbound)).await.unwrap().unwrap();
    assert_eq!(view.display_name_changed_at, Some(now - day));
    // 7 days: A may not change again yet.
    let change = core.set_display_name(ia.player_id, "Fresh Name", Some(Provider::Steelbound), now, &policy);
    let change = change.await.unwrap().unwrap();
    assert_eq!(change.verdict.status, NameStatus::ChangeLimit);
    assert_eq!(change.verdict.next_change_at, now - day + 7 * day);
    // 30 days: B may not take A's old name; A's current name is taken.
    let old = core.check_display_name(&name1, Some(ib.player_id), now, &policy).await.unwrap();
    assert_eq!(old.status, NameStatus::Reserved);
    let cur = core.check_display_name(&name2, Some(ib.player_id), now, &policy).await.unwrap();
    assert_eq!(cur.status, NameStatus::Taken);

    // Playing again creates a fresh campaign account for the same player.
    let (ours2, _theirs2) = tokio::io::duplex(64);
    let mut client2 = session(Box::new(ours2));
    let resolved = core
        .resolve_identity(Provider::Steelbound, &a.to_string(), VerifiedBy::Ticket, "test")
        .await
        .unwrap();
    assert!(!resolved.created);
    assert!(core.open_session(&mut client2, &resolved).await.unwrap());
    assert_eq!(client2.xuid, ia.player_id);
    let (accounts, pilots): (i64, i64) =
        sqlx::query_as("SELECT count(*), count(pilot) FROM accounts WHERE xuid = $1")
            .bind(ia.player_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!((accounts, pilots), (1, 0));
}
