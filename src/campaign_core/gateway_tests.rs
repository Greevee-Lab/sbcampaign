// SPDX-License-Identifier: AGPL-3.0-only
//! End-to-end tests of the Steelbound gateway with the real campaign core:
//! TLS listener, ticket, original protocol, audit, results ledger,
//! plausibility, annulment. Synthetic data and TEST keys only.
//!
//! These live in the campaign core (the one place allowed to query the
//! database) so they can check the stored rows directly.

use super::{
    SbCampaignCore,
    tests::{global_pool, globals_runtime, synthetic_account, test_core},
};
use crate::{
    core_api::{Provider, gateway::GatewayCore},
    gateway_steelbound::{
        Gateway,
        limits::Limits,
        prelude, serve,
        test_support::*,
        tls,
    },
};
use rcgen::{BasicConstraints, CertificateParams, CertifiedIssuer, DnType, IsCa, KeyPair};
use rustls::{ClientConfig, RootCertStore, pki_types::ServerName};
use sqlx::{PgPool, postgres::PgPoolOptions};
use std::{net::IpAddr, sync::Arc, time::Duration};
use tokio::{
    io::{AsyncWriteExt, DuplexStream},
    net::{TcpListener, TcpStream},
};
use tokio_rustls::TlsConnector;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

const HSD: u8 = 1;
const PRF: u8 = 2;

fn generous_limits() -> Limits {
    Limits { source_connect_rate: 1000.0, source_connect_burst: 1000.0, ..Limits::default() }
}

fn new_gateway(core: SbCampaignCore, limits: Limits) -> Arc<Gateway<SbCampaignCore>> {
    Arc::new(Gateway::new(Arc::new(core), ticket_config(), limits, Arc::new(|| NOW)))
}

/// A core whose database is unreachable: enough for every path that refuses
/// before the identity is resolved.
fn core_without_database() -> SbCampaignCore {
    let pool = PgPoolOptions::new()
        .acquire_timeout(Duration::from_secs(2))
        .connect_lazy("postgres://nobody@127.0.0.1:9/none")
        .unwrap();
    SbCampaignCore::new(pool.clone(), pool)
}

fn pseudo_xuid(account: u32) -> u64 {
    (0x5342_4C01u64 << 32) | u64::from(account)
}

/// Open an in-memory connection to the gateway (as if after TLS).
fn connect(gw: &Arc<Gateway<SbCampaignCore>>) -> (DuplexStream, tokio::task::JoinHandle<()>) {
    let (client, server) = tokio::io::duplex(8192);
    let admission = gw.admit("192.0.2.10".parse::<IpAddr>().unwrap()).expect("admitted");
    let g = gw.clone();
    let task = tokio::spawn(async move { g.serve(Box::new(server), admission).await });
    (client, task)
}

/// One transaction like the game does it: new connection, prelude, handshake
/// (reporting `reported_xuid`), one command, answer.
async fn transact(
    gw: &Arc<Gateway<SbCampaignCore>>,
    ticket: &[u8],
    reported_xuid: u64,
    cmd: u16,
    payload: &[u8],
) -> (u16, Vec<u8>) {
    let (mut c, task) = connect(gw);
    c.write_all(&prelude::encode(ticket)).await.unwrap();
    assert_eq!(prelude_status(&mut c).await, Some(0), "prelude accepted");
    assert_eq!(handshake(&mut c, reported_xuid).await, 0x3013, "handshake accepted");
    send(&mut c, cmd, payload).await;
    let answer = recv(&mut c).await.expect("answer");
    drop(c);
    task.await.unwrap();
    answer
}

async fn prelude_only(gw: &Arc<Gateway<SbCampaignCore>>, prelude_bytes: &[u8]) -> Option<u8> {
    let (mut c, task) = connect(gw);
    c.write_all(prelude_bytes).await.unwrap();
    let status = prelude_status(&mut c).await;
    drop(c);
    task.await.unwrap();
    status
}

// ---- Without a database ----------------------------------------------------

fn test_pki() -> (String, String, String) {
    let mut p = CertificateParams::new(Vec::<String>::new()).unwrap();
    p.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    p.distinguished_name.push(DnType::CommonName, "gateway test CA");
    let ca = CertifiedIssuer::self_signed(p, KeyPair::generate().unwrap()).unwrap();
    let key = KeyPair::generate().unwrap();
    let leaf = CertificateParams::new(vec!["localhost".into()]).unwrap().signed_by(&key, &ca).unwrap();
    (ca.pem(), leaf.pem(), key.serialize_pem())
}

/// The real listener: TLS, pin, prelude statuses, and a valid ticket that
/// gets as far as the original handshake.
#[tokio::test]
async fn gateway_over_tls() {
    let (ca_pem, cert_pem, key_pem) = test_pki();
    let (server_tls, pin) = tls::server_config(cert_pem.as_bytes(), key_pem.as_bytes(), false).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let gw = new_gateway(core_without_database(), generous_limits());
    let cancel = CancellationToken::new();
    let server = tokio::spawn(serve(listener, server_tls, gw.clone(), cancel.clone()));

    let mut roots = RootCertStore::empty();
    for c in rustls::pki_types::pem::PemObject::pem_slice_iter(ca_pem.as_bytes()) {
        roots.add(c.unwrap()).unwrap();
    }
    let client_cfg = ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_root_certificates(roots)
        .with_no_client_auth();
    let connector = TlsConnector::from(Arc::new(client_cfg));
    let open = || async {
        let tcp = TcpStream::connect(addr).await.unwrap();
        connector.connect(ServerName::try_from("localhost").unwrap(), tcp).await.unwrap()
    };

    // The pin the server logs is the pin of the certificate it presents.
    let s = open().await;
    let presented = s.get_ref().1.peer_certificates().unwrap()[0].clone();
    assert_eq!(tls::spki_pin(presented.as_ref()).unwrap(), pin);
    drop(s);

    let mut expired = TicketSpec::valid(77);
    expired.issued_at = NOW - 3000;
    expired.expires_at = NOW - 1000;
    let mut forged = TicketSpec::valid(77);
    forged.seed = [0x99; 32];
    let cases: Vec<(&str, Vec<u8>, u8)> = vec![
        ("missing", prelude::encode(&[]), 1),
        ("expired", prelude::encode(&expired.sign()), 1),
        ("forged", prelude::encode(&forged.sign()), 1),
        ("garbage", prelude::encode(&[0xAB; 120]), 1),
        ("not a prelude", vec![0, 0, 0x10, 0x20, 0, 0, 0, 0], 3),
    ];
    for (name, bytes, want) in cases {
        let mut s = open().await;
        s.write_all(&bytes).await.unwrap();
        assert_eq!(prelude_status(&mut s).await, Some(want), "{name}");
        assert!(recv(&mut s).await.is_none(), "{name}: connection closed after refusal");
    }

    // Valid ticket: accepted, the original protocol starts. Without a
    // database the identity cannot be resolved, so the handshake ends in
    // Reject - nothing was trusted from the game.
    let mut s = open().await;
    s.write_all(&prelude::encode(&TicketSpec::valid(77).sign())).await.unwrap();
    assert_eq!(prelude_status(&mut s).await, Some(0));
    assert_eq!(handshake(&mut s, 0x0009_FFFF_0000_0077).await, 0x3014);

    assert_eq!(gw.counters.refused.load(std::sync::atomic::Ordering::Relaxed), 4);
    cancel.cancel();
    server.await.unwrap();
}

#[tokio::test]
async fn identity_connection_rate_is_limited() {
    let gw = new_gateway(
        core_without_database(),
        Limits { identity_connect_rate: 0.001, identity_connect_burst: 2.0, ..generous_limits() },
    );
    let ticket = prelude::encode(&TicketSpec::valid(4711).sign());
    assert_eq!(prelude_only(&gw, &ticket).await, Some(0));
    assert_eq!(prelude_only(&gw, &ticket).await, Some(0));
    assert_eq!(prelude_only(&gw, &ticket).await, Some(2), "third connection of the identity: busy");
    let other = prelude::encode(&TicketSpec::valid(4712).sign());
    assert_eq!(prelude_only(&gw, &other).await, Some(0), "other identity unaffected");
}

// ---- With the database -----------------------------------------------------

async fn player_of(pool: &PgPool, account: u32) -> Option<Uuid> {
    sqlx::query_scalar("SELECT player_id FROM core.identity WHERE provider_id = 'steelbound' AND external_id = $1")
        .bind(account.to_string())
        .fetch_optional(pool)
        .await
        .unwrap()
}

async fn audit_outcomes(pool: &PgPool, player: Uuid) -> Vec<(String, String)> {
    sqlx::query_as(
        "SELECT method, outcome FROM audit_log
         WHERE player_id = $1 AND source = 'gateway-steelbound' AND action = 'game_write' ORDER BY audit_id",
    )
    .bind(player)
    .fetch_all(pool)
    .await
    .unwrap()
}

fn pairs(v: &[(&str, &str)]) -> Vec<(String, String)> {
    v.iter().map(|(a, b)| (a.to_string(), b.to_string())).collect()
}

/// Register, load and save a pilot and choose a faction through the
/// gateway; the reported qwUserID is ignored, the registered name comes from
/// the ticket, every write has an audit row.
#[test]
fn db_gateway_pilot_session() {
    globals_runtime().block_on(async {
        let Some(pool) = global_pool().await else { return };
        let gw = new_gateway(test_core(&pool).await, generous_limits());
        let a = synthetic_account(10);
        let b = synthetic_account(11);
        let ticket_a = TicketSpec { display_name: "Gw Pilot".into(), ..TicketSpec::valid(a) }.sign();
        let ticket_b = TicketSpec::valid(b).sign();

        // Ticket of A, but the game claims to be B.
        let (cmd, p) = transact(&gw, &ticket_a, pseudo_xuid(b), 0x1021, &[]).await;
        assert_eq!((cmd, p[0]), (0x2021, 2), "A has no pilot yet");
        assert!(player_of(&pool, b).await.is_none(), "nothing was created for the reported XUID");
        let player_a = player_of(&pool, a).await.expect("identity of the ticket");

        let (cmd, p) = transact(&gw, &ticket_a, pseudo_xuid(b), 0x5022, &new_user_info("GwPilot", "Spoofed", HSD)).await;
        assert_eq!((cmd, p[0]), (0x6022, 0), "pilot registered");
        let (cmd, p) = transact(&gw, &ticket_a, 0, 0x1021, &[]).await;
        assert_eq!((cmd, p[0]), (0x2021, 0), "pilot loaded");
        assert_eq!(&p[1..8], b"GwPilot");
        assert_eq!(p[17], HSD);
        let (cmd, p) = transact(&gw, &ticket_a, 0, 0x5024, &set_user_info("GwPilot")).await;
        assert_eq!((cmd, p[0]), (0x6024, 0), "pilot saved");
        let (cmd, p) = transact(&gw, &ticket_a, 0, 0x5025, &[PRF]).await;
        // Status 0: the game stays in the campaign ([campaign] transfer_in_place, default true;
        // upstream answers 1, which signs the player out so that the garage reloads).
        assert_eq!((cmd, p[0]), (0x6025, 0), "faction chosen (status 0, transfer in place)");

        // B (claiming to be A) sees its own, empty account.
        let (cmd, p) = transact(&gw, &ticket_b, pseudo_xuid(a), 0x1021, &[]).await;
        assert_eq!((cmd, p[0]), (0x2021, 2));

        let (xname, faction): (Option<String>, String) =
            sqlx::query_as("SELECT xname, faction::text FROM accounts WHERE xuid = $1")
                .bind(player_a)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!((xname.as_deref(), faction.as_str()), (Some("Gw Pilot"), "prf"));
        assert_eq!(
            audit_outcomes(&pool, player_a).await,
            pairs(&[("NewUserInfo", "ok"), ("SetUserInfo", "ok"), ("SetUserFaction", "ok")])
        );
        // Audit rows carry no contents.
        let detail: Option<String> = sqlx::query_scalar(
            "SELECT string_agg(coalesce(detail, ''), '') FROM audit_log WHERE player_id = $1 AND action = 'game_write'",
        )
        .bind(player_a)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(detail.as_deref(), Some(""));
    });
}

/// Results: plausible ones are applied and recorded, implausible ones are
/// answered normally but not applied; annulment takes the applied ones back
/// and suspends the identity, which the gateway then refuses.
#[test]
fn db_gateway_results_plausibility_annul() {
    globals_runtime().block_on(async {
        let Some(pool) = global_pool().await else { return };
        let core = test_core(&pool).await;
        let gw = new_gateway(core.clone(), generous_limits());
        let c = synthetic_account(12);
        let ticket = TicketSpec::valid(c).sign();

        let (_, p) = transact(&gw, &ticket, 0, 0x5022, &new_user_info("GwScore", "x", HSD)).await;
        assert_eq!(p[0], 0);
        let player = player_of(&pool, c).await.unwrap();
        let (cmd, p) = transact(&gw, &ticket, 0, 0x1030, &[]).await;
        assert_eq!(cmd, 0x2030);
        let (round, turn) = (u16::from_le_bytes([p[0], p[1]]), u16::from_le_bytes([p[2], p[3]]));
        let map = 1u8;
        let map_id = i16::from(map) + (turn as i16 - 1) * 3;
        let wartide = || async {
            sqlx::query_scalar::<_, i32>("SELECT hsd FROM wartide WHERE round = $1 AND \"map\" = $2")
                .bind(round as i16)
                .bind(map_id)
                .fetch_optional(&pool)
                .await
                .unwrap()
                .unwrap_or(0)
        };
        let stat1 = || async {
            sqlx::query_scalar::<_, i32>("SELECT coalesce(sum(stat1), 0)::int FROM leaderboard WHERE xuid = $1")
                .bind(player)
                .fetch_one(&pool)
                .await
                .unwrap()
        };
        let tide_before = wartide().await;

        let ok = |cmd: u16| (cmd + 0x1000, 0u8);
        let (cmd, p) = transact(&gw, &ticket, 0, 0x5026, &add_user_point(round, turn, map, [5; 10])).await;
        assert_eq!((cmd, p[0]), ok(0x5026));
        let (cmd, p) = transact(&gw, &ticket, 0, 0x5027, &add_mission_point(round, turn, map, 100, HSD)).await;
        assert_eq!((cmd, p[0]), ok(0x5027));
        // Implausible: answered like a success, not applied.
        let (cmd, p) = transact(&gw, &ticket, 0, 0x5026, &add_user_point(round, turn, map, [1_000_000; 10])).await;
        assert_eq!((cmd, p[0]), ok(0x5026));
        let (cmd, p) = transact(&gw, &ticket, 0, 0x5027, &add_mission_point(round + 1, turn, map, 100, HSD)).await;
        assert_eq!((cmd, p[0]), ok(0x5027));

        assert_eq!(stat1().await, 5);
        assert_eq!(wartide().await, tide_before + 100);
        let ledger: i64 = sqlx::query_scalar("SELECT count(*) FROM gateway_result WHERE player_id = $1")
            .bind(player)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(ledger, 2);
        assert_eq!(
            audit_outcomes(&pool, player).await,
            pairs(&[
                ("NewUserInfo", "ok"),
                ("AddUserPoint", "ok"),
                ("AddMissionPoint", "ok"),
                ("AddUserPoint", "implausible"),
                ("AddMissionPoint", "implausible"),
            ])
        );

        // Dry run counts only.
        let dry = core.annul_identity_results(Provider::Steelbound, &c.to_string(), true, true, "test").await;
        let dry = dry.unwrap().unwrap();
        assert_eq!((dry.user_reports, dry.mission_reports, dry.suspended), (1, 1, false));
        assert_eq!(stat1().await, 5);

        let done = core.annul_identity_results(Provider::Steelbound, &c.to_string(), true, false, "test").await;
        let done = done.unwrap().unwrap();
        assert_eq!((done.user_reports, done.mission_reports, done.suspended), (1, 1, true));
        assert_eq!(done.player_id, player);
        assert_eq!(stat1().await, 0);
        assert_eq!(wartide().await, tide_before);
        let open: i64 = sqlx::query_scalar("SELECT count(*) FROM gateway_result WHERE player_id = $1 AND annulled_at IS NULL")
            .bind(player)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(open, 0);
        let annul_rows: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM audit_log WHERE player_id = $1 AND action = 'results_annulled'",
        )
        .bind(player)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(annul_rows, 1);
        // The suspension is a core change: one row in the core audit.
        let suspended_rows: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM core.audit_log WHERE player_id = $1 AND action = 'identity_suspended'",
        )
        .bind(player)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(suspended_rows, 1);

        // Running it again takes nothing back twice.
        let again = core.annul_identity_results(Provider::Steelbound, &c.to_string(), false, false, "test").await;
        assert_eq!(again.unwrap().unwrap().user_reports, 0);

        // Suspended: the ticket is still valid, the gateway refuses the session.
        let (mut s, task) = connect(&gw);
        s.write_all(&prelude::encode(&ticket)).await.unwrap();
        assert_eq!(prelude_status(&mut s).await, Some(0));
        assert_eq!(handshake(&mut s, 0).await, 0x3014);
        drop(s);
        task.await.unwrap();

        assert!(core
            .annul_identity_results(Provider::Steelbound, "4000000001", false, true, "test")
            .await
            .unwrap()
            .is_none());
    });
}

/// W531: the VT of a battle. A sortie (0x1047) is linked to the pilot's next
/// result (0x5026): the first owned VT of the sortie names the battle; no
/// sortie, an old sortie, a foreign VT or another side give no VT; a sortie
/// belongs to one result; of two open sorties the newer one counts. Every
/// transaction is its own connection, so a reconnect changes nothing.
#[test]
fn db_gateway_battle_vt() {
    globals_runtime().block_on(async {
        let Some(pool) = global_pool().await else { return };
        let gw = new_gateway(test_core(&pool).await, generous_limits());
        let acc = synthetic_account(13);
        let ticket = TicketSpec::valid(acc).sign();
        let (_, p) = transact(&gw, &ticket, 0, 0x5022, &new_user_info("GwVt", "x", HSD)).await;
        assert_eq!(p[0], 0);
        let player = player_of(&pool, acc).await.unwrap();
        let other = Uuid::from_u128(0x5331_0000_0000_0000_0000_0000_0000_0013);
        // Garage: Vitzh (0) and Vortex (2) of the pilot, a Scare Face (8) of someone else.
        for (t, s, owner) in [(0i16, 953_101i64, player), (2, 953_102, player), (8, 953_103, other)] {
            sqlx::query("INSERT INTO garage (\"type\", \"serial\", xuid, faction) VALUES ($1, $2, $3, 'hsd')")
                .bind(t)
                .bind(s)
                .bind(owner)
                .execute(&pool)
                .await
                .unwrap();
        }
        let (_, p) = transact(&gw, &ticket, 0, 0x1030, &[]).await;
        let (round, turn) = (u16::from_le_bytes([p[0], p[1]]), u16::from_le_bytes([p[2], p[3]]));
        let result = |map: u8| {
            let gw = gw.clone();
            let ticket = ticket.clone();
            async move {
                let (cmd, p) = transact(&gw, &ticket, 0, 0x5026, &add_user_point(round, turn, map, [1; 10])).await;
                assert_eq!((cmd, p[0]), (0x6026, 0));
            }
        };
        let sortie = |vts: Vec<(u16, u32)>| {
            let gw = gw.clone();
            let ticket = ticket.clone();
            async move {
                let (cmd, p) = transact(&gw, &ticket, 0, 0x1047, &vt_entries(&vts)).await;
                assert_eq!((cmd, p[0]), (0x2047, 0));
            }
        };
        // VT per result of this player, oldest first (None = no VT).
        let vts = || async {
            sqlx::query_scalar::<_, Option<i16>>(
                "SELECT b.vt_type FROM gateway_result g LEFT JOIN battle_vt b ON b.result_id = g.result_id
                 WHERE g.player_id = $1 AND g.kind = 'user_points' ORDER BY g.result_id",
            )
            .bind(player)
            .fetch_all(&pool)
            .await
            .unwrap()
        };
        let open_sorties = || async {
            sqlx::query_scalar::<_, i64>("SELECT count(*) FROM battle_vt WHERE player_id = $1 AND result_id IS NULL")
                .bind(player)
                .fetch_one(&pool)
                .await
                .unwrap()
        };

        // 1. No sortie: the result has no VT.
        result(1).await;
        assert_eq!(vts().await, vec![None]);

        // 2. Sortie with two VTs: the first one (Vortex) names the battle.
        sortie(vec![(2, 953_102), (0, 953_101)]).await;
        let (vt_type, types): (i16, Vec<i16>) =
            sqlx::query_as("SELECT vt_type, vt_types FROM battle_vt WHERE player_id = $1 AND result_id IS NULL")
                .bind(player)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!((vt_type, types), (2, vec![2, 0]));
        let sortied: i64 = sqlx::query_scalar("SELECT count(*) FROM garage WHERE xuid = $1 AND sortied")
            .bind(player)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(sortied, 2, "the sortie itself is unchanged");
        result(2).await;
        assert_eq!(vts().await, vec![None, Some(2)]);

        // 3. A second result without a new sortie: the sortie is used up.
        result(2).await;
        assert_eq!(vts().await, vec![None, Some(2), None]);

        // 4. A VT the pilot does not own is ignored; a list of only such VTs is no sortie.
        sortie(vec![(8, 953_103)]).await;
        assert_eq!(open_sorties().await, 0);
        sortie(vec![(8, 953_103), (0, 953_101)]).await;
        let t: Vec<i16> = sqlx::query_scalar("SELECT vt_types FROM battle_vt WHERE player_id = $1 AND result_id IS NULL")
            .bind(player)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(t, vec![0], "only the own Vitzh");

        // 5. That sortie is too old (aborted mission): not linked.
        sqlx::query("UPDATE battle_vt SET sortied_at = now() - INTERVAL '121 minutes' WHERE player_id = $1 AND result_id IS NULL")
            .bind(player)
            .execute(&pool)
            .await
            .unwrap();
        result(3).await;
        assert_eq!(vts().await, vec![None, Some(2), None, None]);

        // 6. Two open sorties (the first mission aborted): the newer one counts.
        sortie(vec![(0, 953_101)]).await;
        sortie(vec![(2, 953_102)]).await;
        result(3).await;
        assert_eq!(vts().await, vec![None, Some(2), None, None, Some(2)]);

        // 7. Sortie for HSD, then a transfer to PRF: the PRF result gets no HSD VT.
        sortie(vec![(0, 953_101)]).await;
        let (_, p) = transact(&gw, &ticket, 0, 0x5025, &[PRF]).await;
        assert_eq!(p[0], 0);
        result(1).await;
        assert_eq!(vts().await, vec![None, Some(2), None, None, Some(2), None]);
        // Two old open sorties (step 5 and 6) and the HSD one stay unlinked.
        assert_eq!(open_sorties().await, 3);

        // Nothing but types, sides and times is stored.
        let cols: Vec<String> = sqlx::query_scalar(
            "SELECT column_name::text FROM information_schema.columns WHERE table_name = 'battle_vt' ORDER BY ordinal_position",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(
            cols,
            ["battle_vt_id", "sortied_at", "identity_id", "player_id", "faction", "vt_type", "vt_types", "result_id", "linked_at"]
        );
    });
}
