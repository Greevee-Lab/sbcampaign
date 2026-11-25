// SPDX-License-Identifier: AGPL-3.0-only
//! Tests of the campaign core. Synthetic data only.
//!
//! Database tests run against the database in `SBCAMPAIGN_TEST_DATABASE_URL`
//! (schema from `deploy/server/migrate.sh`, i.e. including
//! `sql/fork/identity.sql`). Without it they are skipped, unless
//! `SBCAMPAIGN_REQUIRE_DB_TESTS=1` (set in CI) turns that into a failure.

use super::{SbCampaignCore, identity::*};
use crate::{
    client::{Client, Faction},
    command::{self, tx::TxCommand},
    core_api::{
        CampaignCore, IdentityError, PilotData, Provider, STEELBOUND_XUID_PREFIX, VerifiedBy,
        dispatch::dispatch, transport::BoxedTransport,
    },
};
use sqlx::{PgPool, postgres::PgPoolOptions};
use tokio_util::codec::Framed;
use uuid::Uuid;

// ---- Derivation (no database) -------------------------------------------

#[test]
fn xuid_derivation_matches_upstream_handshake() {
    // client.rs: `RxCommand::XUID(xuid) => Uuid::from_u64_pair(0, xuid)`
    for xuid in [1u64, 0x0009_FFFF_0000_0001, 0xE000_0000_0000_0001, u64::MAX] {
        assert_eq!(player_id_from_xuid(xuid), Uuid::from_u64_pair(0, xuid));
    }
    assert_eq!(
        player_id_from_xuid(0x0009_FFFF_0000_0001).to_string(),
        "00000000-0000-0000-0009-ffff00000001"
    );
}

#[test]
fn steelbound_account_gets_a_fixed_uuid() {
    assert_eq!(steelbound_pseudo_xuid(0), None);
    assert_eq!(steelbound_pseudo_xuid(1), Some(0x5342_4C01_0000_0001));
    assert_eq!(steelbound_pseudo_xuid(u32::MAX), Some(0x5342_4C01_FFFF_FFFF));

    let id = derive_player_id(Provider::Steelbound, "1").unwrap();
    assert_eq!(id.to_string(), "00000000-0000-0000-5342-4c0100000001");
    assert_eq!(id, player_id_from_xuid(0x5342_4C01_0000_0001));
    // Fixed: the same account always maps to the same player.
    assert_eq!(derive_player_id(Provider::Steelbound, "123456"), derive_player_id(Provider::Steelbound, "123456"));
    assert_eq!(
        derive_player_id(Provider::Steelbound, "4294967295").unwrap().to_string(),
        "00000000-0000-0000-5342-4c01ffffffff"
    );
}

#[test]
fn provider_ranges_never_collide() {
    let sb = derive_player_id(Provider::Steelbound, "9").unwrap();
    let xbox = derive_player_id(Provider::Insignia, "0009ffff00000009").unwrap();
    assert_ne!(sb, xbox);
    // An Insignia identity can never be given a Steelbound player_id.
    let pseudo = format!("{:x}00000009", STEELBOUND_XUID_PREFIX);
    assert!(Provider::Insignia.canonical_external_id(&pseudo).is_err());
    assert_eq!(xuid_of(Provider::Insignia, "0009ffff00000009"), Some(0x0009_FFFF_0000_0009));
}

// ---- Database -----------------------------------------------------------

pub(super) async fn test_pool() -> Option<PgPool> {
    match std::env::var("SBCAMPAIGN_TEST_DATABASE_URL") {
        Ok(url) if !url.is_empty() => Some(
            PgPoolOptions::new()
                .max_connections(4)
                .connect(&url)
                .await
                .expect("connect to SBCAMPAIGN_TEST_DATABASE_URL"),
        ),
        _ => {
            if std::env::var("SBCAMPAIGN_REQUIRE_DB_TESTS").is_ok_and(|v| v == "1") {
                panic!("SBCAMPAIGN_REQUIRE_DB_TESTS=1 but SBCAMPAIGN_TEST_DATABASE_URL is not set");
            }
            eprintln!("skipped: SBCAMPAIGN_TEST_DATABASE_URL not set");
            None
        }
    }
}

/// Synthetic Steelbound account ids, unique per test and per run.
pub(super) fn synthetic_account(test: u32) -> u32 {
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().subsec_nanos();
    0x7000_0000 | (test << 24) | (nanos & 0x00FF_FFFF)
}

#[tokio::test]
async fn db_resolve_creates_then_finds_identity() {
    let Some(pool) = test_pool().await else { return };
    let account = synthetic_account(1);
    let ext = account.to_string();

    let first = resolve_identity(&pool, Provider::Steelbound, &ext, VerifiedBy::Ticket, "test").await.unwrap();
    assert!(first.created);
    assert_eq!(first.player_id, derive_player_id(Provider::Steelbound, &ext).unwrap());
    assert_eq!(first.external_id, ext);

    let again = resolve_identity(&pool, Provider::Steelbound, &ext, VerifiedBy::Ticket, "test").await.unwrap();
    assert!(!again.created);
    assert_eq!(again.identity_id, first.identity_id);
    assert_eq!(again.player_id, first.player_id);

    // Player, campaign account and exactly one audit row exist.
    let (players, accounts, audits): (i64, i64, i64) = sqlx::query_as(
        "SELECT (SELECT count(*) FROM player WHERE player_id = $1),
                (SELECT count(*) FROM accounts WHERE xuid = $1),
                (SELECT count(*) FROM audit_log WHERE identity_id = $2 AND action = 'identity_created')",
    )
    .bind(first.player_id)
    .bind(first.identity_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!((players, accounts, audits), (1, 1, 1));

    let verified_by: String = sqlx::query_scalar("SELECT verified_by FROM identity WHERE identity_id = $1")
        .bind(first.identity_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(verified_by, "ticket");
}

#[tokio::test]
async fn db_identity_is_unique_per_provider_and_external_id() {
    let Some(pool) = test_pool().await else { return };
    let account = synthetic_account(2);
    let ext = account.to_string();

    // Concurrent first contact: one identity, one creator.
    let (a, b) = tokio::join!(
        resolve_identity(&pool, Provider::Steelbound, &ext, VerifiedBy::Ticket, "test-a"),
        resolve_identity(&pool, Provider::Steelbound, &ext, VerifiedBy::Ticket, "test-b"),
    );
    let (a, b) = (a.unwrap(), b.unwrap());
    assert_eq!(a.identity_id, b.identity_id);
    assert_eq!(u8::from(a.created) + u8::from(b.created), 1);

    // The schema refuses a second row for the same pair.
    let dup = sqlx::query(
        "INSERT INTO identity (provider_id, external_id, player_id, verified_by) VALUES ('steelbound', $1, $2, 'admin')",
    )
    .bind(&ext)
    .bind(a.player_id)
    .execute(&pool)
    .await;
    let err = dup.expect_err("duplicate identity accepted");
    assert_eq!(err.as_database_error().and_then(|e| e.code()).as_deref(), Some("23505"));

    // Non-canonical spellings resolve to the same identity.
    let spaced = format!(" {ext} ");
    let c = resolve_identity(&pool, Provider::Steelbound, &spaced, VerifiedBy::Ticket, "test").await.unwrap();
    assert_eq!(c.identity_id, a.identity_id);
}

#[tokio::test]
async fn db_sql_derivation_equals_rust() {
    let Some(pool) = test_pool().await else { return };
    // The formula of sql/account_migration.sql and sql/fork/identity.sql.
    for xuid in [1u64, 0x0009_FFFF_0000_0001, 0x5342_4C01_0000_0001, 0x5342_4C01_FFFF_FFFF, 0xE000_0000_0000_0001] {
        let sql: Uuid = sqlx::query_scalar("SELECT lpad(to_hex($1::bigint), 32, '0')::uuid")
            .bind(xuid as i64)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(sql, player_id_from_xuid(xuid), "xuid {xuid:#x}");
    }
    // The provider registry matches the Rust constants.
    let (rule, prefix, enabled): (String, Option<i64>, bool) = sqlx::query_as(
        "SELECT player_id_rule, pseudo_xuid_prefix, enabled FROM provider WHERE provider_id = 'steelbound'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!((rule.as_str(), prefix, enabled), ("pseudo_xuid", Some(i64::from(STEELBOUND_XUID_PREFIX)), true));
}

#[tokio::test]
async fn db_disabled_and_invalid_are_refused() {
    let Some(pool) = test_pool().await else { return };
    // Insignia is prepared but disabled: no identity is created.
    let r = resolve_identity(&pool, Provider::Insignia, "0009ffff0000beef", VerifiedBy::FirstLogin, "test").await;
    assert!(matches!(r, Err(IdentityError::ProviderDisabled(Provider::Insignia))));
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM identity WHERE provider_id = 'insignia' AND external_id = '0009ffff0000beef'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(n, 0);

    let r = resolve_identity(&pool, Provider::Steelbound, "0", VerifiedBy::Ticket, "test").await;
    assert!(matches!(r, Err(IdentityError::InvalidExternalId { .. })));
}

fn synthetic_pilot(name: &str) -> PilotData {
    let mut data: PilotData = [0; _];
    data[40..40 + name.len()].copy_from_slice(name.as_bytes());
    data
}

fn session(transport: BoxedTransport) -> Client {
    Client {
        codec: Framed::new(transport, command::CampaignCodec),
        session_id: 0,
        xuid: Uuid::nil(),
        xname: String::new(),
        faction: Faction::None,
        language: None,
        banned: false,
    }
}

/// A Steelbound player goes through the core boundary only: resolve, open the
/// session, register a pilot, choose a faction, report stats. The upstream
/// commands store everything under the derived player_id.
#[tokio::test]
async fn db_steelbound_player_through_the_core() {
    let Some(pool) = test_pool().await else { return };
    // The upstream commands use the server's globals.
    let _ = crate::PG_POOL.set(pool.clone());
    let campaign: toml::Table = include_str!("../../loc.toml").parse::<toml::Table>().unwrap()["campaign"]
        .as_table()
        .unwrap()
        .clone();
    let _ = crate::CONFIG.set(crate::CampaignConfig::new(&campaign));

    let core = SbCampaignCore::new(pool.clone());
    let (ours, _theirs) = tokio::io::duplex(64);
    let mut client = session(Box::new(ours));

    let account = synthetic_account(3);
    let resolved = core
        .resolve_identity(Provider::Steelbound, &account.to_string(), VerifiedBy::Ticket, "test")
        .await
        .unwrap();
    assert!(core.open_session(&mut client, &resolved).await.unwrap());
    assert_eq!(client.xuid, derive_player_id(Provider::Steelbound, &account.to_string()).unwrap());

    // No pilot yet.
    match dispatch(&core, &mut client, crate::command::rx::RxCommand::GetUserInfo).await.unwrap() {
        TxCommand::GetUserInfo { status, .. } => assert_eq!(status, 2),
        other => panic!("unexpected {other}"),
    }

    // Register, then load.
    let name = "SynthPilot";
    match core
        .create_pilot(&mut client, name.into(), "SynthTag".into(), Faction::Hsd, synthetic_pilot(name))
        .await
        .unwrap()
    {
        TxCommand::NewUserInfo { status } => assert_eq!(status, 0),
        other => panic!("unexpected {other}"),
    }
    match core.get_pilot(&mut client).await.unwrap() {
        TxCommand::GetUserInfo { status, pilot_name, faction, .. } => {
            assert_eq!(status, 0);
            assert_eq!(pilot_name, name);
            assert!(matches!(faction, Faction::Hsd));
        }
        other => panic!("unexpected {other}"),
    }

    // Save and choose another faction.
    core.update_pilot(&mut client, synthetic_pilot(name)).await.unwrap();
    core.set_faction(&mut client, Faction::Prf).await.unwrap();
    assert!(matches!(client.faction, Faction::Prf));

    // Garage and stats are keyed by the same player_id.
    match core.list_vts(&mut client).await.unwrap() {
        TxCommand::VtOwned { status, .. } => assert_eq!(status, 0),
        other => panic!("unexpected {other}"),
    }
    core.report_user_points(&mut client, 1, 1, 0, [1, 2, 3, 4, 5, 6, 7, 8, 9, 10]).await.unwrap();

    let (faction, xname, stats): (String, Option<String>, i64) = sqlx::query_as(
        "SELECT a.faction::text, a.xname, (SELECT count(*) FROM leaderboard l WHERE l.xuid = a.xuid)
         FROM accounts a WHERE a.xuid = $1",
    )
    .bind(resolved.player_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!((faction.as_str(), xname.as_deref(), stats), ("prf", Some("SynthTag"), 1));

    // A second session of the same identity sees the same player.
    let (ours2, _theirs2) = tokio::io::duplex(64);
    let mut client2 = session(Box::new(ours2));
    let again = core
        .resolve_identity(Provider::Steelbound, &account.to_string(), VerifiedBy::Ticket, "test")
        .await
        .unwrap();
    core.open_session(&mut client2, &again).await.unwrap();
    assert_eq!(client2.xuid, client.xuid);
    assert_eq!(client2.xname, "SynthTag");
    assert!(matches!(client2.faction, Faction::Prf));
}
