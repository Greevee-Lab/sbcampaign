// SPDX-License-Identifier: AGPL-3.0-only
//! Reinforcement shop of a new pilot (sql/fork/shop_catalog.sql).
//!
//! The game decodes the ShopList answer as `u8 status, u16 count` and then
//! `count` entries of 9 bytes (`u16 type, u32 limit, u16 cost, u8 pad`). It
//! only offers a VT whose limit is not zero, and it needs at least one VT in
//! the shop or in the garage, otherwise MISSION SELECT stays disabled.

use super::{
    tests::{global_pool, globals_runtime, session, synthetic_account, synthetic_pilot, test_core},
};
use crate::{
    client::Faction,
    command::{self, tx::TxCommand},
    core_api::{CampaignCore, Provider, VerifiedBy},
};
use bytes::BytesMut;
use tokio_util::codec::Encoder;

/// (type, limit, cost) of every entry, decoded like the game does.
fn shop_entries(answer: TxCommand) -> Vec<(u16, u32, u16)> {
    assert!(matches!(answer, TxCommand::ShopList { status: 0, .. }), "unexpected {answer}");
    let mut wire = BytesMut::new();
    command::CampaignCodec.encode(answer, &mut wire).unwrap();
    let len = u16::from_le_bytes([wire[0], wire[1]]) as usize;
    assert_eq!(wire.len(), len + 4, "frame length");
    assert_eq!(u16::from_le_bytes([wire[2], wire[3]]), 0x2041);
    assert_eq!(wire[4], 0, "status");
    let count = u16::from_le_bytes([wire[5], wire[6]]) as usize;
    assert_eq!(len, 3 + 9 * count, "9 bytes per entry");
    (0..count)
        .map(|i| {
            let e = &wire[7 + 9 * i..16 + 9 * i];
            (
                u16::from_le_bytes([e[0], e[1]]),
                u32::from_le_bytes([e[2], e[3], e[4], e[5]]),
                u16::from_le_bytes([e[6], e[7]]),
            )
        })
        .collect()
}

/// Number of VTs in a VtOwned/VtAdd answer.
fn vt_count(answer: TxCommand) -> usize {
    let mut wire = BytesMut::new();
    command::CampaignCodec.encode(answer, &mut wire).unwrap();
    assert_eq!(wire[4], 0, "status");
    u16::from_le_bytes([wire[5], wire[6]]) as usize
}

#[test]
fn db_new_pilot_can_buy_a_starting_vt() {
    globals_runtime().block_on(new_pilot_can_buy_a_starting_vt());
}

async fn new_pilot_can_buy_a_starting_vt() {
    let Some(pool) = global_pool().await else { return };
    let core = test_core(&pool).await;

    // The catalog is complete: 31 VT types, each with a known faction.
    let (types, unknown): (i64, i64) =
        sqlx::query_as("SELECT count(*), count(*) FILTER (WHERE faction = 'none') FROM shop")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(types >= 31, "shop catalog missing ({types} rows)");
    assert_eq!(unknown, 0);
    let turn: i16 = sqlx::query_scalar("SELECT turn FROM settings ORDER BY round DESC, turn DESC LIMIT 1")
        .fetch_one(&pool)
        .await
        .unwrap();

    let (ours, _theirs) = tokio::io::duplex(64);
    let mut client = session(Box::new(ours));
    let account = synthetic_account(13);
    let resolved = core
        .resolve_identity(Provider::Steelbound, &account.to_string(), VerifiedBy::Ticket, "test")
        .await
        .unwrap();
    assert!(core.open_session(&mut client, &resolved).await.unwrap());
    core.create_pilot(&mut client, "ShopPilot".into(), "ShopTag".into(), Faction::Hsd, synthetic_pilot("ShopPilot"))
        .await
        .unwrap();

    // A new pilot owns nothing ...
    assert_eq!(vt_count(core.list_vts(&mut client).await.unwrap()), 0);

    // ... but the shop of turn 1 offers the HSD starter VTs without a limit.
    let hsd = shop_entries(core.list_shop(&mut client).await.unwrap());
    if turn == 1 {
        assert!(hsd.contains(&(0, u32::MAX, 12)), "Vitzh missing: {hsd:?}");
        assert!(hsd.contains(&(2, u32::MAX, 10)), "Vortex missing: {hsd:?}");
    }
    assert!(hsd.iter().any(|&(_, limit, _)| limit != 0), "no VT for sale: {hsd:?}");
    let hsd_types: Vec<i16> = sqlx::query_scalar("SELECT \"type\" FROM shop WHERE faction = 'hsd'")
        .fetch_all(&pool)
        .await
        .unwrap();
    assert!(hsd.iter().all(|&(t, _, _)| hsd_types.contains(&(t as i16))), "foreign VT: {hsd:?}");

    // Buying one puts it into the garage.
    let bought = core.add_vts(&mut client, vec![0]).await.unwrap();
    assert_eq!(vt_count(bought), 1);
    assert_eq!(vt_count(core.list_vts(&mut client).await.unwrap()), 1);
    // An unlimited VT stays unlimited after a purchase (PostgreSQL GREATEST
    // ignores NULL, so the stock must not be computed with it).
    let after = shop_entries(core.list_shop(&mut client).await.unwrap());
    assert!(after.iter().any(|&(t, limit, _)| t == 0 && limit == u32::MAX), "Vitzh gone after a purchase: {after:?}");

    // PRF sees its own catalog (Decider without a limit, Falchion limited).
    core.set_faction(&mut client, Faction::Prf).await.unwrap();
    let prf = shop_entries(core.list_shop(&mut client).await.unwrap());
    if turn == 1 {
        assert!(prf.contains(&(7, u32::MAX, 25)), "Decider missing: {prf:?}");
        assert!(prf.iter().any(|&(t, limit, _)| t == 5 && limit > 0 && limit <= 25), "Falchion: {prf:?}");
    }
    assert!(prf.iter().all(|&(t, _, _)| t != 0 && t != 2), "HSD VT in the PRF shop: {prf:?}");
    // The HSD VT stays in the HSD garage.
    assert_eq!(vt_count(core.list_vts(&mut client).await.unwrap()), 0);
    // A limited VT loses exactly the bought number.
    if let Some(&(_, before, _)) = prf.iter().find(|&&(t, _, _)| t == 5) {
        assert_eq!(vt_count(core.add_vts(&mut client, vec![5, 5]).await.unwrap()), 2);
        let prf2 = shop_entries(core.list_shop(&mut client).await.unwrap());
        let left = prf2.iter().find(|&&(t, _, _)| t == 5).map(|&(_, l, _)| l).unwrap_or(0);
        assert_eq!(left, before - 2, "Falchion stock");
        assert_eq!(vt_count(core.list_vts(&mut client).await.unwrap()), 2);
    }
}
