// SPDX-License-Identifier: AGPL-3.0-only
//! Ticket verification tests. The shared vectors come from the issuer
//! (steelnet) and use test keys only.

use super::*;

const VECTORS: &str = include_str!("../../testdata/campaign_ticket_vectors.toml");

fn vectors() -> toml::Table {
    VECTORS.parse::<toml::Table>().expect("vector file parses")
}

fn vector_config(v: &toml::Table) -> TicketConfig {
    let mut table = toml::Table::new();
    table.insert("ticket".into(), v["config"].clone());
    TicketConfig::from_toml(&table).unwrap().unwrap()
}

fn case<'a>(v: &'a toml::Table, name: &str) -> (Vec<u8>, u64) {
    let c = v["case"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"].as_str() == Some(name))
        .unwrap_or_else(|| panic!("vector {name} missing"));
    (hex(c["ticket"].as_str().unwrap()), c["now"].as_integer().unwrap() as u64)
}

fn hex(s: &str) -> Vec<u8> {
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
}

#[test]
fn shared_vectors() {
    let v = vectors();
    let config = vector_config(&v);
    let cases = v["case"].as_array().unwrap();
    assert!(cases.len() >= 25, "only {} vectors", cases.len());
    let mut seen = std::collections::BTreeSet::new();
    for c in cases {
        let name = c["name"].as_str().unwrap();
        let ticket = hex(c["ticket"].as_str().unwrap());
        let now = c["now"].as_integer().unwrap() as u64;
        let expected = c["result"].as_str().unwrap();
        seen.insert(expected.to_string());
        match verify_at(&ticket, &config, now) {
            Ok(t) => {
                assert_eq!(expected, "ok", "{name}: accepted, expected {expected}");
                assert_eq!(i64::from(t.account), c["account"].as_integer().unwrap(), "{name}");
                assert_eq!(t.identity(), (Provider::Steelbound, c["external_id"].as_str().unwrap().to_string()), "{name}");
                assert_eq!(format!("{:016x}", t.xuid), c["xuid"].as_str().unwrap(), "{name}");
                assert_eq!(t.display_name, c["display_name"].as_str().unwrap(), "{name}");
                assert_eq!(t.campaign, c["campaign"].as_str().unwrap(), "{name}");
                assert_eq!(t.issuer, "steelnet-eu");
            }
            Err(e) => assert_eq!(e.code(), expected, "{name}"),
        }
    }
    // Every error code is covered by at least one vector.
    for code in [
        "ok", "malformed", "unsupported_version", "unknown_key", "bad_signature", "wrong_issuer",
        "wrong_provider", "wrong_campaign", "bad_identity", "bad_lifetime", "not_yet_valid", "expired",
    ] {
        assert!(seen.contains(code), "no vector for {code}");
    }
}

#[test]
fn reuse_within_lifetime_is_allowed() {
    let v = vectors();
    let config = vector_config(&v);
    let (ticket, now) = case(&v, "valid");
    let first = verify_at(&ticket, &config, now).unwrap();
    for dt in [0, 1, 60, 900, 1799, 1800 + 59] {
        assert_eq!(verify_at(&ticket, &config, now + dt).unwrap(), first);
    }
    assert_eq!(verify_at(&ticket, &config, now + 1800 + 60), Err(TicketError::Expired));
    assert_eq!(verify_at(&ticket, &config, now - 61), Err(TicketError::NotYetValid));
}

#[test]
fn any_single_bit_flip_is_refused() {
    let v = vectors();
    let config = vector_config(&v);
    let (ticket, now) = case(&v, "valid");
    for byte in 0..ticket.len() {
        for bit in 0..8 {
            let mut t = ticket.clone();
            t[byte] ^= 1 << bit;
            assert!(verify_at(&t, &config, now).is_err(), "flip {byte}.{bit} accepted");
        }
    }
}

#[test]
fn removed_key_stops_its_tickets() {
    let v = vectors();
    let keys: Vec<[u8; 32]> = v["config"]["public_keys"]
        .as_array()
        .unwrap()
        .iter()
        .map(|k| parse_hex32(k.as_str().unwrap()).unwrap())
        .collect();
    let (old, now) = case(&v, "valid");
    let (new, _) = case(&v, "valid_rotated_second_key");
    let only_new = TicketConfig::new("steelnet-eu", "steelbound-test", &keys[1..], 60, 3600).unwrap();
    assert_eq!(verify_at(&old, &only_new, now), Err(TicketError::UnknownKey));
    assert!(verify_at(&new, &only_new, now).is_ok());
}

#[test]
fn stricter_lifetime_and_skew_settings() {
    let v = vectors();
    let keys: Vec<[u8; 32]> = v["config"]["public_keys"]
        .as_array()
        .unwrap()
        .iter()
        .map(|k| parse_hex32(k.as_str().unwrap()).unwrap())
        .collect();
    let (ticket, now) = case(&v, "valid");
    let short = TicketConfig::new("steelnet-eu", "steelbound-test", &keys, 0, 600).unwrap();
    assert_eq!(verify_at(&ticket, &short, now), Err(TicketError::BadLifetime));
    let no_skew = TicketConfig::new("steelnet-eu", "steelbound-test", &keys, 0, 3600).unwrap();
    assert_eq!(verify_at(&ticket, &no_skew, now - 1), Err(TicketError::NotYetValid));
    assert_eq!(verify_at(&ticket, &no_skew, now + 1800), Err(TicketError::Expired));
    assert!(verify_at(&ticket, &no_skew, now + 1799).is_ok());
}

#[test]
fn garbage_never_panics() {
    let v = vectors();
    let config = vector_config(&v);
    let (ticket, now) = case(&v, "valid");
    for len in 0..=MAX_TICKET_LEN + 2 {
        let mut t = vec![0u8; len];
        let n = len.min(ticket.len());
        t[..n].copy_from_slice(&ticket[..n]);
        // Every prefix of the valid ticket, zero padded, except the ticket itself.
        assert_eq!(verify_at(&t, &config, now).is_ok(), len == ticket.len(), "length {len}");
        let ff = vec![0xFFu8; len];
        assert!(verify_at(&ff, &config, now).is_err());
    }
    assert_eq!(verify_at(&ticket, &config, u64::MAX), Err(TicketError::Expired));
}

#[test]
fn config_parsing() {
    let ok = r#"
        [ticket]
        issuer = "steelnet-eu"
        campaign = "steelbound-s1"
        public_keys = ["9b77bb24f64a8a35467f0e8020e578929d63cba5bcdb9459984b448ed486db0e"]
    "#;
    let c = TicketConfig::from_toml(&ok.parse().unwrap()).unwrap().unwrap();
    assert_eq!((c.max_clock_skew, c.max_lifetime), (DEFAULT_MAX_CLOCK_SKEW, DEFAULT_MAX_LIFETIME));
    assert_eq!(c.key_ids(), vec!["9b77bb24f64a8a35".to_string()]);
    assert!(!format!("{c:?}").contains("9b77bb24f64a8a35467f"), "Debug prints key ids only");

    assert!(TicketConfig::from_toml(&"[network]\nbind_port = 1".parse().unwrap()).unwrap().is_none());

    let bad = [
        ok.replace("steelbound-s1", "Steelbound S1"),
        ok.replace("steelnet-eu", ""),
        ok.replace("[\"9b", "[\"zz"),
        ok.replace("public_keys = [\"9b77bb24f64a8a35467f0e8020e578929d63cba5bcdb9459984b448ed486db0e\"]", "public_keys = []"),
        format!("{ok}\nmax_clock_skew = -1"),
        format!("{ok}\nmax_lifetime = 0"),
        // Identity point: a small-order (weak) key.
        ok.replace(
            "9b77bb24f64a8a35467f0e8020e578929d63cba5bcdb9459984b448ed486db0e",
            "0100000000000000000000000000000000000000000000000000000000000000",
        ),
        // The same key twice: duplicate key id.
        ok.replace(
            "\"9b77bb24f64a8a35467f0e8020e578929d63cba5bcdb9459984b448ed486db0e\"",
            "\"9b77bb24f64a8a35467f0e8020e578929d63cba5bcdb9459984b448ed486db0e\", \"9b77bb24f64a8a35467f0e8020e578929d63cba5bcdb9459984b448ed486db0e\"",
        ),
    ];
    for (i, text) in bad.iter().enumerate() {
        let table: toml::Table = text.parse().unwrap();
        assert!(TicketConfig::from_toml(&table).is_err(), "bad config {i} accepted");
    }
}

#[test]
fn example_configs_parse() {
    for text in [
        include_str!("../../deploy/config/steelcore.toml.example"),
        include_str!("../../deploy/config/steelcore-test.toml.example"),
    ] {
        let table: toml::Table = text.parse().unwrap();
        // The examples ship without keys: the section is present but must be
        // completed on the server, so parsing reports the missing keys.
        let err = TicketConfig::from_toml(&table).unwrap_err();
        assert!(err.to_string().contains("public_keys is empty"), "{err}");
    }
}
