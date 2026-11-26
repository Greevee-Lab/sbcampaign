// SPDX-License-Identifier: AGPL-3.0-only
//! Tests of the public campaign data (`public.rs`). Synthetic data only.

use super::public::*;
use super::tests::test_pool;
use chrono::{TimeZone, Utc};
use serde_json::Value;
use std::{
    collections::{BTreeSet, HashMap},
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
fn public_document_size_is_bounded() {
    let c = PublicConfig { leaderboard_size: 100, battles: 100, ..cfg() };
    let mut snap = snapshot();
    snap.standings = (0..5000u128)
        .map(|n| Standing { player: uid(n), name: None, faction: Some("hsd".into()), stats: [n as i64; 10] })
        .collect();
    let doc = build(&c, &snap, &mut Placeholders::new(&[1u8; 32], ""));
    let bytes = render(&doc).unwrap();
    assert!(bytes.len() < MAX_BYTES / 2, "{} bytes", bytes.len());
    let huge = Value::String("x".repeat(MAX_BYTES));
    assert!(render(&huge).is_err());
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

    let c = PublicConfig { leaderboard_size: 50, battles: 50, ..cfg() };
    let snap = collect(&pool, c.battles, Some(round)).await.expect("collect");
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
    assert_eq!(overall[0]["points"], 500);
    assert!(overall[1]["pilot"].as_str().unwrap().starts_with("Pilot #"));
    let battles = doc["battles"].as_array().unwrap();
    assert_eq!(battles.len(), 2, "banned, suspended and annulled results are left out: {battles:?}");
    assert_eq!(doc["totals"]["battles_round"], 2);
    // The war tide still holds the hidden players' points (sums of the game); the sector leaders follow it.
    assert_eq!(doc["sectors"][0]["points"]["hsd"], 1100);
    // Read only: the export never writes.
    let mut tx = pool.begin().await.unwrap();
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY").execute(&mut *tx).await.unwrap();
    let write = sqlx::query("INSERT INTO wartide (round, \"map\") VALUES (32000, 0)").execute(&mut *tx).await;
    assert!(write.is_err(), "a read-only transaction refuses writes");
}
