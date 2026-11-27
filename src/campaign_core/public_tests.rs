// SPDX-License-Identifier: AGPL-3.0-only
//! Tests of the public campaign data (`public.rs`). Synthetic data only.

use super::public::*;
use super::tests::test_pool;
use chrono::{TimeZone, Utc};
use serde_json::Value;
use std::{
    collections::{BTreeSet, HashMap, HashSet},
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};
use uuid::Uuid;

fn cfg() -> PublicConfig {
    let root: toml::Table = r#"
        [campaign]
        turn_days = 7
        [ticket]
        campaign = "steelbound-s1"
        [public]
        name = "Steelbound Campaign"
        season = "Season 1"
        leaderboard_size = 3
        battles = 5
        map_base = 0
    "#
    .parse()
    .unwrap();
    PublicConfig::from_toml(&root).unwrap()
}

fn uid(n: u128) -> Uuid {
    Uuid::from_u128(0x5342_4c01_0000_0000_0000_0000_0000_0000 | n)
}

/// Every key that may appear anywhere in the document (whitelist).
const KEYS: &[&str] = &[
    "schema", "generated_at", "campaign", "id", "name", "season", "period", "round", "turn", "turns_per_round",
    "turn_minutes", "turn_started_at", "turn_ends_at", "sides", "joins_turn", "enlisted", "active", "pilot_points",
    "war_points", "sectors_held", "influence", "trend", "last_24h", "prev_24h", "sectors", "index", "slot", "state",
    "played", "points", "share", "hsd", "prf", "rb", "leader", "leaderboards", "overall", "rank", "pilot", "side",
    "kills", "deaths", "wins", "sorties", "captures", "battles", "at", "sector", "result", "totals", "ranked_pilots",
    "battles_round",
    // W526: all ranking categories of the game, full ranking list with round and turn.
    "hits", "shots", "base_captures", "capture_count", "tippings", "pilots", "ranked_pilots_turn",
    // W527: profile slugs, awards, turn reports.
    "slug", "awards", "turns", "ends_at", "closest", "gap", "margin", "discord_md", "categories", "per_turn",
    // W529: battles grouped into matches.
    "matches", "winner", "note",
    // W531: VTs (names only, never VT ratings).
    "vt", "top_vts", "vts_since",
];

/// Keys of a pilot file (W527).
const PILOT_KEYS: &[&str] = &[
    "schema", "campaign", "id", "period", "round", "turn", "pilot", "slug", "side", "hits", "shots", "kills", "points",
    "deaths", "wins", "sorties", "base_captures", "capture_count", "tippings", "history", "favorite_sector", "sector",
    "battles", "at", "result", "awards", "category", "rank",
    // W531: VTs per pilot.
    "vt", "vts", "driven", "lost", "favorite_vt", "killed_by_type",
];

fn keys(v: &Value, out: &mut BTreeSet<String>) {
    match v {
        Value::Object(m) => {
            for (k, x) in m {
                out.insert(k.clone());
                keys(x, out);
            }
        }
        Value::Array(a) => a.iter().for_each(|x| keys(x, out)),
        _ => {}
    }
}

/// Fails if any form of a player id shows up in the text.
fn assert_no_ids(text: &str, players: &[Uuid]) {
    let lower = text.to_lowercase();
    for p in players {
        for form in [p.hyphenated().to_string(), p.simple().to_string(), format!("{:016x}", p.as_u128() as u64)] {
            assert!(!lower.contains(&form), "player id {form} leaked into the public document");
        }
    }
    for marker in ["xuid", "player_id", "identity", "provider", "external", "gamertag", "ip"] {
        assert!(!lower.contains(&format!("\"{marker}\"")), "field {marker} in the public document");
    }
}

fn snapshot() -> Snapshot {
    let s = |n: u128, name: Option<&str>, f: &str, kp: i64, kills: i64| Standing {
        player: uid(n),
        name: name.map(str::to_owned),
        faction: Some(f.to_owned()),
        stats: [10, 20, kills, kp, 1, 2, 3, 0, 0, 0],
        turn_stats: [0; 10],
    };
    let at = Utc.with_ymd_and_hms(2026, 11, 26, 12, 34, 56).unwrap();
    let battle = |n: u128, name: Option<&str>, f: &str, map: i16, win: bool| Battle {
        at,
        turn: 3,
        map,
        faction: f.to_owned(),
        player: uid(n),
        name: name.map(str::to_owned),
        points: vec![5, 9, 2, 150, 0, if win { 1 } else { 0 }, 1, 0, 0, 0],
        vt: None,
    };
    let mut sides = HashMap::new();
    sides.insert("hsd".to_owned(), SideCounts { enlisted: 4, active: 2, last_24h: 300, prev_24h: 100 });
    sides.insert("prf".to_owned(), SideCounts { enlisted: 3, active: 1, last_24h: 0, prev_24h: 50 });
    Snapshot {
        now: at,
        period: Period { round: 1, turn: 3, ends_at: Utc.with_ymd_and_hms(2026, 11, 30, 18, 0, 0).unwrap() },
        standings: vec![
            s(1, Some("Ace One"), "hsd", 900, 9),
            s(2, None, "prf", 800, 8),
            s(3, Some("bad\u{7}name"), "rb", 700, 7),
            s(4, Some("Dee"), "hsd", 900, 3),
            s(5, Some("Eve"), "hsd", 10, 1),
            s(6, Some("Fay"), "hsd", 5, 0),
        ],
        sectors: vec![
            SectorTide { map_id: 0, points: [500, 100, 0] },
            SectorTide { map_id: 1, points: [100, 400, 0] },
            SectorTide { map_id: 6, points: [10, 10, 10] },
            SectorTide { map_id: 7, points: [0, 0, 90] },
            SectorTide { map_id: 99, points: [1, 1, 1] },
        ],
        battles: vec![battle(1, Some("Ace One"), "hsd", 0, true), battle(2, None, "prf", 2, false), battle(7, None, "rb", 9, true)],
        battles_round: 17,
        sides,
        turn_rows: Vec::new(),
        pilot_battles: Vec::new(),
        sector_counts: Vec::new(),
        turn_battles: Vec::new(),
        turn_ends: Vec::new(),
        match_rows: Vec::new(),
        match_rows_complete: true,
        vt_names: HashMap::new(),
        vt_since: None,
        pilot_vts: Vec::new(),
        side_vts: Vec::new(),
        vt_rows: Vec::new(),
        vt_rows_complete: true,
    }
}

// ---- Configuration -------------------------------------------------------

#[test]
fn public_campaign_id_rule() {
    for ok in ["steelbound-s1", "a", "0", "x-y-z", &"a".repeat(32)] {
        assert!(valid_campaign_id(ok), "{ok}");
    }
    for bad in ["", "Steelbound", "a_b", "a/b", "../x", "a.json", "a b", &"a".repeat(33), "ä"] {
        assert!(!valid_campaign_id(bad), "{bad}");
    }
}

#[test]
fn public_config_defaults_and_refusals() {
    let c = cfg();
    assert_eq!(c.campaign, "steelbound-s1");
    assert_eq!(c.out_file(), std::path::Path::new("/var/lib/steelcore/public/steelbound-s1.json"));
    assert_eq!(c.turn_length, Duration::from_secs(7 * 24 * 3600));
    assert_eq!(c.side_names, ["HSD", "PRF", "RB"]);
    let parse = |s: &str| PublicConfig::from_toml(&s.parse::<toml::Table>().unwrap());
    assert!(parse("[public]\nname = \"x\"").is_err(), "no campaign id");
    assert!(parse("[public]\ncampaign = \"../etc\"").is_err());
    assert!(parse("[public]\ncampaign = \"a\"\nmap_base = 2").is_err());
    assert!(parse("[public]\ncampaign = \"a\"\nleaderboard_size = 1000").is_err());
    assert!(parse("[public]\ncampaign = \"a\"\nsector_names = [\"x\"]").is_err());
    let short = parse("[campaign]\nturn_minutes = 180\n[public]\ncampaign = \"steelbound-test\"").unwrap();
    assert_eq!(short.turn_length, Duration::from_secs(180 * 60));
    assert_eq!(short.map_base, 1, "the game sends maps 1..=3 per turn");
}

/// The shipped config template carries the 24 mission names and the game's map base.
#[test]
fn public_config_template_has_sector_names() {
    let root: toml::Table = include_str!("../../deploy/config/steelcore.toml.example").parse().unwrap();
    let c = PublicConfig::from_toml(&root).unwrap();
    assert_eq!(c.map_base, 1);
    assert_eq!(c.sector_names.len(), 24);
    assert_eq!(c.sector_names[0], "Sha Wan Hills");
    assert_eq!(c.sector_names[23], "Defense of Yu Jai Village");
    let doc = build(&c, &snapshot(), &mut Placeholders::new(&[7u8; 32], &c.season));
    assert_eq!(doc["sectors"][10]["name"], "San Kuchen Subterranean Fortress");
}

/// Live data of 26.11.2026: turn 1 had war tide rows and battles on maps 1, 2
/// and 3. With the default map base every one of them lands in a turn-1
/// sector, no battle is "unknown" and no turn-2 sector shows points.
#[test]
fn public_default_map_base_places_live_maps() {
    let root: toml::Table = "[ticket]\ncampaign = \"steelbound-s1\"\n[public]\nname = \"x\"".parse().unwrap();
    let c = PublicConfig::from_toml(&root).unwrap();
    assert_eq!(c.map_base, 1);
    let mut snap = snapshot();
    snap.period.turn = 1;
    snap.sectors = vec![
        SectorTide { map_id: 1, points: [0, 60, 0] },
        SectorTide { map_id: 2, points: [60, 60, 0] },
        SectorTide { map_id: 3, points: [0, 60, 0] },
        SectorTide { map_id: 0, points: [9, 9, 9] },
    ];
    for b in &mut snap.battles {
        b.turn = 1;
    }
    snap.battles[0].map = 2;
    snap.battles[1].map = 3;
    snap.battles[2].map = 1;
    let doc = build(&c, &snap, &mut Placeholders::new(&[7u8; 32], &c.season));
    let sectors = doc["sectors"].as_array().unwrap();
    assert_eq!(sectors[0]["leader"], "prf");
    assert_eq!(sectors[1]["leader"], Value::Null, "tie");
    assert_eq!(sectors[1]["played"], true);
    assert_eq!(sectors[2]["leader"], "prf");
    assert_eq!(sectors[2]["name"], "Sector 1-C");
    assert_eq!(sectors[3]["played"], false, "turn 2 untouched");
    assert_eq!(sectors[3]["turn"], 2);
    let sides = doc["sides"].as_array().unwrap();
    assert_eq!(sides[1]["sectors_held"], 2);
    assert_eq!(sides[1]["war_points"], 180, "the map-0 row is not part of the round");
    // 2 won + 21 unplayed / 2 for PRF, 0 + 10.5 for HSD (map 0 ignored).
    assert_eq!(sides[1]["influence"], 54);
    let b = doc["battles"].as_array().unwrap();
    assert_eq!(b.iter().map(|x| x["sector"].as_i64()).collect::<Vec<_>>(), [Some(1), Some(2), Some(0)]);
    assert_eq!(sector_of(1, 0, 1), None, "map 0 is not a game map with the default");
}

// ---- Game formulas -------------------------------------------------------

#[test]
fn public_war_tide_matches_the_game() {
    assert_eq!(leader(&[5, 4, 4]), Some(0));
    assert_eq!(leader(&[4, 4, 0]), None, "a tie holds no sector");
    assert_eq!(leader(&[0, 0, 1]), Some(2));
    assert_eq!(leader(&[0, 0, 0]), None);
    // war_tide: 300 starting points, RB only from turn 3, rounding to HSD.
    assert_eq!(sector_share(&[0, 0, 0], 1), [50, 50, 0]);
    assert_eq!(sector_share(&[0, 0, 0], 3), [34, 33, 33]);
    assert_eq!(sector_share(&[300, 0, 0], 2), [67, 33, 0]);
    // war_influence: nothing played = equal shares of the 24 sectors.
    assert_eq!(influence(&[], 1), [50, 50, 0]);
    assert_eq!(influence(&[], 3), [34, 33, 33]);
    let won = SectorTide { map_id: 0, points: [1, 0, 0] };
    // 1 won + 23 unplayed / 2 each in turn 1: 12.5 / 11.5 of 24.
    assert_eq!(influence(&[won], 1), [53, 47, 0]);
    assert_eq!(sector_of(1, 0, 0), Some(0));
    assert_eq!(sector_of(3, 2, 0), Some(8));
    assert_eq!(sector_of(3, 3, 0), None);
    assert_eq!(sector_of(3, 3, 1), Some(8));
    assert_eq!(sector_of(9, 0, 0), None);
    assert_eq!(default_sector_name(0), "Sector 1-A");
    assert_eq!(default_sector_name(23), "Sector 8-C");
}

#[test]
fn public_placeholders_are_stable_unique_and_per_season() {
    let players: Vec<Uuid> = (1..=3000u128).map(uid).collect();
    let mut a = Placeholders::new(b"0123456789abcdef0123456789abcdef", "Season 1");
    a.assign(&players);
    let names: Vec<String> = players.iter().map(|p| a.name(p)).collect();
    let unique: BTreeSet<&String> = names.iter().collect();
    assert_eq!(unique.len(), names.len(), "placeholder numbers are unique");
    for n in &names {
        assert!(n.starts_with("Pilot #") && n.len() == 11, "{n}");
    }
    // Same key and season: same number, whatever the order.
    let mut b = Placeholders::new(b"0123456789abcdef0123456789abcdef", "Season 1");
    b.assign(&players.iter().rev().copied().collect::<Vec<_>>());
    assert_eq!(a.name(&uid(42)), b.name(&uid(42)));
    // Another season or key: other numbers (for most players).
    let mut c = Placeholders::new(b"0123456789abcdef0123456789abcdef", "Season 2");
    let mut d = Placeholders::new(b"fedcba9876543210fedcba9876543210", "Season 1");
    let mut differ = |x: &mut Placeholders| (1..=50u128).filter(|n| x.name(&uid(*n)) != a.name(&uid(*n))).count();
    assert!(differ(&mut c) > 40 && differ(&mut d) > 40);
}

// ---- The document --------------------------------------------------------

#[test]
fn public_document_shape_and_privacy() {
    let c = cfg();
    let snap = snapshot();
    let doc = build(&c, &snap, &mut Placeholders::new(&[7u8; 32], &c.season));
    let text = String::from_utf8(render(&doc).unwrap()).unwrap();

    let mut found = BTreeSet::new();
    keys(&doc, &mut found);
    let unknown: Vec<&String> = found.iter().filter(|k| !KEYS.contains(&k.as_str())).collect();
    assert!(unknown.is_empty(), "keys outside the whitelist: {unknown:?}");
    assert_no_ids(&text, &(1..=7u128).map(uid).collect::<Vec<_>>());

    assert_eq!(doc["schema"], 1);
    assert_eq!(doc["campaign"]["id"], "steelbound-s1");
    assert_eq!(doc["period"]["turn"], 3);
    assert_eq!(doc["period"]["turn_minutes"], 7 * 24 * 60);
    assert_eq!(doc["period"]["turn_ends_at"], "2026-11-30T18:00:00Z");
    assert_eq!(doc["period"]["turn_started_at"], "2026-11-23T18:00:00Z");

    // Rankings: kill points, then kills; top 3 only; names cleaned or replaced.
    let overall = doc["leaderboards"]["overall"].as_array().unwrap();
    assert_eq!(overall.len(), 3);
    assert_eq!(overall[0]["pilot"], "Ace One");
    assert_eq!(overall[1]["pilot"], "Dee");
    let p2 = overall[2]["pilot"].as_str().unwrap();
    assert!(p2.starts_with("Pilot #"), "no display name -> placeholder, got {p2}");
    assert_eq!(overall[0]["rank"], 1);
    assert_eq!(overall[0]["points"], 900);
    let hsd = doc["leaderboards"]["hsd"].as_array().unwrap();
    assert_eq!(hsd.iter().map(|r| r["pilot"].as_str().unwrap()).collect::<Vec<_>>(), ["Ace One", "Dee", "Eve"]);
    let rb = doc["leaderboards"]["rb"].as_array().unwrap();
    assert!(rb[0]["pilot"].as_str().unwrap().starts_with("Pilot #"), "control characters -> placeholder");
    assert!(!text.contains("bad"), "an unusable name never appears");

    // Sectors: 24, war tide placed by map id, out-of-range rows ignored.
    let sectors = doc["sectors"].as_array().unwrap();
    assert_eq!(sectors.len(), 24);
    assert_eq!(sectors[0]["leader"], "hsd");
    assert_eq!(sectors[0]["state"], "closed");
    assert_eq!(sectors[1]["leader"], "prf");
    assert_eq!(sectors[6]["leader"], Value::Null);
    assert_eq!(sectors[7]["leader"], "rb");
    assert_eq!(sectors[7]["state"], "contested");
    assert_eq!(sectors[9]["state"], "upcoming");
    assert_eq!(sectors[9]["played"], false);
    assert_eq!(sectors[9]["share"]["hsd"], 0);
    let sides = doc["sides"].as_array().unwrap();
    assert_eq!(sides[0]["sectors_held"], 1);
    assert_eq!(sides[0]["war_points"], 610);
    assert_eq!(sides[0]["enlisted"], 4);
    assert_eq!(sides[0]["trend"]["last_24h"], 300);
    assert_eq!(sides[2]["enlisted"], 0, "no row -> zero");
    assert_eq!(sides[2]["joins_turn"], 3);
    let infl: i64 = sides.iter().map(|s| s["influence"].as_i64().unwrap()).sum();
    assert_eq!(infl, 100);

    // Battles: minute precision, sector index, result.
    let b = doc["battles"].as_array().unwrap();
    assert_eq!(b[0]["at"], "2026-11-26T12:34:00Z");
    assert_eq!(b[0]["sector"], 6);
    assert_eq!(b[0]["result"], "win");
    assert_eq!(b[0]["points"], 150);
    assert_eq!(b[1]["result"], "loss");
    assert_eq!(b[2]["sector"], Value::Null, "map out of range");
    assert_eq!(doc["totals"]["battles_round"], 17);
    // The same player gets the same placeholder in the ranking and the feed.
    let p = b[1]["pilot"].as_str().unwrap();
    assert!(doc["leaderboards"]["prf"][0]["pilot"] == p);
}

#[test]
fn public_full_ranking_list_has_every_category_round_and_turn() {
    let c = cfg();
    let mut snap = snapshot();
    // Ace played this turn too; Fay has only tippings this round.
    snap.standings[0].stats = [40, 160, 9, 900, 3, 4, 6, 2, 5, 1];
    snap.standings[0].turn_stats = [4, 10, 1, 90, 0, 1, 1, 0, 1, 0];
    snap.standings[5].stats = [0, 0, 0, 0, 0, 0, 0, 0, 0, 7];
    let doc = build(&c, &snap, &mut Placeholders::new(&[7u8; 32], &c.season));
    render(&doc).unwrap();

    // Leaderboard rows: new fields only, `captures` keeps its W524 meaning (stat8 = base captures).
    let top = &doc["leaderboards"]["overall"][0];
    assert_eq!(top["pilot"], "Ace One");
    assert_eq!((top["hits"].as_i64(), top["shots"].as_i64()), (Some(40), Some(160)));
    assert_eq!(top["captures"], 2);
    assert_eq!(top["base_captures"], 2);
    assert_eq!(top["capture_count"], 5);
    assert_eq!(top["tippings"], 1);
    assert_eq!(top["sorties"], 6, "sorties = missions played (stat7)");

    // Full list: every ranked pilot (more than leaderboard_size = 3), sorted like the ranking.
    let pilots = doc["pilots"].as_array().unwrap();
    assert_eq!(pilots.len(), 6);
    assert_eq!(doc["totals"]["ranked_pilots"], 6);
    assert_eq!(doc["totals"]["ranked_pilots_turn"], 1);
    assert_eq!(pilots[0]["pilot"], "Ace One");
    assert_eq!(pilots[0]["side"], "hsd");
    let r = &pilots[0]["round"];
    let want = [
        ("hits", 40),
        ("shots", 160),
        ("kills", 9),
        ("points", 900),
        ("deaths", 3),
        ("wins", 4),
        ("sorties", 6),
        ("base_captures", 2),
        ("capture_count", 5),
        ("tippings", 1),
    ];
    for (k, v) in want {
        assert_eq!(r[k], v, "round.{k}");
    }
    assert_eq!(r.as_object().unwrap().len(), 10);
    assert_eq!(pilots[0]["turn"]["points"], 90);
    assert_eq!(pilots[0]["turn"]["capture_count"], 1);
    assert!(pilots[1..].iter().all(|p| p["turn"].is_null()), "no games this turn -> null");
    let fay = pilots.iter().find(|p| p["pilot"] == "Fay").expect("a pilot with only tippings is ranked");
    assert_eq!(fay["round"]["tippings"], 7);
    // The same names as in the leaderboards (placeholders, cleaned names).
    let p2 = pilots.iter().find(|p| p["side"] == "prf").unwrap();
    assert_eq!(p2["pilot"], doc["leaderboards"]["prf"][0]["pilot"]);
    assert!(!String::from_utf8(render(&doc).unwrap()).unwrap().contains("bad"));
}

#[test]
fn public_ranking_selection_keeps_every_category_leader() {
    // 50 pilots sorted by kill points; the list holds 12.
    let mut all: Vec<Standing> = (0..50u128)
        .map(|n| Standing {
            player: uid(n),
            name: None,
            faction: Some("hsd".into()),
            stats: [1, 1, 1, 1000 - n as i64, 1, 1, 1, 0, 0, 0],
            turn_stats: [0; 10],
        })
        .collect();
    all[40].stats[9] = 99; // most tippings
    all[45].turn_stats[3] = 5; // the only kill points this turn
    all[49].stats[0] = 500; // most hits
    let refs: Vec<&Standing> = all.iter().collect();
    let keep = ranking_selection(&refs, 12);
    assert_eq!(keep.len(), 12);
    for i in [0, 40, 45, 49] {
        assert!(keep.contains(&i), "pilot {i} leads a category and must stay");
    }
    assert!(keep.contains(&1) && !keep.contains(&30), "the rest follows kill points");
    assert_eq!(ranking_selection(&refs, 50).len(), 50, "everyone while they fit");

    // The document follows the selection and keeps the kill-point order.
    let c = PublicConfig { ranking_pilots: 12, ..cfg() };
    let mut snap = snapshot();
    snap.standings = all;
    let doc = build(&c, &snap, &mut Placeholders::new(&[2u8; 32], ""));
    let pilots = doc["pilots"].as_array().unwrap();
    assert_eq!(pilots.len(), 12);
    assert_eq!(doc["totals"]["ranked_pilots"], 50);
    let pts: Vec<i64> = pilots.iter().map(|p| p["round"]["points"].as_i64().unwrap()).collect();
    assert!(pts.windows(2).all(|w| w[0] >= w[1]), "sorted by kill points: {pts:?}");
    assert!(pilots.iter().any(|p| p["round"]["tippings"] == 99));
}

#[test]
fn public_document_size_is_bounded() {
    let c = PublicConfig { leaderboard_size: 100, battles: 100, ..cfg() };
    let mut snap = snapshot();
    snap.standings = (0..5000u128)
        .map(|n| Standing {
            player: uid(n),
            name: None,
            faction: Some("hsd".into()),
            stats: [n as i64; 10],
            turn_stats: [n as i64; 10],
        })
        .collect();
    let doc = build(&c, &snap, &mut Placeholders::new(&[1u8; 32], ""));
    let bytes = render(&doc).unwrap();
    assert!(bytes.len() < MAX_BYTES / 2, "{} bytes", bytes.len());

    // Worst case of every list at its upper bound: longest names, large values, both periods.
    let root: toml::Table = r#"
        [ticket]
        campaign = "steelbound-s1"
        [public]
        leaderboard_size = 100
        ranking_pilots = 250
        battles = 100
        matches = 50
    "#
    .parse()
    .unwrap();
    let max = PublicConfig::from_toml(&root).unwrap();
    assert_eq!((max.leaderboard_size, max.ranking_pilots, max.battles, max.matches), (100, 250, 100, 50));
    snap.standings = (0..1000u128)
        .map(|n| Standing {
            player: uid(n),
            name: Some(format!("Pilot{n:011}")),
            faction: Some(["hsd", "prf", "rb"][n as usize % 3].into()),
            stats: [9_999_999; 10],
            turn_stats: [999_999; 10],
        })
        .collect();
    let at = snap.now;
    snap.battles = (0..100u128)
        .map(|n| Battle {
            at,
            turn: 8,
            map: 2,
            faction: "prf".into(),
            player: uid(n),
            name: Some(format!("Pilot{n:011}")),
            points: vec![9_999_999; 10],
            vt: Some((n % 40) as i16),
        })
        .collect();
    // W527: turn 8 (seven closed turns), every listed pilot holds all four award
    // categories in every closed turn (more than the 40 places a turn has), a full history.
    snap.period.turn = 8;
    snap.turn_rows = (0..1000u128)
        .flat_map(|n| {
            (1..=8i16).map(move |t| TurnRow {
                player: uid(n),
                turn: t,
                faction: Some("prf".into()),
                stats: [999_999, 1_000_000, 999_999, 9_999_999 - n as i64, 9, 99_999, 99_999, 9_999, 9, 9],
            })
        })
        .collect();
    snap.pilot_battles = (0..250u128)
        .flat_map(|n| {
            (0..12).map(move |_| Battle {
                at,
                turn: 8,
                map: 2,
                faction: "prf".into(),
                player: uid(n),
                name: Some(format!("Pilot{n:011}")),
                points: vec![9_999_999; 10],
                vt: Some((n % 40) as i16),
            })
        })
        .collect();
    snap.sector_counts = (0..1000u128).map(|n| (uid(n), 8, 2, 99_999)).collect();
    snap.turn_battles = (1..=8).map(|t| (t, 9_999_999)).collect();
    snap.turn_ends = (1..=8).map(|t| (t, at)).collect();
    // W529: 50 matches of 12 listed pilots each (6 against 6), every value at 7 digits.
    snap.match_rows = (0..600u128)
        .map(|n| Battle {
            at: at - chrono::Duration::seconds((n / 12) as i64 * 600),
            turn: 8,
            map: 2,
            faction: if n % 2 == 0 { "prf" } else { "hsd" }.into(),
            player: uid(n % 250),
            name: Some(format!("Pilot{:011}", n % 250)),
            points: vec![9_999_999; 10],
            vt: Some((n % 40) as i16),
        })
        .collect();
    // W531: every battle with a VT, 40 types with names beyond the character
    // limit (20), every listed pilot with all 40 types driven and destroyed.
    snap.vt_names = (0..40i16).map(|t| (t, format!("{t:0>32}"))).collect();
    snap.vt_since = Some(at);
    snap.pilot_vts = (0..1000u128)
        .flat_map(|n| (0..40i16).map(move |t| (uid(n), t, 9_999_999, 9_999_999, 9_999_999, 9_999_999, 9_999_999)))
        .collect();
    snap.side_vts = SIDES.iter().flat_map(|f| (0..40i16).map(move |t| (f.to_string(), t, 9_999_999))).collect();
    snap.vt_rows = (0..250u128 * 40)
        .map(|k| Battle {
            at: at - chrono::Duration::seconds((k / 2) as i64 * 600),
            turn: 8,
            map: 2,
            faction: if k % 2 == 0 { "prf" } else { "hsd" }.into(),
            player: uid((k / 80) * 2 + k % 2),
            name: None,
            points: vec![9_999_999; 10],
            vt: Some(((k / 2) % 40) as i16),
        })
        .collect();
    let (doc, files) = build_all(&max, &snap, &mut Placeholders::new(&[1u8; 32], ""));
    let kept = doc["matches"].as_array().unwrap().len();
    // W531: with a VT name (20 characters) on every battle and pilot, 5 matches
    // stay in this extreme case (W529 without VTs: 8); matches give way first.
    assert!(kept >= 5, "the largest configuration still shows {kept} matches");
    eprintln!("worst case: {kept} of 50 matches kept");
    assert_eq!(doc["pilots"].as_array().unwrap().len(), 250);
    assert_eq!(doc["turns"].as_array().unwrap().len(), 8);
    let bytes = render(&doc).expect("the largest configuration fits the size limit");
    // W527: slugs, awards and turn reports add about 15 KB; the largest
    // configuration with every value at 7 digits keeps 5 % room (236 KB).
    assert!(bytes.len() < MAX_BYTES * 95 / 100, "{} bytes", bytes.len());
    eprintln!("worst case: document {} bytes", bytes.len());
    // The default list lengths (25 / 200 / 30) keep more than 20 % room.
    let root: toml::Table = "[ticket]\ncampaign = \"steelbound-s1\"".parse().unwrap();
    let (dflt, _) = build_all(&PublicConfig::from_toml(&root).unwrap(), &snap, &mut Placeholders::new(&[1u8; 32], ""));
    assert_eq!(dflt["matches"].as_array().unwrap().len(), 30, "the default list lengths keep every match");
    let n = render(&dflt).unwrap().len();
    assert!(n < MAX_BYTES * 80 / 100, "{n} bytes");
    eprintln!("worst case with the default list lengths: {n} bytes");
    let rendered = render_pilots(&files).expect("every pilot file fits its limit");
    let largest = rendered.iter().map(|(_, b)| b.len()).max().unwrap();
    assert!(largest < MAX_PILOT_BYTES / 4, "{largest} bytes");
    eprintln!("worst case: largest pilot file {largest} bytes, {} files", rendered.len());
    let huge = Value::String("x".repeat(MAX_BYTES));
    assert!(render(&huge).is_err());
}

// ---- W529: matches -----------------------------------------------------------

/// One pilot report `secs` after a fixed time (turn 3), with kill points and kills.
#[allow(clippy::too_many_arguments)]
fn mrow(n: u128, name: Option<&str>, secs: i64, map: i16, f: &str, win: Option<bool>, kp: i64, kills: i64) -> Battle {
    let base = Utc.with_ymd_and_hms(2026, 11, 26, 20, 0, 0).unwrap();
    Battle {
        at: base + chrono::Duration::seconds(secs),
        turn: 3,
        map,
        faction: f.to_owned(),
        player: uid(n),
        name: name.map(str::to_owned),
        points: match win {
            Some(w) => vec![5, 9, kills, kp, 1, w as i64, 1, 0, 0, 0],
            None => vec![kp],
        },
        vt: None,
    }
}

/// Newest first, like the query (`ORDER BY at DESC, result_id DESC`; the list
/// order stands in for the result id).
fn newest_first(rows: Vec<Battle>) -> Vec<Battle> {
    let mut idx: Vec<usize> = (0..rows.len()).collect();
    idx.sort_by(|&a, &b| rows[b].at.cmp(&rows[a].at).then(b.cmp(&a)));
    idx.into_iter().map(|i| rows[i].clone()).collect()
}

/// The players (low bits of the id) of every group, newest group first.
fn players(rows: &[Battle], groups: &[Vec<usize>]) -> Vec<Vec<u128>> {
    groups
        .iter()
        .map(|g| {
            let mut v: Vec<u128> = g.iter().map(|&i| rows[i].player.as_u128() & 0xffff).collect();
            v.sort();
            v
        })
        .collect()
}

#[test]
fn public_matches_group_by_sector_window_and_pilot() {
    let w = chrono::Duration::seconds(MATCH_WINDOW_SECS);
    let rows = newest_first(vec![
        // 1:1 in sector 0, both reports within seconds.
        mrow(1, Some("Ace One"), 0, 0, "hsd", Some(true), 50, 1),
        mrow(2, None, 7, 0, "prf", Some(false), 0, 0),
        // The same two pilots again ten minutes later: a new match.
        mrow(1, Some("Ace One"), 600, 0, "hsd", Some(false), 0, 0),
        mrow(2, None, 610, 0, "prf", Some(true), 100, 2),
        // Window: 120 s after the first report still joins, 121 s starts a new match.
        mrow(3, None, 1000, 1, "hsd", Some(true), 10, 1),
        mrow(4, None, 1120, 1, "prf", Some(false), 0, 0),
        mrow(5, None, 1121, 1, "prf", Some(false), 0, 0),
        // At the same time in another sector: its own match.
        mrow(6, None, 1000, 2, "rb", Some(true), 10, 1),
        // A pilot reports twice within the window (two short matches): the second
        // report starts a new match, later reports join that one.
        mrow(7, None, 2000, 0, "hsd", Some(true), 10, 1),
        mrow(8, None, 2005, 0, "prf", Some(false), 0, 0),
        mrow(7, None, 2030, 0, "hsd", Some(false), 0, 0),
        mrow(9, None, 2040, 0, "prf", Some(true), 10, 1),
    ]);
    let g = group_matches(&rows, w, true);
    assert_eq!(
        players(&rows, &g),
        vec![vec![7, 9], vec![7, 8], vec![5], vec![3, 4], vec![6], vec![1, 2], vec![1, 2]],
        "newest match first (by its last report)"
    );
    // Same round, turn and sector only: another turn never joins.
    let mut other = mrow(2, None, 7, 0, "prf", Some(false), 0, 0);
    other.turn = 4;
    let rows = newest_first(vec![mrow(1, None, 0, 0, "hsd", Some(true), 0, 0), other]);
    assert_eq!(group_matches(&rows, w, true).len(), 2);

    // The query hit its row limit: a match that began within the window of the
    // oldest row may miss reports and is left out; later ones stay.
    let rows = newest_first(vec![
        mrow(1, None, 0, 0, "hsd", Some(true), 0, 0),
        mrow(2, None, 60, 0, "prf", Some(false), 0, 0),
        mrow(3, None, 100, 1, "hsd", Some(true), 0, 0),
        mrow(4, None, 121, 2, "prf", Some(false), 0, 0),
    ]);
    assert_eq!(players(&rows, &group_matches(&rows, w, true)), vec![vec![4], vec![3], vec![1, 2]]);
    assert_eq!(players(&rows, &group_matches(&rows, w, false)), vec![vec![4]]);
    assert!(group_matches(&[], w, false).is_empty());
}

#[test]
fn public_match_winner_rules() {
    assert_eq!(pilot_result(&[1, 2, 3, 4, 5, 1, 1, 0, 0, 0]), Some("win"));
    assert_eq!(pilot_result(&[1, 2, 3, 4, 5, 0, 1, 0, 0, 0]), Some("loss"));
    assert_eq!(pilot_result(&[1, 2, 3, 4, 5, 0, 0, 0, 0, 0]), None, "no mission played");
    assert_eq!(pilot_result(&[50]), None, "no stats");
    assert_eq!(side_result(&[Some("win"), None, Some("win")]), Some("win"));
    assert_eq!(side_result(&[Some("win"), Some("loss")]), Some("mixed"));
    assert_eq!(side_result(&[None, None]), None);
    let (h, p, r) = (0usize, 1usize, 2usize);
    // The side whose pilots reported a win.
    assert_eq!(match_winner(&[(h, Some("loss")), (p, Some("win"))]), (Some("prf"), None));
    assert_eq!(match_winner(&[(h, Some("win")), (p, Some("loss")), (r, Some("loss"))]), (Some("hsd"), None));
    assert_eq!(match_winner(&[(h, Some("win")), (p, None)]), (Some("hsd"), None), "the loser sent no result");
    // Reports contradict: two winners, or one side disagrees with itself.
    assert_eq!(match_winner(&[(h, Some("win")), (p, Some("win"))]), (None, Some("conflict")));
    assert_eq!(match_winner(&[(h, Some("mixed")), (p, Some("loss"))]), (None, Some("conflict")));
    // Nobody won.
    assert_eq!(match_winner(&[(h, Some("loss")), (p, Some("loss"))]), (Some("draw"), None));
    // Only one side reported.
    assert_eq!(match_winner(&[(p, Some("win"))]), (Some("prf"), Some("one_side")));
    assert_eq!(match_winner(&[(p, Some("loss"))]), (None, Some("one_side")));
    assert_eq!(match_winner(&[(h, None), (p, None)]), (None, None));
}

#[test]
fn public_matches_twelve_pilots_in_one_battle() {
    let c = cfg();
    let mut snap = snapshot();
    // 6 against 6 in turn 3, first map (sector 6 with map_base 0), the twelve
    // reports spread over 78 s; pilots 1..6 are ranked (they have profiles).
    let mut rows = Vec::new();
    for k in 0..6u128 {
        let name = snap.standings[k as usize].name.clone();
        rows.push(mrow(k + 1, name.as_deref(), (k * 15) as i64, 0, "prf", Some(true), 55 - k as i64, 1));
        rows.push(mrow(20 + k, None, (k * 15 + 3) as i64, 0, "hsd", Some(false), 0, 0));
    }
    // 5 against 3 a quarter of an hour later in the second map.
    for k in 0..5u128 {
        rows.push(mrow(40 + k, None, 900 + k as i64, 1, "rb", Some(false), 10, 0));
    }
    for k in 0..3u128 {
        rows.push(mrow(50 + k, None, 905 + k as i64, 1, "hsd", Some(true), 100, 3));
    }
    snap.match_rows = newest_first(rows);
    let groups = group_matches(&snap.match_rows, chrono::Duration::seconds(MATCH_WINDOW_SECS), true);
    assert_eq!(groups.iter().map(Vec::len).collect::<Vec<_>>(), vec![8, 12]);

    let (doc, _) = build_all(&c, &snap, &mut Placeholders::new(&[7u8; 32], &c.season));
    let text = String::from_utf8(render(&doc).unwrap()).unwrap();
    let mut found = BTreeSet::new();
    keys(&doc, &mut found);
    let unknown: Vec<&String> = found.iter().filter(|k| !KEYS.contains(&k.as_str())).collect();
    assert!(unknown.is_empty(), "keys outside the whitelist: {unknown:?}");
    assert_no_ids(&text, &(1..=60u128).map(uid).collect::<Vec<_>>());

    let m = doc["matches"].as_array().unwrap();
    assert_eq!(m.len(), 2);
    let five_three = &m[0];
    assert_eq!(five_three["at"], "2026-11-26T20:15:00Z", "last report 20:15:07, to the minute");
    assert_eq!(five_three["sector"], 7);
    assert_eq!(five_three["winner"], "hsd");
    assert_eq!(five_three["note"], Value::Null);
    assert_eq!(five_three["sides"][0]["side"], "hsd", "the winner first");
    assert_eq!(five_three["sides"][0]["pilots"].as_array().unwrap().len(), 3);
    assert_eq!(five_three["sides"][0]["points"], 300);
    assert_eq!(five_three["sides"][0]["kills"], 9);
    assert_eq!(five_three["sides"][1]["side"], "rb");
    assert_eq!(five_three["sides"][1]["result"], "loss");
    assert_eq!(five_three["sides"][1]["pilots"].as_array().unwrap().len(), 5);

    let six = &m[1];
    assert_eq!(six["turn"], 3);
    assert_eq!(six["sector"], 6);
    assert_eq!(six["at"], "2026-11-26T20:01:00Z", "last report 20:01:18");
    assert_eq!(six["winner"], "prf");
    let prf = &six["sides"][0];
    assert_eq!(prf["side"], "prf");
    assert_eq!(prf["result"], "win");
    assert_eq!(prf["points"], 55 + 54 + 53 + 52 + 51 + 50);
    assert_eq!(prf["kills"], 6);
    let pilots = prf["pilots"].as_array().unwrap();
    assert_eq!(pilots.len(), 6);
    // Best first; ranked pilots carry their profile slug, the others none.
    assert_eq!(pilots[0]["points"], 55);
    assert_eq!(pilots[0]["pilot"], "Ace One");
    assert_eq!(pilots[5]["points"], 50);
    assert!(pilots.iter().all(|p| p["slug"].as_str().is_some_and(valid_slug)), "{pilots:?}");
    let hsd = &six["sides"][1];
    assert_eq!(hsd["result"], "loss");
    let hp = hsd["pilots"].as_array().unwrap();
    assert_eq!(hp.len(), 6);
    assert!(hp.iter().all(|p| p["pilot"].as_str().unwrap().starts_with("Pilot #") && p.get("slug").is_none()));
    // Every pilot once.
    let names: HashSet<&str> = six["sides"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|s| s["pilots"].as_array().unwrap().iter().map(|p| p["pilot"].as_str().unwrap()))
        .collect();
    assert_eq!(names.len(), 12);

    // `[public] matches` limits the list; battles[] stays as it was.
    let c1 = PublicConfig { matches: 1, ..cfg() };
    let doc1 = build(&c1, &snap, &mut Placeholders::new(&[7u8; 32], &c.season));
    assert_eq!(doc1["matches"].as_array().unwrap().len(), 1);
    assert_eq!(doc1["matches"][0]["sector"], 7);
    assert_eq!(doc1["battles"], doc["battles"]);

    // A contradiction: both sides report a win.
    snap.match_rows = newest_first(vec![
        mrow(1, Some("Ace One"), 0, 2, "hsd", Some(true), 50, 1),
        mrow(2, None, 4, 2, "prf", Some(true), 50, 1),
    ]);
    let doc = build(&c, &snap, &mut Placeholders::new(&[7u8; 32], &c.season));
    assert_eq!(doc["matches"][0]["winner"], Value::Null);
    assert_eq!(doc["matches"][0]["note"], "conflict");
    assert_eq!(doc["matches"][0]["sides"][0]["side"], "hsd", "no winner: game order");
}

#[test]
fn public_fit_matches_shortens_only_matches() {
    let list: Vec<String> = (0..40).map(|i| "x".repeat(100 + i)).collect();
    let mut doc = serde_json::json!({"battles": [1, 2, 3], "matches": list});
    let full = render(&doc).unwrap().len();
    fit_matches(&mut doc, full);
    assert_eq!(doc["matches"].as_array().unwrap().len(), 40, "fits: unchanged");
    fit_matches(&mut doc, 2000);
    let n = doc["matches"].as_array().unwrap().len();
    assert!(n > 0 && n < 40, "{n}");
    assert!(render(&doc).unwrap().len() <= 2000);
    assert_eq!(doc["matches"][0].as_str().unwrap().len(), 100, "the newest stay");
    assert_eq!(doc["battles"].as_array().unwrap().len(), 3);
    fit_matches(&mut doc, 10);
    assert!(doc["matches"].as_array().unwrap().is_empty());
}

fn md_has(turn: &Value, text: &str) -> bool {
    turn["discord_md"].as_str().is_some_and(|m| m.contains(text))
}

fn row(n: u128, turn: i16, f: &str, stats: [i64; 10]) -> TurnRow {
    TurnRow { player: uid(n), turn, faction: Some(f.to_owned()), stats }
}

/// Turn 3 of round 1: turns 1 and 2 are closed.
fn snapshot_with_turns() -> Snapshot {
    let mut snap = snapshot();
    // stats: hits, shots, kills, kill points, deaths, wins, missions, base captures, captures, tippings
    snap.turn_rows = vec![
        // Turn 1: Ace best accuracy (60 shots), Dee most kill points, Eve too few shots / missions.
        row(1, 1, "hsd", [30, 60, 4, 400, 1, 4, 5, 1, 0, 0]),
        row(4, 1, "hsd", [10, 50, 6, 600, 2, 1, 5, 0, 0, 0]),
        row(5, 1, "hsd", [40, 49, 9, 0, 0, 4, 4, 2, 0, 0]),
        row(2, 1, "prf", [5, 10, 1, 100, 3, 0, 1, 0, 0, 0]),
        // Turn 2: the unnamed PRF pilot wins on kill points; Ace once more.
        row(2, 2, "prf", [1, 2, 7, 700, 0, 1, 1, 0, 0, 0]),
        row(1, 2, "hsd", [1, 2, 3, 300, 0, 1, 1, 0, 0, 0]),
        // Turn 3 (running): no awards yet.
        row(1, 3, "hsd", [1, 1, 2, 200, 0, 1, 1, 0, 0, 0]),
        // A hidden player never reaches the snapshot (collect filters it); a
        // player outside the standings still gets a stable placeholder.
        row(9, 2, "rb", [0, 0, 1, 50, 0, 0, 1, 0, 0, 0]),
    ];
    let at = snap.now;
    snap.pilot_battles = (0..14)
        .map(|k| Battle {
            at: at - chrono::Duration::minutes(k),
            turn: if k < 4 { 3 } else { 1 },
            map: (k % 3) as i16,
            faction: "hsd".into(),
            player: uid(1),
            name: Some("Ace One".into()),
            points: vec![1, 2, 1, 10 + k, 0, 1, 1, 0, 0, 0],
            vt: None,
        })
        .collect();
    snap.sector_counts = vec![(uid(1), 1, 0, 3), (uid(1), 1, 1, 9), (uid(1), 3, 0, 9), (uid(1), 9, 0, 99)];
    snap.turn_battles = vec![(1, 21), (2, 8), (3, 4)];
    snap.turn_ends = vec![
        (1, Utc.with_ymd_and_hms(2026, 11, 16, 18, 0, 31).unwrap()),
        (2, Utc.with_ymd_and_hms(2026, 11, 23, 18, 0, 0).unwrap()),
        (3, Utc.with_ymd_and_hms(2026, 11, 30, 18, 0, 0).unwrap()),
    ];
    snap
}

#[test]
fn public_turn_awards_follow_the_game_rules() {
    // give_turn_award: accuracy needs 50 shots, win rate 5 missions, kill points
    // and base captures a value above 0; integer per mille; at most 10 places.
    let stats = [
        [30, 60, 0, 400, 0, 4, 5, 1, 0, 0],  // 0: 500 per mille, 800 win rate
        [10, 50, 0, 600, 0, 1, 5, 0, 0, 0],  // 1: 200, 200
        [40, 49, 0, 0, 0, 4, 4, 2, 0, 0],    // 2: too few shots and missions, no kill points
        [0, 50, 0, 0, 0, 0, 5, 0, 0, 0],     // 3: 0 accuracy and 0 win rate still rank (as in the SQL)
    ];
    let names = ["Ace", "Dee", "Eve", "Zed"];
    let places = turn_award_places(&stats, &names);
    assert_eq!(places[0], [0, 1, 3], "accuracy");
    assert_eq!(places[1], [1, 0], "kill points");
    assert_eq!(places[2], [0, 1, 3], "win rate");
    assert_eq!(places[3], [2, 0], "base captures");
    // Equal values: name order; never more than 10 places.
    let many: Vec<[i64; 10]> = (0..15).map(|_| [0, 0, 0, 5, 0, 0, 0, 0, 0, 0]).collect();
    let nm: Vec<String> = (0..15).map(|i| format!("P{:02}", 14 - i)).collect();
    let refs: Vec<&str> = nm.iter().map(String::as_str).collect();
    let p = turn_award_places(&many, &refs);
    assert_eq!(p[1].len(), AWARD_PLACES);
    assert_eq!(p[1][0], 14, "P00 first");
    assert_eq!(AWARD_CATEGORIES, ["accuracy", "kill_points", "win_rate", "base_captures"]);
}

#[test]
fn public_slugs_are_unique_valid_and_without_ids() {
    let list = vec![
        (1u32, "Foo Bar".to_owned(), false),
        (2, "foo bar".to_owned(), false),
        (3, "Pilot 1234".to_owned(), false),
        (4, "Pilot #1234".to_owned(), true),
        (5, "X".to_owned(), false),
        (6, "  ".to_owned(), true),
    ];
    let s = assign_slugs(&list);
    assert_eq!(s[&1], "foo-bar");
    assert_eq!(s[&2], "foo-bar-2");
    assert_eq!(s[&3], "pilot-1234", "a real name keeps its slug before a placeholder");
    assert_eq!(s[&4], "pilot-1234-2");
    assert_eq!(s[&5], "x");
    assert_eq!(s[&6], "pilot");
    let all: HashSet<&String> = s.values().collect();
    assert_eq!(all.len(), list.len(), "unique");
    assert!(s.values().all(|v| valid_slug(v)), "{s:?}");
    for bad in ["", "-a", "a-", "a--b", "A", "a_b", "a.json", "../x", &"a".repeat(41)] {
        assert!(!valid_slug(bad), "{bad:?}");
    }
    assert!(valid_slug(&"a".repeat(40)));
    assert_eq!(slug_base(&"Long Name ".repeat(9)).len() <= MAX_SLUG - 4, true);
}

#[test]
fn public_turn_reports_awards_and_pilot_files() {
    let c = PublicConfig { sector_names: (0..24).map(|i| format!("Name*{i}_[x](y)")).collect(), ..cfg() };
    let snap = snapshot_with_turns();
    let (doc, files) = build_all(&c, &snap, &mut Placeholders::new(&[7u8; 32], &c.season));
    let text = String::from_utf8(render(&doc).unwrap()).unwrap();
    let mut found = BTreeSet::new();
    keys(&doc, &mut found);
    let unknown: Vec<&String> = found.iter().filter(|k| !KEYS.contains(&k.as_str())).collect();
    assert!(unknown.is_empty(), "keys outside the whitelist: {unknown:?}");
    assert_no_ids(&text, &(1..=9u128).map(uid).collect::<Vec<_>>());

    // Slugs on every listed pilot, unique.
    let pilots = doc["pilots"].as_array().unwrap();
    let slugs: HashSet<&str> = pilots.iter().map(|p| p["slug"].as_str().unwrap()).collect();
    assert_eq!(slugs.len(), pilots.len());
    let ace = pilots.iter().find(|p| p["pilot"] == "Ace One").unwrap();
    assert_eq!(ace["slug"], "ace-one");

    // Awards: closed turns 1 and 2 only.
    assert_eq!(doc["awards"]["turns"], serde_json::json!([1, 2]));
    assert_eq!(doc["awards"]["per_turn"], 10);
    // Ace: turn 1 accuracy 1st, kill points 2nd, win rate 1st, base captures 2nd (Eve 1st); turn 2 kill points 2nd.
    assert_eq!(ace["awards"], serde_json::json!([[1, 0, 1], [1, 1, 2], [1, 2, 1], [1, 3, 2], [2, 1, 2]]));
    let dee = pilots.iter().find(|p| p["pilot"] == "Dee").unwrap();
    assert_eq!(dee["awards"], serde_json::json!([[1, 0, 2], [1, 1, 1], [1, 2, 2]]));
    let eve = pilots.iter().find(|p| p["pilot"] == "Eve").unwrap();
    assert_eq!(eve["awards"], serde_json::json!([[1, 3, 1]]), "too few shots and missions: base captures only");
    assert!(pilots.iter().find(|p| p["pilot"] == "Fay").unwrap().get("awards").is_none());

    // W528: battles per sector, summed over the pilots (turn 9 is outside the round).
    let sectors = doc["sectors"].as_array().unwrap();
    let per_sector: Vec<i64> = sectors.iter().map(|s| s["battles"].as_i64().unwrap()).collect();
    assert_eq!(&per_sector[..7], &[3, 9, 0, 0, 0, 0, 9]);
    assert_eq!(per_sector.iter().sum::<i64>(), 21);

    // Turns: one entry per turn so far.
    let turns = doc["turns"].as_array().unwrap();
    assert_eq!(turns.len(), 3);
    assert_eq!(turns[0]["state"], "closed");
    assert_eq!(turns[2]["state"], "contested");
    assert_eq!(turns[0]["ends_at"], "2026-11-16T18:00:00Z", "minute precision");
    assert_eq!(turns[0]["battles"], 21);
    // Sectors of turn 1 (map_base 0): maps 0 and 1 -> 500/100 and 100/400.
    assert_eq!(turns[0]["war_points"], serde_json::json!({"hsd": 600, "prf": 500, "rb": 0}));
    // Influence after turn 1: HSD and PRF won one sector each, 22 unplayed shared by two.
    assert_eq!(turns[0]["influence"], serde_json::json!({"hsd": 50, "prf": 50, "rb": 0}));
    // The running turn equals the side summary.
    for (i, side) in ["hsd", "prf", "rb"].iter().enumerate() {
        assert_eq!(turns[2]["influence"][side], doc["sides"][i]["influence"]);
    }
    assert_eq!(turns[0]["pilot"]["pilot"], "Dee");
    assert_eq!(turns[0]["pilot"]["points"], 600);
    assert_eq!(turns[0]["pilot"]["slug"], "dee");
    let p2 = turns[1]["pilot"]["pilot"].as_str().unwrap();
    assert!(p2.starts_with("Pilot #"), "{p2}");
    assert_eq!(turns[1]["pilot"]["side"], "prf");
    // Closest sector of turn 1: 100 vs 400 war points (gap 300; shares 37/63, margin 26) beats 500 vs 100 (gap 400).
    assert_eq!(turns[0]["closest"], serde_json::json!({"sector": 1, "leader": "prf", "gap": 300, "margin": 26}));
    assert!(md_has(&turns[0], "Closest sector: Name1xy, PRF ahead by 300 war points"));
    assert_eq!(turns[1]["closest"], Value::Null, "turn 2 has no played sector");
    assert_eq!(turns[2]["closest"], serde_json::json!({"sector": 6, "leader": null, "gap": 0, "margin": 1}), "a draw (the rounding rest goes to HSD, as in the game)");
    // Discord text only for closed turns, plain (no markup from names), short.
    let md = turns[0]["discord_md"].as_str().unwrap();
    assert!(md.starts_with("**Turn 1 report** · Season 1 · round 1\n"), "{md}");
    assert!(md.contains("- Name0xy: **HSD** (HSD 67% / PRF 33%)"), "{md}");
    assert!(md.contains("Pilot of the turn: **Dee** (HSD), 600 kill points"), "{md}");
    assert!(md.contains("War influence after the turn: HSD 50% · PRF 50%"), "{md}");
    assert!(md.contains("Battles: 21"));
    assert!(!md.contains("RB"), "RB is not in the war before turn 3: {md}");
    assert!(md.len() < 2000);
    assert!(turns[2].get("discord_md").is_none());

    // Pilot files: every listed pilot, slug = file name, keys whitelisted, no ids.
    assert_eq!(files.len(), pilots.len());
    let rendered = render_pilots(&files).unwrap();
    for (slug, bytes) in &rendered {
        assert!(valid_slug(slug));
        assert!(bytes.len() < MAX_PILOT_BYTES);
        let t = String::from_utf8(bytes.clone()).unwrap();
        assert_no_ids(&t, &(1..=9u128).map(uid).collect::<Vec<_>>());
        let v: Value = serde_json::from_str(&t).unwrap();
        let mut found = BTreeSet::new();
        keys(&v, &mut found);
        let unknown: Vec<&String> = found.iter().filter(|k| !PILOT_KEYS.contains(&k.as_str())).collect();
        assert!(unknown.is_empty(), "pilot keys outside the whitelist: {unknown:?}");
        assert_eq!(v["slug"], slug.as_str());
        assert_eq!(v["schema"], 1);
        assert_eq!(v["campaign"]["id"], "steelbound-s1");
    }
    let (_, ace_file) = files.iter().find(|(s, _)| s == "ace-one").unwrap();
    let h = ace_file["history"].as_array().unwrap();
    assert_eq!(h.iter().map(|x| x["turn"].as_i64().unwrap()).collect::<Vec<_>>(), [1, 2, 3]);
    assert_eq!(h[0]["points"], 400);
    assert_eq!(h[0]["side"], "hsd");
    assert_eq!(ace_file["battles"].as_array().unwrap().len(), PILOT_BATTLES);
    assert_eq!(ace_file["battles"][0]["at"], "2026-11-26T12:34:00Z");
    assert!(ace_file["battles"][0].get("pilot").is_none());
    // Favourite: 9 battles in sector 1 (turn 1) and sector 6 (turn 3) -> the later one; map 9 is no sector.
    assert_eq!(ace_file["favorite_sector"], serde_json::json!({"sector": 6, "battles": 9}));
    assert_eq!(ace_file["awards"][0], serde_json::json!({"turn": 1, "category": "accuracy", "rank": 1}));
    assert_eq!(ace_file["awards"].as_array().unwrap().len(), 5);
}

#[test]
fn public_pilot_files_sync() {
    let dir = std::env::temp_dir().join(format!("sbc-pilots-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let f = |s: &str, b: &str| (s.to_owned(), b.as_bytes().to_vec());
    assert_eq!(sync_pilot_files(&dir, &[f("ace", "1"), f("dee", "2")]).unwrap(), (2, 0));
    std::fs::write(dir.join("README.txt"), "not ours").unwrap();
    std::fs::write(dir.join("Bad_Name.json"), "not a slug").unwrap();
    std::fs::write(dir.join(".zed.json.tmp"), "left over").unwrap();
    let before = std::fs::metadata(dir.join("ace.json")).unwrap().modified().unwrap();
    std::thread::sleep(Duration::from_millis(20));
    // Ace unchanged (not rewritten), Dee gone, Eve new.
    assert_eq!(sync_pilot_files(&dir, &[f("ace", "1"), f("eve", "3")]).unwrap(), (1, 2));
    assert_eq!(std::fs::metadata(dir.join("ace.json")).unwrap().modified().unwrap(), before);
    assert!(!dir.join("dee.json").exists());
    assert!(!dir.join(".zed.json.tmp").exists());
    assert!(dir.join("README.txt").exists() && dir.join("Bad_Name.json").exists(), "foreign files stay");
    assert_eq!(std::fs::read(dir.join("eve.json")).unwrap(), b"3");
    assert!(render_pilots(&[("../x".into(), Value::Null)]).is_err(), "bad slug refused");
    let big = Value::String("x".repeat(MAX_PILOT_BYTES));
    assert!(render_pilots(&[("x".into(), big)]).is_err(), "size limit");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn public_write_is_atomic() {
    let dir = std::env::temp_dir().join(format!("sbc-public-{}", std::process::id()));
    let path = dir.join("steelbound-s1.json");
    write_atomic(&path, b"{\"a\":1}\n").unwrap();
    write_atomic(&path, b"{\"a\":2}\n").unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), b"{\"a\":2}\n");
    let left: Vec<_> = std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().file_name()).collect();
    assert_eq!(left.len(), 1, "no temporary file left: {left:?}");
    let key = dir.join("k");
    std::fs::write(&key, [9u8; 32]).unwrap();
    assert_eq!(load_or_create_key(&key).unwrap(), vec![9u8; 32]);
    std::fs::write(&key, [9u8; 4]).unwrap();
    assert!(load_or_create_key(&key).is_err(), "short key refused");
    let _ = std::fs::remove_dir_all(&dir);
}

// ---- Database ------------------------------------------------------------

static SEQ: AtomicU64 = AtomicU64::new(1);

fn fresh() -> u64 {
    let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos() as u64;
    (t ^ (std::process::id() as u64) << 20).wrapping_add(SEQ.fetch_add(1, Ordering::Relaxed) * 7919)
}

/// A synthetic player: account, core player, Steelbound identity, optional display name.
async fn player(pool: &sqlx::PgPool, faction: &str, name: Option<&str>, banned: bool, suspended: bool) -> (Uuid, Uuid) {
    let account = (fresh() % 4_000_000_000) as u32 | 1;
    let player = Uuid::from_u64_pair(0, 0x5342_4C01_0000_0000 | u64::from(account));
    sqlx::query("INSERT INTO accounts (xuid, xname, faction, banned) VALUES ($1, 'SbcPublicTest', $2::faction, $3)")
        .bind(player)
        .bind(faction)
        .bind(banned)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO core.player (player_id) VALUES ($1)").bind(player).execute(pool).await.unwrap();
    let identity: Uuid = sqlx::query_scalar(
        "INSERT INTO core.identity (provider_id, external_id, player_id, verified_by, suspended)
         VALUES ('steelbound', $1, $2, 'ticket', $3) RETURNING identity_id",
    )
    .bind(account.to_string())
    .bind(player)
    .bind(suspended)
    .fetch_one(pool)
    .await
    .unwrap();
    if let Some(n) = name {
        sqlx::query("INSERT INTO core.display_name (player_id, name, name_key, skeleton) VALUES ($1, $2, $3, $3)")
            .bind(player)
            .bind(n)
            .bind(n.to_lowercase())
            .execute(pool)
            .await
            .unwrap();
    }
    (player, identity)
}

#[allow(clippy::too_many_arguments)]
async fn result(
    pool: &sqlx::PgPool,
    who: (Uuid, Uuid),
    round: i16,
    turn: i16,
    map: i16,
    faction: &str,
    stats: [i64; 10],
    war: i64,
    annulled: bool,
) {
    // An annulment takes the result out of the sums (leaderboard, war tide); the ledger row stays, marked.
    let (lb, tide) = if annulled { ([0i64; 10], 0) } else { (stats, war) };
    sqlx::query(
        "INSERT INTO leaderboard (xuid, faction, round, turn, \"map\", stat1, stat2, stat3, stat4, stat5, stat6, stat7, stat8, stat9, stat10)
         VALUES ($1, $2::faction, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15)
         ON CONFLICT (xuid, faction, round, turn, \"map\") DO UPDATE SET stat4 = leaderboard.stat4 + excluded.stat4",
    )
    .bind(who.0)
    .bind(faction)
    .bind(round)
    .bind(turn)
    .bind(map)
    .bind(lb[0] as i32)
    .bind(lb[1] as i32)
    .bind(lb[2] as i32)
    .bind(lb[3] as i32)
    .bind(lb[4] as i32)
    .bind(lb[5] as i32)
    .bind(lb[6] as i32)
    .bind(lb[7] as i32)
    .bind(lb[8] as i32)
    .bind(lb[9] as i32)
    .execute(pool)
    .await
    .unwrap();
    let col = match faction {
        "hsd" => "hsd",
        "prf" => "prf",
        _ => "rb",
    };
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "INSERT INTO wartide (round, \"map\", {col}) VALUES ($1, $2, $3)
         ON CONFLICT (round, \"map\") DO UPDATE SET {col} = wartide.{col} + excluded.{col}"
    )))
    .bind(round)
    .bind(map + (turn - 1) * 3)
    .bind(tide as i32)
    .execute(pool)
    .await
    .unwrap();
    let annulled_at = if annulled { Some(Utc::now()) } else { None };
    for (kind, points) in [("user_points", stats.to_vec()), ("mission_points", vec![war])] {
        sqlx::query(
            "INSERT INTO gateway_result (identity_id, player_id, kind, round, turn, \"map\", faction, points, annulled_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7::faction, $8, $9)",
        )
        .bind(who.1)
        .bind(who.0)
        .bind(kind)
        .bind(round)
        .bind(turn)
        .bind(map)
        .bind(faction)
        .bind(&points)
        .bind(annulled_at)
        .execute(pool)
        .await
        .unwrap();
    }
}

#[tokio::test]
async fn db_public_export_hides_banned_suspended_and_annulled() {
    let Some(pool) = test_pool().await else { return };
    // A round of its own, so other tests on the same database do not matter.
    let round = 2000 + (fresh() % 28000) as i16;
    let tag = fresh() % 100_000;
    let ace = format!("Pub Ace {tag}");
    let ban = format!("Pub Ban {tag}");
    let sus = format!("Pub Sus {tag}");
    let ann = format!("Pub Ann {tag}");
    let a = player(&pool, "hsd", Some(&ace), false, false).await;
    let n = player(&pool, "prf", None, false, false).await;
    let b = player(&pool, "hsd", Some(&ban), true, false).await;
    let s = player(&pool, "prf", Some(&sus), false, true).await;
    let x = player(&pool, "rb", Some(&ann), false, false).await;

    let st = |kp: i64, win: i64| [10, 20, 3, kp, 1, win, 1, 0, 0, 0];
    result(&pool, a, round, 1, 0, "hsd", st(500, 1), 200, false).await;
    result(&pool, n, round, 1, 1, "prf", st(300, 0), 100, false).await;
    result(&pool, b, round, 1, 0, "hsd", st(9000, 1), 900, false).await;
    result(&pool, s, round, 1, 1, "prf", st(8000, 1), 800, false).await;
    // Annulled ledger rows: the annulment already took them out of the sums.
    result(&pool, x, round, 1, 2, "rb", st(400, 1), 400, true).await;

    // A second result of the ace in the current turn of the campaign.
    let now_turn: i16 = sqlx::query_scalar("SELECT turn FROM settings ORDER BY round DESC, turn DESC LIMIT 1")
        .fetch_one(&pool)
        .await
        .unwrap();
    result(&pool, a, round, now_turn, 2, "hsd", st(50, 0), 20, false).await;

    let c = PublicConfig { leaderboard_size: 50, battles: 50, ..cfg() };
    let snap = collect(&pool, c.battles, Some(round)).await.expect("collect");
    // Turn sums: only rows of the current turn (from `settings`) count.
    let turn = snap.period.turn;
    assert_eq!(turn, now_turn);
    let ace_turn = snap.standings.iter().find(|s| s.player == a.0).unwrap().turn_stats;
    let mut want = st(50, 0);
    if turn == 1 {
        for (w, v) in want.iter_mut().zip(st(500, 1)) {
            *w += v;
        }
    }
    assert_eq!(ace_turn, want, "turn {turn}");
    let unnamed_turn = snap.standings.iter().find(|s| s.player == n.0).unwrap().turn_stats;
    assert_eq!(unnamed_turn, if turn == 1 { st(300, 0) } else { [0; 10] });
    let doc = build(&c, &snap, &mut Placeholders::new(&[3u8; 32], "test"));
    let text = String::from_utf8(render(&doc).unwrap()).unwrap();

    assert_no_ids(&text, &[a.0, a.1, n.0, n.1, b.0, b.1, s.0, s.1, x.0, x.1]);
    assert!(text.contains(&ace));
    for hidden in [&ban, &sus, &ann] {
        assert!(!text.contains(hidden.as_str()), "{hidden} must not appear");
    }
    let overall = doc["leaderboards"]["overall"].as_array().unwrap();
    assert_eq!(overall.len(), 2, "ace and the unnamed pilot; the annulled pilot has nothing left: {overall:?}");
    assert_eq!(overall[0]["pilot"], ace.as_str());
    assert_eq!(overall[0]["points"], 550);
    assert_eq!(overall[0]["hits"], 20);
    assert_eq!(overall[0]["shots"], 40);
    assert!(overall[1]["pilot"].as_str().unwrap().starts_with("Pilot #"));
    let pilots = doc["pilots"].as_array().unwrap();
    assert_eq!(pilots.len(), 2, "the full list has the same pilots: {pilots:?}");
    assert_eq!(pilots[0]["round"]["points"], 550);
    assert_eq!(pilots[0]["turn"]["points"], if turn == 1 { 550 } else { 50 });
    let battles = doc["battles"].as_array().unwrap();
    assert_eq!(battles.len(), 3, "banned, suspended and annulled results are left out: {battles:?}");
    assert_eq!(doc["totals"]["battles_round"], 3);
    // The war tide still holds the hidden players' points (sums of the game); the sector leaders follow it.
    assert_eq!(doc["sectors"][0]["points"]["hsd"], 1100);

    // W527: per-turn sums, pilot battles and battle counts follow the same rules.
    let seen: HashSet<Uuid> = snap.turn_rows.iter().map(|r| r.player).collect();
    assert_eq!(seen, HashSet::from([a.0, n.0]), "turn sums: no banned, suspended or fully annulled pilot");
    let ace_t1 = snap.turn_rows.iter().find(|r| r.player == a.0 && r.turn == 1).unwrap();
    assert_eq!(ace_t1.faction.as_deref(), Some("hsd"));
    assert_eq!(ace_t1.stats[3], if turn == 1 { 550 } else { 500 });
    let pb: HashSet<Uuid> = snap.pilot_battles.iter().map(|b| b.player).collect();
    assert_eq!(pb, HashSet::from([a.0, n.0]));
    assert_eq!(snap.pilot_battles.iter().filter(|b| b.player == a.0).count(), 2);
    let t1 = snap.turn_battles.iter().find(|(t, _)| *t == 1).unwrap().1;
    assert_eq!(t1, if turn == 1 { 3 } else { 2 });
    let fav: i64 = snap.sector_counts.iter().filter(|c| c.0 == a.0).map(|c| c.3).sum();
    assert_eq!(fav, 2);
    let (_, files) = build_all(&c, &snap, &mut Placeholders::new(&[3u8; 32], "test"));
    assert_eq!(files.len(), 2);
    for (_, f) in &files {
        let t = serde_json::to_string(f).unwrap();
        assert_no_ids(&t, &[a.0, a.1, n.0, n.1, b.0, b.1, s.0, s.1, x.0, x.1]);
        for hidden in [&ban, &sus, &ann] {
            assert!(!t.contains(hidden.as_str()));
        }
    }
    // W529: matches come from the same filtered results: no banned, suspended or
    // annulled report; the ace's sector-0 match (the banned pilot also reported
    // there) shows the ace alone, as a one-sided report.
    assert!(snap.match_rows_complete);
    let mr: HashSet<Uuid> = snap.match_rows.iter().map(|b| b.player).collect();
    assert_eq!(mr, HashSet::from([a.0, n.0]));
    let matches = doc["matches"].as_array().unwrap();
    assert_eq!(matches.len(), 3, "three sectors, three matches: {matches:?}");
    for m in matches {
        assert_eq!(m["note"], "one_side", "{m}");
        assert_eq!(m["sides"].as_array().unwrap().len(), 1);
    }
    let m0 = matches.iter().find(|m| m["turn"] == 1 && m["sector"] == 0).unwrap();
    assert_eq!(m0["winner"], "hsd");
    assert_eq!(m0["sides"][0]["pilots"].as_array().unwrap().len(), 1);
    assert_eq!(m0["sides"][0]["pilots"][0]["pilot"], ace.as_str());
    let m1 = matches.iter().find(|m| m["turn"] == 1 && m["sector"] == 1).unwrap();
    assert_eq!(m1["winner"], Value::Null, "one side lost, the other sent nothing");
    let ace_file = &files.iter().find(|(_, f)| f["pilot"] == ace.as_str()).unwrap().1;
    assert_eq!(ace_file["battles"].as_array().unwrap().len(), 2);
    assert_eq!(ace_file["history"][0]["turn"], 1);
    // Read only: the export never writes.
    let mut tx = pool.begin().await.unwrap();
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY").execute(&mut *tx).await.unwrap();
    let write = sqlx::query("INSERT INTO wartide (round, \"map\") VALUES (32000, 0)").execute(&mut *tx).await;
    assert!(write.is_err(), "a read-only transaction refuses writes");
}

// ---- W531: VTs ------------------------------------------------------------------

/// `mrow` with a VT type (a won battle, 10 kill points per kill).
fn vrow(n: u128, secs: i64, map: i16, f: &str, kills: i64, vt: Option<i16>) -> Battle {
    Battle { vt, ..mrow(n, None, secs, map, f, Some(true), 10 * kills, kills) }
}

#[test]
fn public_killed_by_type_only_where_clear() {
    let rows = vec![
        // Match A (map 0): HSD 1 with a Vitzh against two PRF Deciders.
        vrow(1, 0, 0, "hsd", 2, Some(0)),
        vrow(2, 10, 0, "prf", 1, Some(7)),
        vrow(3, 20, 0, "prf", 0, Some(7)),
        // Match B (map 1): the other side drove two types -> nothing by type.
        vrow(1, 0, 1, "hsd", 3, Some(0)),
        vrow(2, 10, 1, "prf", 1, Some(5)),
        vrow(3, 20, 1, "prf", 1, Some(7)),
        // Match C (map 2): one opponent without a known VT -> nothing by type for HSD.
        vrow(1, 0, 2, "hsd", 4, Some(2)),
        vrow(2, 10, 2, "prf", 1, Some(7)),
        vrow(4, 20, 2, "prf", 1, None),
        // Match D (map 3): three sides; HSD meets only Deciders, RB meets a Vortex and a Decider.
        vrow(1, 0, 3, "hsd", 1, Some(2)),
        vrow(2, 10, 3, "prf", 1, Some(7)),
        vrow(5, 20, 3, "rb", 5, Some(7)),
        // Match E (map 4): nobody on another side reported -> nothing.
        vrow(1, 0, 4, "hsd", 6, Some(0)),
    ];
    let groups = group_matches(&rows, chrono::Duration::seconds(MATCH_WINDOW_SECS), true);
    assert_eq!(groups.len(), 5);
    let k = killed_by_type(&rows, &groups);
    let get = |n: u128| {
        let mut v: Vec<(i16, i64)> = k.get(&uid(n)).map(|m| m.iter().map(|(&t, &c)| (t, c)).collect()).unwrap_or_default();
        v.sort();
        v
    };
    assert_eq!(get(1), vec![(7, 2 + 1)], "A and D only");
    // PRF 2: A (opponent Vitzh) 1, B (Vitzh) 1, C (Vortex) 1, D (HSD Vortex + RB Decider: two types) nothing.
    assert_eq!(get(2), vec![(0, 2), (2, 1)]);
    assert_eq!(get(3), vec![(0, 1)], "no kills in A, one in B");
    assert_eq!(get(4), vec![(2, 1)], "the own VT does not matter");
    assert_eq!(get(5), Vec::<(i16, i64)>::new(), "RB met two types");
}

#[test]
fn public_vts_in_the_documents() {
    let c = cfg();
    let mut snap = snapshot_with_turns();
    snap.vt_names = [(0i16, "Vitzh".to_owned()), (7, "Decider".to_owned()), (2, "  Vortex With A Very Long Name  ".to_owned())]
        .into_iter()
        .collect();
    snap.vt_since = Some(Utc.with_ymd_and_hms(2026, 11, 28, 9, 30, 0).unwrap());
    // Battles: one with a known VT, one with a type without a shop name, one without VT.
    snap.battles[0].vt = Some(0);
    snap.battles[1].vt = Some(42);
    snap.pilot_battles[0].vt = Some(2);
    // Ace One (uid 1): Vitzh 3 battles, Vortex 3 battles but fewer wins -> favourite Vitzh.
    snap.pilot_vts = vec![(uid(1), 2, 3, 2, 1, 4, 300), (uid(1), 0, 3, 1, 2, 5, 500), (uid(2), 7, 1, 1, 0, 0, 0)];
    snap.side_vts = vec![
        ("hsd".into(), 0, 3),
        ("hsd".into(), 2, 3),
        ("hsd".into(), 1, 1),
        ("hsd".into(), 3, 9),
        ("prf".into(), 7, 1),
    ];
    snap.vt_rows = vec![vrow(1, 0, 0, "hsd", 2, Some(0)), vrow(2, 10, 0, "prf", 0, Some(7))];
    snap.match_rows = snap.vt_rows.clone();
    let (doc, files) = build_all(&c, &snap, &mut Placeholders::new(&[7u8; 32], &c.season));

    assert_eq!(doc["vts_since"], "2026-11-28");
    assert_eq!(doc["battles"][0]["vt"], "Vitzh");
    assert_eq!(doc["battles"][1]["vt"], "VT 42", "a type without a shop name");
    assert_eq!(doc["battles"][2]["vt"], Value::Null, "no VT known");
    let hsd = &doc["sides"][0];
    assert_eq!(hsd["id"], "hsd");
    // Most battles first, ties by name, at most three; type 3 has no name.
    assert_eq!(
        hsd["top_vts"],
        serde_json::json!([{"vt": "VT 3", "battles": 9}, {"vt": "Vitzh", "battles": 3}, {"vt": "Vortex With A Very L", "battles": 3}])
    );
    assert_eq!(doc["sides"][2]["top_vts"], serde_json::json!([]));
    let m = &doc["matches"][0];
    let vts: Vec<&Value> = m["sides"].as_array().unwrap().iter().map(|s| &s["pilots"][0]["vt"]).collect();
    assert_eq!(vts, vec!["Vitzh", "Decider"]);

    let ace = &files.iter().find(|(_, f)| f["pilot"] == "Ace One").unwrap().1;
    assert_eq!(ace["favorite_vt"], serde_json::json!({"vt": "Vitzh", "battles": 3}));
    assert_eq!(
        ace["vts"],
        serde_json::json!([
            {"vt": "Vitzh", "driven": 3, "lost": 1, "wins": 2, "kills": 5, "points": 500},
            {"vt": "Vortex With A Very L", "driven": 3, "lost": 2, "wins": 1, "kills": 4, "points": 300},
        ])
    );
    assert_eq!(ace["killed_by_type"], serde_json::json!([{"vt": "Decider", "kills": 2}]));
    assert_eq!(ace["battles"][0]["vt"], "Vortex With A Very L");
    assert_eq!(ace["battles"][1]["vt"], Value::Null);
    // A pilot without VT data: empty lists, no favourite.
    let other = &files.iter().find(|(_, f)| f["pilot"] == "Dee").unwrap().1;
    assert_eq!((other["vts"].clone(), other["favorite_vt"].clone()), (serde_json::json!([]), Value::Null));
    assert_eq!(other["killed_by_type"], serde_json::json!([]));
    // Without any VT data (a season before the update) the fields are there and empty.
    let (doc, _) = build_all(&c, &snapshot(), &mut Placeholders::new(&[7u8; 32], &c.season));
    assert_eq!(doc["vts_since"], Value::Null);
    assert_eq!(doc["battles"][0]["vt"], Value::Null);
}

/// The VT statistics from the database: own VTs exact, hidden pilots never.
#[tokio::test]
async fn db_public_vts() {
    let Some(pool) = test_pool().await else { return };
    let round = 2000 + (fresh() % 28000) as i16;
    let tag = fresh() % 100_000;
    let ace = format!("Vt Ace {tag}");
    let bob_name = format!("Vt Bob {tag}");
    let a = player(&pool, "hsd", Some(&ace), false, false).await;
    let b = player(&pool, "prf", Some(&bob_name), false, false).await;
    let c2 = player(&pool, "prf", Some(&format!("Vt Cat {tag}")), false, false).await;
    let ban = player(&pool, "prf", Some(&format!("Vt Ban {tag}")), true, false).await;
    // The newest user_points result of a player gets a VT.
    let vt = |who: (Uuid, Uuid), faction: &'static str, t: i16| {
        let pool = pool.clone();
        async move {
            sqlx::query(
                "INSERT INTO battle_vt (identity_id, player_id, faction, vt_type, vt_types, result_id)
                 SELECT $1, $2, $3::faction, $4, ARRAY[$4], max(result_id) FROM gateway_result
                 WHERE player_id = $2 AND kind = 'user_points'",
            )
            .bind(who.1)
            .bind(who.0)
            .bind(faction)
            .bind(t)
            .execute(&pool)
            .await
            .unwrap();
        }
    };
    let st = |kills: i64, deaths: i64, win: i64| [10, 20, kills, 100 * kills, deaths, win, 1, 0, 0, 0];
    // Sector 0: ace (Vitzh) against two Deciders; sector 1: ace (Vitzh) against Falchion + Decider;
    // sector 2: ace (Vortex) against bob without VT and the banned pilot (Falchion).
    result(&pool, a, round, 1, 0, "hsd", st(2, 1, 1), 10, false).await;
    vt(a, "hsd", 0).await;
    result(&pool, b, round, 1, 0, "prf", st(1, 1, 0), 10, false).await;
    vt(b, "prf", 7).await;
    result(&pool, c2, round, 1, 0, "prf", st(0, 2, 0), 10, false).await;
    vt(c2, "prf", 7).await;
    result(&pool, a, round, 1, 1, "hsd", st(3, 0, 1), 10, false).await;
    vt(a, "hsd", 0).await;
    result(&pool, b, round, 1, 1, "prf", st(0, 1, 0), 10, false).await;
    vt(b, "prf", 5).await;
    result(&pool, c2, round, 1, 1, "prf", st(0, 1, 0), 10, false).await;
    vt(c2, "prf", 7).await;
    result(&pool, a, round, 1, 2, "hsd", st(1, 1, 0), 10, false).await;
    vt(a, "hsd", 2).await;
    result(&pool, b, round, 1, 2, "prf", st(1, 0, 1), 10, false).await;
    result(&pool, ban, round, 1, 2, "prf", st(9, 0, 1), 10, false).await;
    vt(ban, "prf", 5).await;

    let c = PublicConfig { leaderboard_size: 50, battles: 50, ..cfg() };
    let snap = collect(&pool, c.battles, Some(round)).await.expect("collect");
    assert!(snap.vt_since.is_some());
    let mut mine: Vec<_> = snap.pilot_vts.iter().filter(|r| r.0 == a.0).map(|r| (r.1, r.2, r.3, r.4, r.5, r.6)).collect();
    mine.sort();
    assert_eq!(mine, vec![(0, 2, 1, 2, 5, 500), (2, 1, 1, 0, 1, 100)]);
    assert!(snap.pilot_vts.iter().all(|r| r.0 != ban.0), "hidden pilots have no VT statistics");
    let (doc, files) = build_all(&c, &snap, &mut Placeholders::new(&[3u8; 32], "test"));
    let side = |i: usize| doc["sides"][i]["top_vts"].clone();
    assert_eq!(side(0), serde_json::json!([{"vt": "Vitzh", "battles": 2}, {"vt": "Vortex", "battles": 1}]));
    assert_eq!(side(1), serde_json::json!([{"vt": "Decider", "battles": 3}, {"vt": "Falchion", "battles": 1}]));
    let file = &files.iter().find(|(_, f)| f["pilot"] == ace.as_str()).unwrap().1;
    assert_eq!(file["favorite_vt"], serde_json::json!({"vt": "Vitzh", "battles": 2}));
    assert_eq!(file["vts"][0], serde_json::json!({"vt": "Vitzh", "driven": 2, "lost": 1, "wins": 2, "kills": 5, "points": 500}));
    // Only sector 0 is clear (two Deciders); sector 1 has two types, sector 2 an unknown VT.
    assert_eq!(file["killed_by_type"], serde_json::json!([{"vt": "Decider", "kills": 2}]));
    let bob = &files.iter().find(|(_, f)| f["pilot"] == bob_name.as_str()).unwrap().1;
    assert_eq!(bob["killed_by_type"], serde_json::json!([{"vt": "Vitzh", "kills": 1}, {"vt": "Vortex", "kills": 1}]));
    let text = String::from_utf8(render(&doc).unwrap()).unwrap();
    assert!(!text.contains("Vt Ban"));
    let newest = doc["battles"].as_array().unwrap();
    assert!(newest.iter().any(|b| b["pilot"] == ace.as_str() && b["vt"] == "Vortex"));
    assert!(newest.iter().any(|b| b["pilot"] == bob_name.as_str() && b["vt"].is_null()), "bob's last battle has no VT");
}
