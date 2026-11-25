// SPDX-License-Identifier: AGPL-3.0-only
//! Gateway tests without a database: configuration, ticket gate, admission
//! limits, plausibility. The end-to-end tests (TLS, database, audit,
//! annulment) are in `campaign_core::gateway_tests`.

use super::{
    Gateway, GatewayConfig,
    limits::{self, KeyedLimiter, Limits, Plausibility, SourceKey},
    session::campaign_xname,
    test_support::*,
};
use crate::{client::Faction, command::rx::RxCommand, ticket::TicketError};
use std::{net::IpAddr, sync::Arc, time::Duration};

fn gateway(limits: Limits) -> Gateway<()> {
    Gateway::new(Arc::new(()), ticket_config(), limits, Arc::new(|| NOW))
}

fn root(extra: &str) -> toml::Table {
    format!(
        "[network]\nbind_port = 23458\nbind_ip = \"127.0.0.1\"\n\n[gateway_steelbound]\nbind = \"0.0.0.0:47100\"\n\
         cert = \"/etc/steelcore/gateway/server.crt\"\nkey = \"/etc/steelcore/gateway/server.key\"\n{extra}"
    )
    .parse()
    .unwrap()
}

// ---- Configuration -------------------------------------------------------

#[test]
fn config_defaults_and_overrides() {
    let c = GatewayConfig::from_root(&root("")).unwrap().unwrap();
    assert_eq!(c.bind.port(), 47100);
    assert!(!c.tls13_only);
    assert_eq!(c.limits, Limits::default());

    let c = GatewayConfig::from_root(&root(
        "tls_min = \"1.3\"\n[gateway_steelbound.limits]\nmax_connections_per_source = 3\nresults_per_hour = 120\n\
         max_mission_points = 500\nturn_grace = 0\nsource_connect_rate = 0.5\n",
    ))
    .unwrap()
    .unwrap();
    assert!(c.tls13_only);
    assert_eq!(c.limits.max_connections_per_source, 3);
    assert!((c.limits.identity_result_rate - 120.0 / 3600.0).abs() < 1e-9);
    assert_eq!(c.limits.plausibility.max_mission_points, 500);
    assert_eq!(c.limits.plausibility.turn_grace, 0);
    assert_eq!(c.limits.source_connect_rate, 0.5);
}

#[test]
fn config_off_or_invalid() {
    assert!(GatewayConfig::from_root(&"[network]\nbind_port = 1\n".parse().unwrap()).unwrap().is_none());
    assert!(GatewayConfig::from_root(&root("enabled = false\n")).unwrap().is_none());
    assert!(GatewayConfig::from_root(&root("tls_min = \"1.0\"\n")).is_err());
    assert!(GatewayConfig::from_root(&root("[gateway_steelbound.limits]\nmax_connections = 0\n")).is_err());
    assert!(GatewayConfig::from_root(&root("[gateway_steelbound.limits]\nsource_connect_rate = -1\n")).is_err());
}

#[test]
fn upstream_listener_must_stay_on_loopback() {
    let public: toml::Table = "[network]\nbind_port = 1\nbind_ip = \"0.0.0.0\"\n[gateway_steelbound]\nbind = \"0.0.0.0:47100\"\ncert = \"c\"\nkey = \"k\"\n"
        .parse()
        .unwrap();
    assert!(GatewayConfig::from_root(&public).unwrap_err().contains("loopback"));
    // Upstream default ("::", all interfaces) when bind_ip is missing.
    let missing: toml::Table = "[network]\nbind_port = 1\n[gateway_steelbound]\nbind = \"0.0.0.0:47100\"\ncert = \"c\"\nkey = \"k\"\n"
        .parse()
        .unwrap();
    assert!(GatewayConfig::from_root(&missing).is_err());
    let v6: toml::Table = "[network]\nbind_port = 1\nbind_ip = \"::1\"\n[gateway_steelbound]\nbind = \"[::]:47100\"\ncert = \"c\"\nkey = \"k\"\n"
        .parse()
        .unwrap();
    assert!(GatewayConfig::from_root(&v6).unwrap().is_some());
}

#[test]
fn example_configs_have_a_gateway_section() {
    for (name, text) in [
        ("steelcore.toml.example", include_str!("../../deploy/config/steelcore.toml.example")),
        ("steelcore-test.toml.example", include_str!("../../deploy/config/steelcore-test.toml.example")),
    ] {
        let root: toml::Table = text.parse().unwrap();
        let cfg = GatewayConfig::from_root(&root).unwrap_or_else(|e| panic!("{name}: {e}"));
        assert!(cfg.is_some(), "{name}: [gateway_steelbound] missing");
    }
}

// ---- Ticket gate ---------------------------------------------------------

#[test]
fn valid_ticket_passes() {
    let g = gateway(Limits::default());
    let t = g.check_ticket(&TicketSpec::valid(4242).sign()).unwrap();
    assert_eq!(t.account, 4242);
    assert_eq!(t.identity().1, "4242");
    assert_eq!(t.display_name, "Test Pilot");
}

#[test]
fn bad_tickets_are_refused() {
    let g = gateway(Limits::default());
    let mut expired = TicketSpec::valid(1);
    expired.issued_at = NOW - 2000;
    expired.expires_at = NOW - 200;
    assert_eq!(g.check_ticket(&expired.sign()).unwrap_err(), TicketError::Expired);

    // Signed with another key: unknown key id.
    let mut forged = TicketSpec::valid(1);
    forged.seed = [0x17; 32];
    assert_eq!(g.check_ticket(&forged.sign()).unwrap_err(), TicketError::UnknownKey);

    // Right key id, changed account after signing: bad signature.
    let mut tampered = TicketSpec::valid(1).sign();
    tampered[32] ^= 0x02;
    assert!(matches!(
        g.check_ticket(&tampered).unwrap_err(),
        TicketError::BadSignature | TicketError::BadIdentity
    ));

    let mut other = TicketSpec::valid(1);
    other.campaign = "steelbound-s1".into();
    assert_eq!(g.check_ticket(&other.sign()).unwrap_err(), TicketError::WrongCampaign);

    assert_eq!(g.check_ticket(&[]).unwrap_err(), TicketError::Malformed);
}

#[test]
fn shared_vector_ticket_is_accepted_by_a_matching_gateway() {
    // The issuer's own vectors (steelnet) pass the same gate.
    let v: toml::Table = include_str!("../../testdata/campaign_ticket_vectors.toml").parse().unwrap();
    let mut root = toml::Table::new();
    root.insert("ticket".into(), v["config"].clone());
    let cfg = crate::ticket::TicketConfig::from_toml(&root).unwrap().unwrap();
    let case = v["case"].as_array().unwrap().iter().find(|c| c["name"].as_str() == Some("valid")).unwrap();
    let now = case["now"].as_integer().unwrap() as u64;
    let hex = case["ticket"].as_str().unwrap();
    let bytes: Vec<u8> = (0..hex.len()).step_by(2).map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap()).collect();
    let g: Gateway<()> = Gateway::new(Arc::new(()), cfg, Limits::default(), Arc::new(move || now));
    assert_eq!(g.check_ticket(&bytes).unwrap().account, 4242);
}

// ---- Admission -----------------------------------------------------------

fn ip(s: &str) -> IpAddr {
    s.parse().unwrap()
}

#[test]
fn open_connections_per_source_are_limited() {
    let g = gateway(Limits { max_connections_per_source: 2, ..Limits::default() });
    let a = g.admit(ip("192.0.2.1")).unwrap();
    let _b = g.admit(ip("192.0.2.1")).unwrap();
    assert!(g.admit(ip("192.0.2.1")).is_none(), "third connection of the same source");
    assert!(g.admit(ip("192.0.2.2")).is_some(), "other source");
    drop(a);
    assert!(g.admit(ip("192.0.2.1")).is_some(), "slot freed on close");
}

#[test]
fn total_connections_are_limited() {
    let g = gateway(Limits { max_connections: 2, ..Limits::default() });
    let _a = g.admit(ip("192.0.2.1")).unwrap();
    let _b = g.admit(ip("192.0.2.2")).unwrap();
    assert!(g.admit(ip("192.0.2.3")).is_none());
}

#[test]
fn new_connections_per_source_are_rate_limited() {
    let g = gateway(Limits { source_connect_rate: 0.001, source_connect_burst: 3.0, ..Limits::default() });
    for _ in 0..3 {
        drop(g.admit(ip("198.51.100.7")).unwrap());
    }
    assert!(g.admit(ip("198.51.100.7")).is_none());
    // IPv4-mapped IPv6 is the same source.
    assert!(g.admit(ip("::ffff:198.51.100.7")).is_none());
    assert!(g.admit(ip("198.51.100.8")).is_some());
}

#[test]
fn ipv6_sources_are_limited_per_64() {
    assert_eq!(SourceKey::of(ip("2001:db8:1:2::1")), SourceKey::of(ip("2001:db8:1:2:ffff::9")));
    assert_ne!(SourceKey::of(ip("2001:db8:1:2::1")), SourceKey::of(ip("2001:db8:1:3::1")));
    assert_eq!(SourceKey::of(ip("::ffff:192.0.2.1")), SourceKey::of(ip("192.0.2.1")));
}

#[test]
fn refused_tickets_lock_the_source_out() {
    let g = gateway(Limits { source_failure_rate: 0.001, source_failure_burst: 2.0, ..Limits::default() });
    let src = SourceKey::of(ip("203.0.113.5"));
    g.refused(src, 0, "test");
    assert!(g.admit(ip("203.0.113.5")).is_some());
    g.refused(src, 1, "test");
    assert!(g.admit(ip("203.0.113.5")).is_none(), "locked out after two refused tickets");
    assert!(g.admit(ip("203.0.113.6")).is_some());
}

#[test]
fn limiter_forgets_idle_full_buckets() {
    let l: KeyedLimiter<u32> = KeyedLimiter::new(1000.0, 1.0);
    for k in 0..50 {
        assert!(l.allow(&k));
    }
    assert_eq!(l.len(), 50);
    std::thread::sleep(Duration::from_millis(5));
    assert!(l.has_tokens(&1));
}

// ---- Plausibility --------------------------------------------------------

fn user_point(round: u16, turn: u16, map: u8, stat: u32) -> RxCommand {
    RxCommand::AddUserPoint { round, turn, map, points: [stat; 10] }
}

fn mission_point(round: u16, turn: u16, points: u32, faction: Faction) -> RxCommand {
    RxCommand::AddMissionPoint { round, turn, map: 1, points, faction }
}

#[test]
fn plausibility_bounds() {
    let p = Plausibility::default();
    let now = Some((2, 4));
    let ok = |rx: RxCommand| limits::check(&p, &rx, now, Faction::Hsd);
    assert_eq!(ok(user_point(2, 4, 1, 50)), Ok(()));
    assert_eq!(ok(user_point(2, 3, 1, 50)), Ok(()), "one turn of grace");
    assert_eq!(ok(user_point(2, 2, 1, 50)), Err("wrong_period"));
    assert_eq!(ok(user_point(2, 5, 1, 50)), Err("wrong_period"), "future turn");
    assert_eq!(ok(user_point(1, 4, 1, 50)), Err("wrong_period"), "old round");
    assert_eq!(ok(user_point(2, 4, 9, 50)), Err("bad_map"));
    assert_eq!(ok(user_point(2, 4, 1, p.max_user_stat + 1)), Err("stat_too_high"));
    assert_eq!(
        limits::check(&p, &user_point(2, 4, 1, 1), now, Faction::None),
        Err("no_faction")
    );
    assert_eq!(limits::check(&p, &user_point(2, 4, 1, 1), None, Faction::Hsd), Err("wrong_period"));

    assert_eq!(ok(mission_point(2, 4, 300, Faction::Prf)), Ok(()));
    assert_eq!(ok(mission_point(2, 4, p.max_mission_points + 1, Faction::Prf)), Err("points_too_high"));
    assert_eq!(ok(mission_point(2, 4, 1, Faction::Jar)), Err("bad_faction"));
    assert_eq!(ok(mission_point(2, 4, 1, Faction::None)), Err("bad_faction"));

    assert_eq!(ok(RxCommand::VtAdd { vt_list: vec![1; 64] }), Ok(()));
    assert_eq!(ok(RxCommand::VtAdd { vt_list: vec![1; 65] }), Err("vt_list_too_long"));
    assert_eq!(ok(RxCommand::GetUserInfo), Ok(()));
}

/// The results of a short campaign battle (draw, no kills, both forces) are
/// accepted: the client reports zero statistics and zero mission points for
/// both forces of the map.
#[test]
fn campaign_battle_results_are_plausible() {
    let p = Plausibility::default();
    let now = Some((1, 1));
    for (me, other) in [(Faction::Hsd, Faction::Prf), (Faction::Prf, Faction::Hsd)] {
        assert_eq!(limits::check(&p, &user_point(1, 1, 1, 0), now, me), Ok(()));
        assert_eq!(limits::check(&p, &mission_point(1, 1, 0, me), now, me), Ok(()));
        assert_eq!(limits::check(&p, &mission_point(1, 1, 0, other), now, me), Ok(()));
        assert_eq!(limits::check(&p, &mission_point(1, 1, 10, other), now, me), Ok(()));
    }
    // After a turn change the result of the battle still counts once.
    assert_eq!(limits::check(&p, &user_point(1, 1, 1, 0), Some((1, 2)), Faction::Hsd), Ok(()));
    assert_eq!(limits::check(&p, &user_point(1, 1, 1, 0), Some((1, 3)), Faction::Hsd), Err("wrong_period"));
}

#[test]
fn writes_are_classified() {
    assert_eq!(limits::write_name(&RxCommand::GetUserInfo), None);
    assert_eq!(limits::write_name(&RxCommand::CampaignPeriod), None);
    assert_eq!(limits::write_name(&RxCommand::DelUserInfo), Some("DelUserInfo"));
    assert_eq!(limits::write_name(&RxCommand::SetUserFaction { faction: Faction::Rb }), Some("SetUserFaction"));
    assert_eq!(limits::write_name(&user_point(1, 1, 1, 1)), Some("AddUserPoint"));
    assert!(limits::is_result(&mission_point(1, 1, 1, Faction::Hsd)));
    assert!(!limits::is_result(&RxCommand::DelUserInfo));
}

#[test]
fn registered_name_comes_from_the_ticket() {
    assert_eq!(campaign_xname("Test Pilot"), "Test Pilot");
    assert_eq!(campaign_xname("Ünïcode Name"), "ncode Name");
    assert_eq!(campaign_xname("ThisNameIsWayTooLong"), "ThisNameIsWayTo");
    assert_eq!(campaign_xname("!!!"), "Pilot");
}
