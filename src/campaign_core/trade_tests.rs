// SPDX-License-Identifier: AGPL-3.0-only
//! VT trade between two pilots (chat room of the Neutral Zone): TradeToken,
//! then TradeVT from both sides with the same token. Only the issuer and the
//! named partner meet, and only VTs the giving side owns change hands.

use super::tests::{global_pool, globals_runtime, session, synthetic_account, synthetic_pilot, test_core};
use crate::{
    client::{Client, Faction},
    command::{VTEntry, tx::TxCommand},
    core_api::{CampaignCore, Provider, VerifiedBy},
};
use uuid::Uuid;

async fn pilot<C: CampaignCore>(core: &C, test: u32, name: &str, faction: Faction) -> Client {
    let (ours, _theirs) = tokio::io::duplex(64);
    let mut client = session(Box::new(ours));
    let account = synthetic_account(test);
    let resolved = core
        .resolve_identity(Provider::Steelbound, &account.to_string(), VerifiedBy::Ticket, "test")
        .await
        .unwrap();
    assert!(core.open_session(&mut client, &resolved).await.unwrap());
    core.create_pilot(&mut client, name.into(), format!("{name}Tag"), faction, synthetic_pilot(name))
        .await
        .unwrap();
    client
}

/// Owner of a VT (type, serial).
async fn owner(pool: &sqlx::PgPool, vt: (u16, u32)) -> Option<Uuid> {
    sqlx::query_scalar("SELECT xuid FROM garage WHERE \"type\" = $1 AND \"serial\" = $2")
        .bind(vt.0 as i16)
        .bind(vt.1 as i64)
        .fetch_optional(pool)
        .await
        .unwrap()
}

/// The newest VT of a pilot.
async fn newest_vt(pool: &sqlx::PgPool, xuid: Uuid) -> (u16, u32) {
    let (t, s): (i16, i64) =
        sqlx::query_as("SELECT \"type\", \"serial\" FROM garage WHERE xuid = $1 ORDER BY \"serial\" DESC LIMIT 1")
            .bind(xuid)
            .fetch_one(pool)
            .await
            .unwrap();
    (t as u16, s as u32)
}

fn token_of(answer: TxCommand) -> u32 {
    match answer {
        TxCommand::TradeToken { status: 0, token } => token,
        _ => panic!("no token"),
    }
}

#[test]
fn db_trade_swaps_owned_vts_and_refuses_theft() {
    globals_runtime().block_on(trade_swaps_owned_vts_and_refuses_theft());
}

async fn trade_swaps_owned_vts_and_refuses_theft() {
    let Some(pool) = global_pool().await else { return };
    let core = test_core(&pool).await;
    let mut a = pilot(&core, 14, "TradeHsd", Faction::Hsd).await;
    let mut b = pilot(&core, 15, "TradePrf", Faction::Prf).await;
    let mut c = pilot(&core, 16, "TradeThief", Faction::Prf).await;

    core.add_vts(&mut a, vec![0]).await.unwrap(); // Vitzh
    let prf_type: i16 = sqlx::query_scalar("SELECT \"type\" FROM shop WHERE faction = 'prf' ORDER BY \"type\" LIMIT 1")
        .fetch_one(&pool)
        .await
        .unwrap();
    core.add_vts(&mut b, vec![prf_type as u16]).await.unwrap();
    let vt_a = newest_vt(&pool, a.xuid).await;
    let vt_b = newest_vt(&pool, b.xuid).await;

    // A regular trade: A gives its Vitzh for B's VT.
    let token = token_of(core.create_trade_token(&mut a, "TradePrf".into()).await.unwrap());
    let entry = |vt: (u16, u32)| vec![VTEntry::new(vt.0, vt.1)];
    let (ra, rb) = tokio::join!(
        core.trade_vts(&mut a, token, "TradePrf".into(), entry(vt_b), entry(vt_a)),
        core.trade_vts(&mut b, token, "TradeHsd".into(), entry(vt_a), entry(vt_b)),
    );
    assert!(matches!(ra, Ok(TxCommand::TradeVT { status: 0, .. })), "A refused");
    assert!(matches!(rb, Ok(TxCommand::TradeVT { status: 0, .. })), "B refused");
    assert_eq!(owner(&pool, vt_a).await, Some(b.xuid));
    assert_eq!(owner(&pool, vt_b).await, Some(a.xuid));
    // The token is spent for VTs.
    let again = core.trade_vts(&mut a, token, "TradePrf".into(), vec![], vec![]).await;
    assert!(again.is_err(), "token used twice");

    // Theft with an accomplice: B now owns vt_a. A asks for a token with C (another identity), and both
    // report that C gives vt_a to A - but C does not own it.
    let token = token_of(core.create_trade_token(&mut a, "TradeThief".into()).await.unwrap());
    let (ra, rc) = tokio::join!(
        core.trade_vts(&mut a, token, "TradeThief".into(), entry(vt_a), vec![]),
        core.trade_vts(&mut c, token, "TradeHsd".into(), vec![], entry(vt_a)),
    );
    assert!(ra.is_err() && rc.is_err(), "theft went through");
    assert_eq!(owner(&pool, vt_a).await, Some(b.xuid), "rolled back");

    // A one-sided claim (upstream accepted it): A claims B's VT, B agrees to nothing in return - swap mismatch.
    let token = token_of(core.create_trade_token(&mut a, "TradePrf".into()).await.unwrap());
    let (ra, rb) = tokio::join!(
        core.trade_vts(&mut a, token, "TradePrf".into(), entry(vt_a), vec![]),
        core.trade_vts(&mut b, token, "TradeHsd".into(), vec![], vec![]),
    );
    assert!(ra.is_err() && rb.is_err());
    assert_eq!(owner(&pool, vt_a).await, Some(b.xuid));

    // Nobody can trade with oneself, and a stranger cannot use someone else's token.
    assert!(matches!(
        core.create_trade_token(&mut a, "TradeHsd".into()).await.unwrap(),
        TxCommand::TradeToken { status: 1, .. }
    ));
    let token = token_of(core.create_trade_token(&mut a, "TradePrf".into()).await.unwrap());
    assert!(core.trade_vts(&mut c, token, "TradeHsd".into(), vec![], vec![]).await.is_err());
}
