// SPDX-License-Identifier: AGPL-3.0-only
//! Changing sides at any time (campaign_core::faction_swap), against the test
//! database: garages per side, the answer to SetUserFaction, one statistic per
//! player and battle points for the side they were fought for.

use super::tests::{global_pool, globals_runtime, session, synthetic_account, synthetic_pilot, test_core};
use crate::{
    client::{Client, Faction},
    command::{self, RankCategory, tx::TxCommand},
    core_api::{CampaignCore, Provider, VerifiedBy},
};
use bytes::BytesMut;
use tokio_util::codec::Encoder;

use super::SbCampaignCore;

/// VT types of a VtOwned/VtAdd answer, decoded like the game does
/// (`u8 status, u16 count`, then `u16 type, u32 serial` per VT).
fn vt_types(answer: TxCommand) -> Vec<u16> {
    let mut wire = BytesMut::new();
    command::CampaignCodec.encode(answer, &mut wire).unwrap();
    assert_eq!(wire[4], 0, "status");
    let count = u16::from_le_bytes([wire[5], wire[6]]) as usize;
    let mut types: Vec<u16> = (0..count).map(|i| u16::from_le_bytes([wire[7 + 6 * i], wire[8 + 6 * i]])).collect();
    types.sort_unstable();
    types
}

async fn pilot(core: &SbCampaignCore, account: u32, name: &str, faction: Faction) -> Client {
    let mut client = reconnect(core, account).await;
    core.create_pilot(&mut client, name.into(), format!("{name}Tag"), faction, synthetic_pilot(name))
        .await
        .unwrap();
    client
}

/// A (new) connection of a player, as the game opens one per transaction:
/// the faction comes from the database again.
async fn reconnect(core: &SbCampaignCore, account: u32) -> Client {
    let (ours, _theirs) = tokio::io::duplex(64);
    let mut client = session(Box::new(ours));
    let resolved = core
        .resolve_identity(Provider::Steelbound, &account.to_string(), VerifiedBy::Ticket, "test")
        .await
        .unwrap();
    assert!(core.open_session(&mut client, &resolved).await.unwrap());
    client
}

#[test]
fn db_swap_keeps_a_garage_per_side() {
    globals_runtime().block_on(swap_keeps_a_garage_per_side());
}

async fn swap_keeps_a_garage_per_side() {
    let Some(pool) = global_pool().await else { return };
    let core = test_core(&pool).await;
    let account = synthetic_account(14);
    let mut a = pilot(&core, account, "SwapPilot", Faction::Prf).await;

    // PRF: buy a Decider (type 7, unlimited in turn 1 and later).
    assert_eq!(vt_types(core.list_vts(&mut a).await.unwrap()), Vec::<u16>::new());
    assert_eq!(vt_types(core.add_vts(&mut a, vec![7]).await.unwrap()), vec![7]);

    // Transfer to HSD at once (no cooldown): status 0, the game's success path.
    let answer = core.set_faction(&mut a, Faction::Hsd).await.unwrap();
    assert!(matches!(answer, TxCommand::SetUserFaction { status: 0 }), "{answer}");
    let mut a = reconnect(&core, account).await;
    assert!(matches!(a.faction, Faction::Hsd));
    // The HSD garage is empty, the PRF Decider is not in it.
    assert_eq!(vt_types(core.list_vts(&mut a).await.unwrap()), Vec::<u16>::new());
    // HSD buys a Vitzh (type 0).
    assert_eq!(vt_types(core.add_vts(&mut a, vec![0]).await.unwrap()), vec![0]);
    assert_eq!(vt_types(core.list_vts(&mut a).await.unwrap()), vec![0]);

    // Back to PRF: the PRF garage is there again, the Vitzh stays with HSD.
    let answer = core.set_faction(&mut a, Faction::Prf).await.unwrap();
    assert!(matches!(answer, TxCommand::SetUserFaction { status: 0 }), "{answer}");
    let mut a = reconnect(&core, account).await;
    assert_eq!(vt_types(core.list_vts(&mut a).await.unwrap()), vec![7]);
    // And once more to HSD: its Vitzh is still there.
    core.set_faction(&mut a, Faction::Hsd).await.unwrap();
    let mut a = reconnect(&core, account).await;
    assert_eq!(vt_types(core.list_vts(&mut a).await.unwrap()), vec![0]);

    // Rows in the garage table: one per side, nothing lost or moved.
    let rows: Vec<(i16, String)> = sqlx::query_as(
        "SELECT \"type\", faction::text FROM garage WHERE xuid = $1 ORDER BY \"type\"",
    )
    .bind(a.xuid)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(rows, vec![(0, "hsd".to_owned()), (7, "prf".to_owned())]);
}

#[test]
fn db_one_statistic_per_player_over_all_sides() {
    globals_runtime().block_on(one_statistic_per_player_over_all_sides());
}

/// stats of one mission with `kills` in stat3 (KillCount)
fn kills(kills: u32) -> [u32; 10] {
    let mut p = [0u32; 10];
    p[2] = kills;
    p
}

async fn one_statistic_per_player_over_all_sides() {
    let Some(pool) = global_pool().await else { return };
    let core = test_core(&pool).await;
    // A round of its own, so other tests' leaderboard rows do not count.
    let round: u16 = 30_000 + (synthetic_account(0) & 0x07FF) as u16; // smallint
    let turn: u16 = 1;
    for sql in ["DELETE FROM leaderboard WHERE round = $1", "DELETE FROM wartide WHERE round = $1"] {
        sqlx::query(sql)
            .bind(round as i16)
            .execute(&pool)
            .await
            .unwrap();
    }

    let account_a = synthetic_account(15);
    let mut a = pilot(&core, account_a, "Swapper", Faction::Hsd).await;
    let account_b = account_a ^ 0x0080_0000;
    let mut b = pilot(&core, account_b, "Stayer", Faction::Prf).await;

    // A fights for HSD (3 kills), B for PRF (5 kills).
    core.report_user_points(&mut a, round, turn, 1, kills(3)).await.unwrap();
    core.report_mission_points(&mut a, round, turn, 1, 10, Faction::Hsd).await.unwrap();
    core.report_user_points(&mut b, round, turn, 1, kills(5)).await.unwrap();

    // A changes to PRF and fights there (4 kills): the battle counts for PRF.
    core.set_faction(&mut a, Faction::Prf).await.unwrap();
    let mut a = reconnect(&core, account_a).await;
    core.report_user_points(&mut a, round, turn, 1, kills(4)).await.unwrap();
    core.report_mission_points(&mut a, round, turn, 1, 20, Faction::Prf).await.unwrap();

    let rows: Vec<(String, i32)> = sqlx::query_as(
        "SELECT faction::text, stat3 FROM leaderboard WHERE xuid = $1 AND round = $2 ORDER BY faction",
    )
    .bind(a.xuid)
    .bind(round as i16)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(rows, vec![("hsd".to_owned(), 3), ("prf".to_owned(), 4)], "points stay with the side");
    let (hsd, prf): (i64, i64) =
        sqlx::query_as("SELECT hsd::int8, prf::int8 FROM wartide WHERE round = $1 AND \"map\" = 1")
            .bind(round as i16)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!((hsd, prf), (10, 20), "war tide per side");

    // Global: A has 3 + 4 = 7 kills, one entry, ahead of B (5).
    let global = core.rank_count_global(&mut a, round, turn, RankCategory::KillCount).await.unwrap();
    assert!(matches!(global, TxCommand::RankCountGlobal { place: 1, count: 2, .. }), "{global}");
    // PRF ranking: both pilots of PRF, A with all 7 kills, so A is first.
    let prf = core.rank_count_faction(&mut a, round, turn, RankCategory::KillCount, Faction::Prf).await.unwrap();
    assert!(matches!(prf, TxCommand::RankCountFaction { place: 1, count: 2, .. }), "{prf}");
    let prf_b = core.rank_count_faction(&mut b, round, turn, RankCategory::KillCount, Faction::Prf).await.unwrap();
    assert!(matches!(prf_b, TxCommand::RankCountFaction { place: 2, count: 2, .. }), "{prf_b}");
    let list = core.rank_values(&mut a, round, turn, Faction::Prf, RankCategory::KillCount, 1, 10).await.unwrap();
    assert!(matches!(&list, TxCommand::RankValues { status: 0, rank_list } if rank_list.len() == 2), "{list}");
    // HSD ranking: nobody is in HSD now, A is not listed twice.
    let hsd = core.rank_count_faction(&mut a, round, turn, RankCategory::KillCount, Faction::Hsd).await.unwrap();
    assert!(matches!(hsd, TxCommand::RankCountFaction { place: 0, count: 0, .. }), "{hsd}");
    let list = core.rank_values(&mut a, round, turn, Faction::Hsd, RankCategory::KillCount, 1, 10).await.unwrap();
    assert!(matches!(&list, TxCommand::RankValues { status: 0, rank_list } if rank_list.is_empty()), "{list}");
    // The turn ranking agrees with the round ranking.
    let turn_rank = core.rank_count_faction(&mut a, round, 0, RankCategory::KillCount, Faction::Prf).await.unwrap();
    assert!(matches!(turn_rank, TxCommand::RankCountFaction { place: 1, count: 2, .. }), "{turn_rank}");
}
