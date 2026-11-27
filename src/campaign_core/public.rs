// SPDX-License-Identifier: AGPL-3.0-only
//! Public campaign data: a read-only snapshot of one campaign as JSON.
//!
//! `sbcampaign <config.toml> --public-export` (run by the
//! `steelcore-public` timer) writes `<out_dir>/<campaign>.json`: period
//! (round, turn, end of the turn), war tide per sector, side summaries,
//! rankings, the latest battles. steelnet serves the file unchanged on a
//! loopback listener (`--public-api`, `GET /v1/campaigns/<campaign>.json`),
//! the host's TLS proxy publishes it as `api.core.steelbound.dev`, and the
//! steelcore web page (core.steelbound.dev) loads it in the browser. On Xbox
//! Live the same campaign data was public for everyone.
//!
//! **Privacy rules** (tested in `public_tests.rs`):
//! - Players appear with their display name (`core.display_name`) only.
//!   Without one they get a neutral placeholder, `Pilot #NNNN`, a keyed hash
//!   of the player per season (`#` is not allowed in display names, so a
//!   placeholder can never be taken for a real name).
//! - Never: XUIDs, player ids, identity ids, provider ids, account names,
//!   pilot data, addresses.
//! - Banned accounts, suspended identities and annulled results are left out.
//! - One `REPEATABLE READ READ ONLY` transaction: a consistent snapshot that
//!   cannot write.
//!
//! The JSON schema is versioned (`"schema": 1`); fields are only added, a
//! change of meaning needs schema 2.

use chrono::{DateTime, SecondsFormat, Utc};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::{PgPool, Postgres, Transaction};
use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    time::Duration,
};
use uuid::Uuid;

/// Version of the JSON layout.
pub const SCHEMA: u32 = 1;
/// The sides of the war tide, in the order of the game.
pub const SIDES: [&str; 3] = ["hsd", "prf", "rb"];
/// Turns per round (upstream `TURNS_PER_ROUND`).
pub const TURNS_PER_ROUND: i16 = 8;
/// Sectors (maps) per turn: the war tide stores `map + (turn - 1) * 3`.
pub const SECTORS_PER_TURN: i16 = 3;
/// Sectors per round (upstream `MAP_COUNT`).
pub const SECTOR_COUNT: i16 = TURNS_PER_ROUND * SECTORS_PER_TURN;
/// First turn in which RB takes part (upstream `RB_START_TURN`).
pub const RB_START_TURN: i16 = 3;
/// First map number of a turn as the game sends it (1..=3 per turn).
pub const DEFAULT_MAP_BASE: i16 = 1;
/// Starting points per side in a sector share (upstream `war_tide`).
const STARTING_POINTS: f64 = 300.0;
/// Hard size limit of the file; a larger result is an error, never truncated JSON.
pub const MAX_BYTES: usize = 256 * 1024;
/// Upper bounds for the configurable list lengths.
const MAX_LEADERBOARD: usize = 100;
const MAX_BATTLES: usize = 100;
/// W529: upper bound of `[public] matches` (grouped battles, `matches[]`).
const MAX_MATCHES: usize = 50;
/// W529: pilot results read for the grouping (newest first). A match has up to
/// 12 pilots; 1 200 rows hold 50 full matches twice over.
pub const MATCH_ROWS: usize = 1200;
/// W529: the reports of one match arrive within this many seconds of its first
/// report (see `group_matches`).
pub const MATCH_WINDOW_SECS: i64 = 120;
/// W529: `matches[]` gives way (oldest first) when the document would grow past this.
pub const MATCH_FIT_BYTES: usize = MAX_BYTES * 95 / 100;
/// W531: results read for `killed_by_type` (only since the VT recording began).
pub const VT_ROWS: usize = 20_000;
/// W531: VT names longer than this are cut (the longest in the catalog has 17).
const MAX_VT_CHARS: usize = 20;
/// W531: most driven VTs per side.
const TOP_VTS: usize = 3;
/// Upper bound of `[public] ranking_pilots` (the full ranking list, `pilots`).
const MAX_PILOTS: usize = 250;
/// Longest display name that is shown (the name rules allow 15 characters).
const MAX_NAME_CHARS: usize = 16;
/// Latest battles in one pilot file.
pub const PILOT_BATTLES: usize = 10;
/// Longest pilot slug (`[a-z0-9]+(-[a-z0-9]+)*`, the same rule as steelnet).
pub const MAX_SLUG: usize = 40;
/// Hard size limit of one pilot file.
pub const MAX_PILOT_BYTES: usize = 64 * 1024;
/// Award categories in the order of the `awards` table (`rank1..rank4`,
/// upstream `give_turn_award.sql`): 1 weapon accuracy (stat1/stat2, at least
/// 50 shots in the turn), 2 kill points (stat4 > 0; the game files it under
/// "kills" when it shows the award), 3 mission victory rate (stat6/stat7, at
/// least 5 missions in the turn), 4 base captures (stat8 > 0).
pub const AWARD_CATEGORIES: [&str; 4] = ["accuracy", "kill_points", "win_rate", "base_captures"];
/// Places per category and turn (`give_turn_award(..., award_limit = 10)`).
pub const AWARD_PLACES: usize = 10;
const AWARD_MIN_SHOTS: i64 = 50;
const AWARD_MIN_MISSIONS: i64 = 5;

/// `[public]` section of the instance configuration.
#[derive(Debug, Clone)]
pub struct PublicConfig {
    /// Campaign id, `[a-z0-9-]{1,32}`; default `[ticket] campaign`.
    pub campaign: String,
    pub name: String,
    pub season: String,
    pub out_dir: PathBuf,
    /// Secret key file for the placeholder numbers (created if missing).
    pub name_key: PathBuf,
    pub leaderboard_size: usize,
    /// Most pilots in the full ranking list (`pilots`); default 200.
    pub ranking_pilots: usize,
    pub battles: usize,
    /// W529: newest grouped battles (`matches[]`); default 30.
    pub matches: usize,
    /// Value the game uses for the first map of a turn (0 or 1). The game
    /// sends 1..=3 (live data 26.11.2026: maps 1, 2 and 3 in turn 1), so the
    /// default is 1.
    pub map_base: i16,
    pub sector_names: Vec<String>,
    pub side_names: [String; 3],
    /// Length of one turn (`[campaign] turn_minutes`, default one week).
    pub turn_length: Duration,
}

/// `[a-z0-9-]{1,32}`: the same rule as the steelnet endpoint.
pub fn valid_campaign_id(id: &str) -> bool {
    (1..=32).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

fn text(t: &toml::Table, key: &str, max: usize) -> Result<Option<String>, String> {
    match t.get(key) {
        None => Ok(None),
        Some(v) => {
            let s = v.as_str().ok_or(format!("public.{key} must be a string"))?;
            if s.chars().count() > max || s.chars().any(char::is_control) {
                return Err(format!("public.{key}: at most {max} characters, no control characters"));
            }
            Ok(Some(s.to_owned()))
        }
    }
}

fn count(t: &toml::Table, key: &str, default: usize, max: usize) -> Result<usize, String> {
    match t.get(key) {
        None => Ok(default),
        Some(v) => v
            .as_integer()
            .and_then(|i| usize::try_from(i).ok())
            .filter(|&n| (1..=max).contains(&n))
            .ok_or(format!("public.{key} must be 1..={max}")),
    }
}

impl PublicConfig {
    pub fn from_toml(root: &toml::Table) -> Result<Self, String> {
        let empty = toml::Table::new();
        let p = root.get("public").and_then(|v| v.as_table()).unwrap_or(&empty);
        let ticket_campaign = root.get("ticket").and_then(|t| t.get("campaign")).and_then(|v| v.as_str());
        let campaign = match text(p, "campaign", 32)? {
            Some(c) => c,
            None => ticket_campaign.ok_or("public.campaign (or ticket.campaign) is missing")?.to_owned(),
        };
        if !valid_campaign_id(&campaign) {
            return Err(format!("campaign id {campaign:?} must match [a-z0-9-]{{1,32}}"));
        }
        let sector_names = match p.get("sector_names") {
            None => Vec::new(),
            Some(v) => {
                let list = v.as_array().ok_or("public.sector_names must be a list")?;
                if list.len() != SECTOR_COUNT as usize {
                    return Err(format!("public.sector_names needs exactly {SECTOR_COUNT} names"));
                }
                list.iter()
                    .map(|n| {
                        n.as_str()
                            .filter(|s| (1..=40).contains(&s.chars().count()) && !s.chars().any(char::is_control))
                            .map(str::to_owned)
                            .ok_or("public.sector_names: 1..=40 characters each".to_owned())
                    })
                    .collect::<Result<_, _>>()?
            }
        };
        let mut side_names = SIDES.map(|s| s.to_uppercase());
        if let Some(v) = p.get("side_names") {
            let t = v.as_table().ok_or("public.side_names must be a table")?;
            for (i, side) in SIDES.iter().enumerate() {
                if let Some(n) = text(t, side, 32)? {
                    side_names[i] = n;
                }
            }
        }
        let map_base = match p.get("map_base") {
            None => DEFAULT_MAP_BASE,
            Some(v) => match v.as_integer() {
                Some(0) => 0,
                Some(1) => 1,
                _ => return Err("public.map_base must be 0 or 1".into()),
            },
        };
        let turn_minutes = root.get("campaign").and_then(|c| c.as_table()).and_then(|c| {
            c.get("turn_minutes").and_then(|v| v.as_integer()).and_then(|i| u32::try_from(i).ok())
        });
        Ok(Self {
            name: text(p, "name", 60)?.unwrap_or_else(|| campaign.clone()),
            season: text(p, "season", 40)?.unwrap_or_default(),
            out_dir: PathBuf::from(text(p, "out_dir", 200)?.unwrap_or("/var/lib/steelcore/public".into())),
            name_key: PathBuf::from(text(p, "name_key", 200)?.unwrap_or("/var/lib/steelcore/public-name.key".into())),
            leaderboard_size: count(p, "leaderboard_size", 25, MAX_LEADERBOARD)?,
            ranking_pilots: count(p, "ranking_pilots", 200, MAX_PILOTS)?,
            battles: count(p, "battles", 30, MAX_BATTLES)?,
            matches: count(p, "matches", 30, MAX_MATCHES)?,
            map_base,
            sector_names,
            side_names,
            turn_length: super::turn::turn_length(turn_minutes),
            campaign,
        })
    }

    pub fn out_file(&self) -> PathBuf {
        self.out_dir.join(format!("{}.json", self.campaign))
    }
}

// ---- Snapshot (what the database says) ----------------------------------

#[derive(Debug, Clone)]
pub struct Period {
    pub round: i16,
    pub turn: i16,
    pub ends_at: DateTime<Utc>,
}

/// Sums of one player in the current round. `stats[i]` = `stat{i+1}` (upstream
/// `RankCategory`, `src/command.rs`): 1 hits, 2 shots (accuracy = 1/2), 3 kills,
/// 4 kill points, 5 deaths, 6 mission victories, 7 missions played (win rate =
/// 6/7), 8 base captures, 9 captures, 10 tippings.
#[derive(Debug, Clone)]
pub struct Standing {
    pub player: Uuid,
    pub name: Option<String>,
    pub faction: Option<String>,
    pub stats: [i64; 10],
    /// The same sums for the current turn only (the game's turn ranking).
    pub turn_stats: [i64; 10],
}

#[derive(Debug, Clone)]
pub struct SectorTide {
    pub map_id: i16,
    pub points: [i64; 3],
}

#[derive(Debug, Clone)]
pub struct Battle {
    pub at: DateTime<Utc>,
    pub turn: i16,
    pub map: i16,
    pub faction: String,
    pub player: Uuid,
    pub name: Option<String>,
    pub points: Vec<i64>,
    /// W531: the VT type of the battle (`battle_vt`), if known.
    pub vt: Option<i16>,
}

/// Sums of one player in one turn of the round (pilot history, awards).
#[derive(Debug, Clone)]
pub struct TurnRow {
    pub player: Uuid,
    pub turn: i16,
    /// The side of the player's most played missions in that turn.
    pub faction: Option<String>,
    pub stats: [i64; 10],
}

#[derive(Debug, Clone, Default)]
pub struct SideCounts {
    pub enlisted: i64,
    pub active: i64,
    pub last_24h: i64,
    pub prev_24h: i64,
}

#[derive(Debug, Clone)]
pub struct Snapshot {
    pub now: DateTime<Utc>,
    pub period: Period,
    pub standings: Vec<Standing>,
    pub sectors: Vec<SectorTide>,
    pub battles: Vec<Battle>,
    pub battles_round: i64,
    pub sides: HashMap<String, SideCounts>,
    /// W527: per player and turn of the round.
    pub turn_rows: Vec<TurnRow>,
    /// W527: the latest `PILOT_BATTLES` battles of every player.
    pub pilot_battles: Vec<Battle>,
    /// W527: battles per (player, turn, map) for the favourite sector.
    pub sector_counts: Vec<(Uuid, i16, i16, i64)>,
    /// W527: battles per turn of the round.
    pub turn_battles: Vec<(i16, i64)>,
    /// W527: end of every turn of the round (`settings.period`).
    pub turn_ends: Vec<(i16, DateTime<Utc>)>,
    /// W529: the newest `MATCH_ROWS` pilot results (newest first) for `matches[]`.
    pub match_rows: Vec<Battle>,
    /// W529: false when the query hit `MATCH_ROWS` (older results exist).
    pub match_rows_complete: bool,
    /// W531: VT names by type (`shop.name`).
    pub vt_names: HashMap<i16, String>,
    /// W531: first recorded sortie (the VT statistics start there); None = none yet.
    pub vt_since: Option<DateTime<Utc>>,
    /// W531: per player and own VT type of the round:
    /// (player, type, battles, deaths, wins, kills, kill points).
    pub pilot_vts: Vec<(Uuid, i16, i64, i64, i64, i64, i64)>,
    /// W531: battles per side and VT type of the round.
    pub side_vts: Vec<(String, i16, i64)>,
    /// W531: the round's results since the VT recording began (newest first,
    /// at most `VT_ROWS`), grouped like `matches[]` for `killed_by_type`.
    pub vt_rows: Vec<Battle>,
    pub vt_rows_complete: bool,
}

/// Players that never appear: banned accounts and players with a suspended identity.
const HIDDEN: &str = "(SELECT xuid FROM public.accounts WHERE banned \
     UNION SELECT player_id FROM core.identity WHERE suspended)";

/// Read the snapshot in one read-only transaction.
/// `round` overrides the current round (tests); the period still comes from `settings`.
pub async fn collect(pool: &PgPool, battles: usize, round: Option<i16>) -> Result<Snapshot, sqlx::Error> {
    let mut tx: Transaction<'_, Postgres> = pool.begin().await?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY").execute(&mut *tx).await?;
    // A slow query must not hold the snapshot (and its locks) for long; the timer simply tries again.
    sqlx::query("SET LOCAL statement_timeout = '20s'").execute(&mut *tx).await?;

    let now: DateTime<Utc> = sqlx::query_scalar("SELECT now()").fetch_one(&mut *tx).await?;
    let (r, turn, ends_at): (i16, i16, DateTime<Utc>) =
        sqlx::query_as("SELECT round, turn, period FROM public.settings ORDER BY round DESC, turn DESC LIMIT 1")
            .fetch_one(&mut *tx)
            .await?;
    let round = round.unwrap_or(r);
    let period = Period { round, turn, ends_at };

    let round_sums = (1..=10).map(|i| format!("SUM(l.stat{i})")).collect::<Vec<_>>().join(", ");
    let turn_sums =
        (1..=10).map(|i| format!("SUM(CASE WHEN l.turn = $2 THEN l.stat{i} ELSE 0 END)")).collect::<Vec<_>>().join(", ");
    let rows: Vec<(Uuid, Option<String>, Option<String>, Vec<i64>, Vec<i64>)> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT l.xuid, d.name, a.faction::text, ARRAY[{round_sums}]::bigint[], ARRAY[{turn_sums}]::bigint[]
         FROM public.leaderboard l
         LEFT JOIN public.accounts a ON a.xuid = l.xuid
         LEFT JOIN core.display_name d ON d.player_id = l.xuid
         WHERE l.round = $1 AND l.xuid NOT IN {HIDDEN}
         GROUP BY l.xuid, d.name, a.faction"
    )))
    .bind(round)
    .bind(turn)
    .fetch_all(&mut *tx)
    .await?;
    let ten = |s: Vec<i64>| {
        let mut out = [0i64; 10];
        for (i, v) in s.into_iter().take(10).enumerate() {
            out[i] = v;
        }
        out
    };
    let standings = rows
        .into_iter()
        .map(|(player, name, faction, s, t)| Standing { player, name, faction, stats: ten(s), turn_stats: ten(t) })
        // Nothing left (e.g. all results annulled): not ranked.
        .filter(|s| s.stats.iter().any(|&v| v != 0))
        .collect();

    let tides: Vec<(i16, i32, i32, i32)> =
        sqlx::query_as("SELECT \"map\", hsd, prf, rb FROM public.wartide WHERE round = $1 ORDER BY \"map\"")
            .bind(round)
            .fetch_all(&mut *tx)
            .await?;
    let sectors = tides
        .into_iter()
        .map(|(map_id, h, p, b)| SectorTide { map_id, points: [h.into(), p.into(), b.into()] })
        .collect();

    let open = format!(
        "g.annulled_at IS NULL AND g.round = $1 AND g.player_id NOT IN {HIDDEN}
         AND NOT EXISTS (SELECT 1 FROM core.identity i WHERE i.identity_id = g.identity_id AND i.suspended)"
    );
    let rows: Vec<(DateTime<Utc>, i16, i16, String, Uuid, Option<String>, Vec<i64>, Option<i16>)> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT g.at, g.turn, g.\"map\", g.faction::text, g.player_id, d.name, g.points, v.vt_type
         FROM public.gateway_result g
         LEFT JOIN core.display_name d ON d.player_id = g.player_id
         LEFT JOIN public.battle_vt v ON v.result_id = g.result_id
         WHERE g.kind = 'user_points' AND {open}
         ORDER BY g.at DESC, g.result_id DESC LIMIT $2"
    )))
    .bind(round)
    .bind(battles as i64)
    .fetch_all(&mut *tx)
    .await?;
    let battles = rows
        .into_iter()
        .map(|(at, turn, map, faction, player, name, points, vt)| Battle { at, turn, map, faction, player, name, points, vt })
        .collect();
    let battles_round: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT count(*) FROM public.gateway_result g WHERE g.kind = 'user_points' AND {open}"
    )))
    .bind(round)
    .fetch_one(&mut *tx)
    .await?;

    let mut sides: HashMap<String, SideCounts> = HashMap::new();
    let enlisted: Vec<(String, i64)> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT faction::text, count(*) FROM public.accounts
         WHERE faction IN ('hsd', 'prf', 'rb') AND xuid NOT IN {HIDDEN} GROUP BY faction"
    )))
    .fetch_all(&mut *tx)
    .await?;
    for (f, n) in enlisted {
        sides.entry(f).or_default().enlisted = n;
    }
    // Active: played for the side in the current or the previous turn of this round.
    let active: Vec<(String, i64)> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT faction::text, count(DISTINCT xuid) FROM public.leaderboard
         WHERE round = $1 AND turn >= $2 - 1 AND faction IN ('hsd', 'prf', 'rb') AND xuid NOT IN {HIDDEN}
         GROUP BY faction"
    )))
    .bind(round)
    .bind(turn)
    .fetch_all(&mut *tx)
    .await?;
    for (f, n) in active {
        sides.entry(f).or_default().active = n;
    }
    // Trend: war points of the last 24 hours against the 24 hours before.
    let trend: Vec<(String, i64, i64)> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT g.faction::text,
                COALESCE(SUM(g.points[1]) FILTER (WHERE g.at > now() - INTERVAL '24 hours'), 0)::bigint,
                COALESCE(SUM(g.points[1]) FILTER (WHERE g.at <= now() - INTERVAL '24 hours'), 0)::bigint
         FROM public.gateway_result g
         WHERE g.kind = 'mission_points' AND g.at > now() - INTERVAL '48 hours'
           AND g.faction IN ('hsd', 'prf', 'rb') AND {open}
         GROUP BY g.faction"
    )))
    .bind(round)
    .fetch_all(&mut *tx)
    .await?;
    for (f, a, b) in trend {
        let s = sides.entry(f).or_default();
        s.last_24h = a;
        s.prev_24h = b;
    }

    // W527: sums per turn (pilot history, awards like give_turn_award, pilot of the turn).
    let sums = (1..=10).map(|i| format!("SUM(l.stat{i})")).collect::<Vec<_>>().join(", ");
    let rows: Vec<(Uuid, i16, Option<String>, Vec<i64>)> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT l.xuid, l.turn, (array_agg(l.faction::text ORDER BY l.stat7 DESC, l.faction))[1],
                ARRAY[{sums}]::bigint[]
         FROM public.leaderboard l
         WHERE l.round = $1 AND l.xuid NOT IN {HIDDEN}
         GROUP BY l.xuid, l.turn"
    )))
    .bind(round)
    .fetch_all(&mut *tx)
    .await?;
    let turn_rows = rows
        .into_iter()
        .map(|(player, turn, faction, s)| TurnRow { player, turn, faction, stats: ten(s) })
        .filter(|r| r.stats.iter().any(|&v| v != 0))
        .collect();
    let rows: Vec<(DateTime<Utc>, i16, i16, String, Uuid, Option<String>, Vec<i64>, Option<i16>)> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT g.at, g.turn, g.\"map\", g.faction::text, g.player_id, d.name, g.points, v.vt_type
         FROM (SELECT g.*, row_number() OVER (PARTITION BY g.player_id ORDER BY g.at DESC, g.result_id DESC) AS nth
               FROM public.gateway_result g
               WHERE g.kind = 'user_points' AND {open}) g
         LEFT JOIN core.display_name d ON d.player_id = g.player_id
         LEFT JOIN public.battle_vt v ON v.result_id = g.result_id
         WHERE g.nth <= $2
         ORDER BY g.at DESC, g.result_id DESC"
    )))
    .bind(round)
    .bind(PILOT_BATTLES as i64)
    .fetch_all(&mut *tx)
    .await?;
    let pilot_battles = rows
        .into_iter()
        .map(|(at, turn, map, faction, player, name, points, vt)| Battle { at, turn, map, faction, player, name, points, vt })
        .collect();
    let sector_counts: Vec<(Uuid, i16, i16, i64)> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT g.player_id, g.turn, g.\"map\", count(*) FROM public.gateway_result g
         WHERE g.kind = 'user_points' AND {open} GROUP BY g.player_id, g.turn, g.\"map\""
    )))
    .bind(round)
    .fetch_all(&mut *tx)
    .await?;
    let turn_battles: Vec<(i16, i64)> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT g.turn, count(*) FROM public.gateway_result g WHERE g.kind = 'user_points' AND {open} GROUP BY g.turn"
    )))
    .bind(round)
    .fetch_all(&mut *tx)
    .await?;
    let turn_ends: Vec<(i16, DateTime<Utc>)> =
        sqlx::query_as("SELECT turn, period FROM public.settings WHERE round = $1 ORDER BY turn")
            .bind(round)
            .fetch_all(&mut *tx)
            .await?;
    // W529: the same results as `battles`, more of them, for grouping into matches.
    let rows: Vec<(DateTime<Utc>, i16, i16, String, Uuid, Option<String>, Vec<i64>, Option<i16>)> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT g.at, g.turn, g.\"map\", g.faction::text, g.player_id, d.name, g.points, v.vt_type
         FROM public.gateway_result g
         LEFT JOIN core.display_name d ON d.player_id = g.player_id
         LEFT JOIN public.battle_vt v ON v.result_id = g.result_id
         WHERE g.kind = 'user_points' AND {open}
         ORDER BY g.at DESC, g.result_id DESC LIMIT $2"
    )))
    .bind(round)
    .bind(MATCH_ROWS as i64)
    .fetch_all(&mut *tx)
    .await?;
    let match_rows_complete = rows.len() < MATCH_ROWS;
    let match_rows = rows
        .into_iter()
        .map(|(at, turn, map, faction, player, name, points, vt)| Battle { at, turn, map, faction, player, name, points, vt })
        .collect();
    // W531: VTs of the battles.
    let names: Vec<(i16, String)> = sqlx::query_as("SELECT \"type\", name FROM public.shop").fetch_all(&mut *tx).await?;
    let vt_names = names.into_iter().collect();
    let vt_since: Option<DateTime<Utc>> =
        sqlx::query_scalar("SELECT min(sortied_at) FROM public.battle_vt").fetch_one(&mut *tx).await?;
    let pilot_vts: Vec<(Uuid, i16, i64, i64, i64, i64, i64)> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT g.player_id, v.vt_type, count(*),
                COALESCE(SUM(g.points[5]), 0)::bigint,
                count(*) FILTER (WHERE g.points[7] > 0 AND g.points[6] > 0),
                COALESCE(SUM(g.points[3]), 0)::bigint,
                COALESCE(SUM(g.points[4]), 0)::bigint
         FROM public.gateway_result g JOIN public.battle_vt v ON v.result_id = g.result_id
         WHERE g.kind = 'user_points' AND cardinality(g.points) = 10 AND {open}
         GROUP BY g.player_id, v.vt_type"
    )))
    .bind(round)
    .fetch_all(&mut *tx)
    .await?;
    let side_vts: Vec<(String, i16, i64)> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT g.faction::text, v.vt_type, count(*)
         FROM public.gateway_result g JOIN public.battle_vt v ON v.result_id = g.result_id
         WHERE g.kind = 'user_points' AND {open}
         GROUP BY g.faction, v.vt_type"
    )))
    .bind(round)
    .fetch_all(&mut *tx)
    .await?;
    // Results from one match window before the first sortie on: older ones
    // cannot have a VT and cannot share a match with one that has.
    let (vt_rows, vt_rows_complete) = match vt_since {
        None => (Vec::new(), true),
        Some(since) => {
            let rows: Vec<(DateTime<Utc>, i16, i16, String, Uuid, Option<String>, Vec<i64>, Option<i16>)> =
                sqlx::query_as(sqlx::AssertSqlSafe(format!(
                    "SELECT g.at, g.turn, g.\"map\", g.faction::text, g.player_id, NULL::text, g.points, v.vt_type
                     FROM public.gateway_result g
                     LEFT JOIN public.battle_vt v ON v.result_id = g.result_id
                     WHERE g.kind = 'user_points' AND {open} AND g.at >= $2
                     ORDER BY g.at DESC, g.result_id DESC LIMIT $3"
                )))
                .bind(round)
                .bind(since - chrono::Duration::seconds(MATCH_WINDOW_SECS))
                .bind(VT_ROWS as i64)
                .fetch_all(&mut *tx)
                .await?;
            let complete = rows.len() < VT_ROWS;
            let rows = rows
                .into_iter()
                .map(|(at, turn, map, faction, player, name, points, vt)| Battle { at, turn, map, faction, player, name, points, vt })
                .collect();
            (rows, complete)
        }
    };
    tx.rollback().await?;
    Ok(Snapshot {
        now,
        period,
        standings,
        sectors,
        battles,
        battles_round,
        sides,
        turn_rows,
        pilot_battles,
        sector_counts,
        turn_battles,
        turn_ends,
        match_rows,
        match_rows_complete,
        vt_names,
        vt_since,
        pilot_vts,
        side_vts,
        vt_rows,
        vt_rows_complete,
    })
}

// ---- Building the public document (no database) -------------------------

fn ts(t: DateTime<Utc>) -> String {
    t.to_rfc3339_opts(SecondsFormat::Secs, true)
}

/// Shown name, or `None` when the stored name is unusable.
fn clean_name(name: Option<&str>) -> Option<String> {
    let n = name?.trim();
    let ok = !n.is_empty()
        && n.chars().count() <= MAX_NAME_CHARS
        && n.chars().all(|c| c.is_ascii_alphanumeric() || c == ' ');
    ok.then(|| n.to_owned())
}

/// Placeholder numbers: keyed hash of (season, player), 1000..=9999, unique
/// within one document (collisions move to the next free number, in hash order).
pub struct Placeholders {
    key: Vec<u8>,
    season: String,
    taken: HashMap<Uuid, u16>,
    used: HashSet<u16>,
}

impl Placeholders {
    pub fn new(key: &[u8], season: &str) -> Self {
        Self { key: key.to_vec(), season: season.to_owned(), taken: HashMap::new(), used: HashSet::new() }
    }

    fn hash(&self, player: &Uuid) -> u32 {
        let mut h = Sha256::new();
        h.update(b"steelcore-public-name-v1\0");
        h.update((self.key.len() as u32).to_be_bytes());
        h.update(&self.key);
        h.update(self.season.as_bytes());
        h.update([0u8]);
        h.update(player.as_bytes());
        let d = h.finalize();
        u32::from_be_bytes([d[0], d[1], d[2], d[3]])
    }

    /// Reserve numbers for all players first, so the order of calls does not matter.
    pub fn assign(&mut self, players: &[Uuid]) {
        let mut list: Vec<(u32, Uuid)> =
            players.iter().filter(|p| !self.taken.contains_key(p)).map(|p| (self.hash(p), *p)).collect();
        list.sort();
        list.dedup();
        for (h, p) in list {
            let mut n = (h % 9000) as u16 + 1000;
            while self.used.contains(&n) {
                n = if n == 9999 { 1000 } else { n + 1 };
            }
            self.used.insert(n);
            self.taken.insert(p, n);
        }
    }

    /// Placeholder of a player that `assign` has seen.
    pub fn label(&self, player: &Uuid) -> String {
        match self.taken.get(player) {
            Some(n) => format!("Pilot #{n}"),
            None => "Pilot".to_owned(),
        }
    }

    pub fn name(&mut self, player: &Uuid) -> String {
        if !self.taken.contains_key(player) {
            self.assign(&[*player]);
        }
        format!("Pilot #{}", self.taken[player])
    }
}

fn side_index(f: &str) -> Option<usize> {
    SIDES.iter().position(|s| *s == f)
}

/// Sector index 0..24 for a turn and a map number of the game.
pub fn sector_of(turn: i16, map: i16, map_base: i16) -> Option<usize> {
    let slot = map - map_base;
    if !(1..=TURNS_PER_ROUND).contains(&turn) || !(0..SECTORS_PER_TURN).contains(&slot) {
        return None;
    }
    Some(((turn - 1) * SECTORS_PER_TURN + slot) as usize)
}

/// Strict leader of a sector, as the game counts a won map (`war_tide_as_wins`).
pub fn leader(p: &[i64; 3]) -> Option<usize> {
    (0..3).find(|&i| (0..3).all(|j| j == i || p[i] > p[j]))
}

/// Percentages like the game's pie chart (`war_tide`): 300 starting points per
/// side, RB only from turn 3, the rounding error goes to HSD.
pub fn sector_share(p: &[i64; 3], turn: i16) -> [i64; 3] {
    let h = p[0] as f64 + STARTING_POINTS;
    let f = p[1] as f64 + STARTING_POINTS;
    let r = if turn >= RB_START_TURN { p[2] as f64 + STARTING_POINTS } else { 0.0 };
    percentages([h, f, r])
}

fn percentages(v: [f64; 3]) -> [i64; 3] {
    let total = v[0] + v[1] + v[2];
    if total <= 0.0 {
        return [0, 0, 0];
    }
    let mut out = v.map(|x| (100.0 * x / total) as i64);
    out[0] += 100 - (out[0] + out[1] + out[2]);
    out
}

/// War influence of the round like the game (`war_influence`): won sectors,
/// unplayed sectors shared among the sides in the war.
pub fn influence(tides: &[SectorTide], turn: i16) -> [i64; 3] {
    let mut wins = [0f64; 3];
    for t in tides {
        if let Some(i) = leader(&t.points) {
            wins[i] += 1.0;
        }
    }
    let unplayed = (SECTOR_COUNT as f64 - tides.len() as f64).max(0.0);
    if turn >= RB_START_TURN {
        for w in &mut wins {
            *w += unplayed / 3.0;
        }
    } else {
        wins[0] += unplayed / 2.0;
        wins[1] += unplayed / 2.0;
    }
    percentages(wins)
}

/// Neutral sector name: "Sector 3-B" (turn 3, second map).
pub fn default_sector_name(index: usize) -> String {
    let turn = index / SECTORS_PER_TURN as usize + 1;
    let slot = (b'A' + (index % SECTORS_PER_TURN as usize) as u8) as char;
    format!("Sector {turn}-{slot}")
}

/// `[a-z0-9]+(-[a-z0-9]+)*`, at most `MAX_SLUG` characters (steelnet checks the same).
pub fn valid_slug(s: &str) -> bool {
    (1..=MAX_SLUG).contains(&s.len())
        && s.split('-').all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit()))
}

/// Slug of a shown name: lower case, every run of other characters becomes one `-`.
pub fn slug_base(name: &str) -> String {
    let mut out = String::new();
    for c in name.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if !out.is_empty() && !out.ends_with('-') {
            out.push('-');
        }
    }
    // Room for a "-NNN" suffix.
    out.truncate(MAX_SLUG - 4);
    let out = out.trim_end_matches('-');
    if out.is_empty() { "pilot".to_owned() } else { out.to_owned() }
}

/// Unique slugs for (key, shown name, is placeholder). Real names first, then
/// placeholders, each in name order; a taken slug gets `-2`, `-3`, ...
pub fn assign_slugs<K: Copy + Eq + std::hash::Hash + Ord>(list: &[(K, String, bool)]) -> HashMap<K, String> {
    let mut order: Vec<&(K, String, bool)> = list.iter().collect();
    order.sort_by(|a, b| a.2.cmp(&b.2).then(a.1.cmp(&b.1)).then(a.0.cmp(&b.0)));
    let mut taken: HashSet<String> = HashSet::new();
    let mut out = HashMap::new();
    for (k, name, _) in order {
        let base = slug_base(name);
        let mut slug = base.clone();
        let mut n = 2;
        while taken.contains(&slug) {
            slug = format!("{base}-{n}");
            n += 1;
        }
        taken.insert(slug.clone());
        out.insert(*k, slug);
    }
    out
}

/// Award places of one turn like `give_turn_award`: per category the indices of
/// at most `AWARD_PLACES` players, best first. Ratios are integer per mille as in
/// the SQL; equal values are ordered by the shown name (the game takes any order).
pub fn turn_award_places(stats: &[[i64; 10]], names: &[&str]) -> [Vec<usize>; 4] {
    let score = |cat: usize, s: &[i64; 10]| -> Option<i64> {
        match cat {
            0 => (s[1] >= AWARD_MIN_SHOTS).then(|| s[0] * 1000 / s[1]),
            1 => (s[3] > 0).then_some(s[3]),
            2 => (s[6] >= AWARD_MIN_MISSIONS).then(|| s[5] * 1000 / s[6]),
            _ => (s[7] > 0).then_some(s[7]),
        }
    };
    std::array::from_fn(|cat| {
        let mut list: Vec<(i64, usize)> =
            stats.iter().enumerate().filter_map(|(i, s)| score(cat, s).map(|v| (v, i))).collect();
        list.sort_by(|a, b| b.0.cmp(&a.0).then(names[a.1].cmp(names[b.1])).then(a.1.cmp(&b.1)));
        list.into_iter().take(AWARD_PLACES).map(|(_, i)| i).collect()
    })
}

/// Closest played sector of one turn: the smallest war-point gap between the two
/// best sides in the war. (sector index, strict leader, gap in war points, gap in
/// share points). `None` without a played sector; equal gaps: the first sector.
pub fn closest_sector(tides: &HashMap<usize, [i64; 3]>, turn: i16) -> Option<(usize, Option<usize>, i64, i64)> {
    let first = ((turn - 1) * SECTORS_PER_TURN) as usize;
    let sides = if turn >= RB_START_TURN { 3 } else { 2 };
    let top2 = |v: &[i64]| {
        let mut v = v.to_vec();
        v.sort_unstable_by(|a, b| b.cmp(a));
        v[0] - v[1]
    };
    let mut best: Option<(usize, Option<usize>, i64, i64)> = None;
    for idx in first..first + SECTORS_PER_TURN as usize {
        let Some(p) = tides.get(&idx) else { continue };
        let gap = top2(&p[..sides]);
        let margin = top2(&sector_share(p, turn)[..sides]);
        if best.is_none_or(|b| gap < b.2) {
            best = Some((idx, leader(p), gap, margin));
        }
    }
    best
}

fn thousands(n: i64) -> String {
    let digits = n.unsigned_abs().to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    if n < 0 { format!("-{out}") } else { out }
}

/// Text safe for Discord markdown: no formatting, mention, heading or link characters.
pub fn md_plain(s: &str) -> String {
    s.chars()
        .filter(|c| !matches!(c, '*' | '_' | '~' | '`' | '|' | '>' | '<' | '@' | '[' | ']' | '(' | ')' | '#' | '\\' | ':'))
        .filter(|c| !c.is_control())
        .collect()
}

/// Build the public document. Pure: everything comes from the snapshot.
// ---- W529: battles grouped into matches -----------------------------------

/// Result of one pilot report: `win`, `loss`, or `None` without the ten stats
/// (stat7 = missions played, stat6 = missions won).
pub fn pilot_result(points: &[i64]) -> Option<&'static str> {
    let st = |i: usize| points.get(i).copied().unwrap_or(0);
    (points.len() == 10 && st(6) > 0).then(|| if st(5) > 0 { "win" } else { "loss" })
}

/// W531: kills per pilot and VT type of the destroyed VTs, only where that
/// is clear: every pilot on the other sides of the match reported and drove
/// the same VT type. The game reports only the number of kills, so in every
/// other match the kills stay unassigned (they count in the sums).
pub fn killed_by_type(rows: &[Battle], groups: &[Vec<usize>]) -> HashMap<Uuid, HashMap<i16, i64>> {
    let mut out: HashMap<Uuid, HashMap<i16, i64>> = HashMap::new();
    for g in groups {
        for &i in g {
            let p = &rows[i];
            let kills = p.points.get(2).copied().unwrap_or(0);
            let Some(side) = side_index(&p.faction) else { continue };
            if kills <= 0 || p.points.len() != 10 {
                continue;
            }
            let mut types = g.iter().map(|&j| &rows[j]).filter(|o| side_index(&o.faction).is_some_and(|s| s != side)).map(|o| o.vt);
            let Some(Some(first)) = types.next() else { continue };
            if types.all(|t| t == Some(first)) {
                *out.entry(p.player).or_default().entry(first).or_insert(0) += kills;
            }
        }
    }
    out
}

/// Result of one side in a match: every reporting pilot agrees (`win`/`loss`),
/// `mixed` when they do not, `None` when nobody reported a result.
pub fn side_result(results: &[Option<&'static str>]) -> Option<&'static str> {
    let mut seen = results.iter().flatten();
    let first = *seen.next()?;
    Some(if seen.all(|r| *r == first) { first } else { "mixed" })
}

/// Winner of a match from the side results (game order, `SIDES` index):
/// `(winner, note)`.
/// - The one side whose pilots reported `win` wins (the others lost or sent no result).
/// - Two sides with `win`, or a side whose pilots disagree: `(None, "conflict")`.
/// - Every reporting side lost (at least two): `draw`.
/// - Only one side in the match: shown anyway with `"one_side"` (a win stays a win).
/// - No result at all: `(None, None)`.
pub fn match_winner(sides: &[(usize, Option<&'static str>)]) -> (Option<&'static str>, Option<&'static str>) {
    let one = (sides.len() == 1).then_some("one_side");
    if sides.iter().any(|s| s.1 == Some("mixed")) {
        return (None, Some("conflict"));
    }
    let wins: Vec<usize> = sides.iter().filter(|s| s.1 == Some("win")).map(|s| s.0).collect();
    let losses = sides.iter().filter(|s| s.1 == Some("loss")).count();
    match wins.len() {
        1 => (Some(SIDES[wins[0]]), one),
        0 if losses >= 2 && losses == sides.len() => (Some("draw"), None),
        0 => (None, one),
        _ => (None, Some("conflict")),
    }
}

/// Group pilot results (`rows` newest first, as the query returns them) into
/// matches; returns the row indices of every match, the newest match first.
///
/// There is no match id. The pilots of one match report at its end, each
/// console on its own, so the reports of one match share round, turn and
/// sector (map) and arrive close together: a report joins the open match of
/// its sector when it came at most `window` after that match's first report.
/// A pilot is in a match at most once: a second report of the same pilot
/// starts a new match (two short matches back to back in one sector).
/// `complete = false` (the query hit its row limit): matches that began within
/// `window` of the oldest row may miss reports and are left out.
pub fn group_matches(rows: &[Battle], window: chrono::Duration, complete: bool) -> Vec<Vec<usize>> {
    // (first report, last report, members)
    let mut groups: Vec<(DateTime<Utc>, DateTime<Utc>, Vec<usize>)> = Vec::new();
    let mut open: HashMap<(i16, i16), usize> = HashMap::new();
    for i in (0..rows.len()).rev() {
        let r = &rows[i];
        let key = (r.turn, r.map);
        let join = open.get(&key).copied().filter(|&g| {
            r.at - groups[g].0 <= window && !groups[g].2.iter().any(|&j| rows[j].player == r.player)
        });
        match join {
            Some(g) => {
                groups[g].1 = groups[g].1.max(r.at);
                groups[g].2.push(i);
            }
            None => {
                groups.push((r.at, r.at, vec![i]));
                open.insert(key, groups.len() - 1);
            }
        }
    }
    if !complete {
        if let Some(oldest) = rows.last() {
            groups.retain(|g| g.0 - oldest.at > window);
        }
    }
    groups.sort_by(|a, b| b.1.cmp(&a.1).then(b.0.cmp(&a.0)));
    groups.into_iter().map(|g| g.2).collect()
}

/// Shorten `matches[]` (oldest first) until the rendered document fits `limit`
/// bytes; the other lists stay as they are.
pub fn fit_matches(doc: &mut Value, limit: usize) {
    // Rendered size: compact JSON plus the final newline (`render`).
    let mut size = serde_json::to_vec(doc).map(|v| v.len() + 1).unwrap_or(0);
    let Some(m) = doc.get_mut("matches").and_then(Value::as_array_mut) else { return };
    while size > limit {
        let Some(last) = m.pop() else { return };
        // The entry and the comma before it (none for the only entry).
        size -= serde_json::to_vec(&last).map(|v| v.len()).unwrap_or(0) + usize::from(!m.is_empty());
    }
}

pub fn build(cfg: &PublicConfig, snap: &Snapshot, names: &mut Placeholders) -> Value {
    build_all(cfg, snap, names).0
}

/// The public document plus one file per listed pilot (`<slug>`, document).
pub fn build_all(cfg: &PublicConfig, snap: &Snapshot, names: &mut Placeholders) -> (Value, Vec<(String, Value)>) {
    let period = &snap.period;
    // Placeholder numbers for everyone who may appear, before any is used.
    let mut everyone: Vec<Uuid> = snap.standings.iter().map(|s| s.player).collect();
    everyone.extend(snap.battles.iter().map(|b| b.player));
    everyone.extend(snap.turn_rows.iter().map(|r| r.player));
    everyone.extend(snap.pilot_battles.iter().map(|b| b.player));
    everyone.extend(snap.match_rows.iter().map(|b| b.player));
    names.assign(&everyone);
    let minute = |t: DateTime<Utc>| ts(t - chrono::Duration::seconds(t.timestamp().rem_euclid(60)));
    // Everyone has a number now: read-only from here on.
    let names: &Placeholders = names;
    let shown = |player: &Uuid, name: Option<&str>| clean_name(name).unwrap_or_else(|| names.label(player));
    // W531: VT name by type (shop name; a type without a shop entry gets "VT <type>").
    let vt_name = |t: i16| -> String {
        snap.vt_names
            .get(&t)
            .map(|n| n.trim().chars().take(MAX_VT_CHARS).collect::<String>())
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| format!("VT {t}"))
    };
    let vt_of = |b: &Battle| b.vt.map(vt_name);

    // Sectors of the round.
    let mut tide_by_index: HashMap<usize, [i64; 3]> = HashMap::new();
    for t in &snap.sectors {
        let idx = t.map_id - cfg.map_base;
        if (0..SECTOR_COUNT).contains(&idx) {
            tide_by_index.insert(idx as usize, t.points);
        }
    }
    // W528: battles (pilot results) per sector, from the per-player counts.
    let mut sector_battles: HashMap<usize, i64> = HashMap::new();
    for &(_, turn, map, n) in &snap.sector_counts {
        if let Some(idx) = sector_of(turn, map, cfg.map_base) {
            *sector_battles.entry(idx).or_insert(0) += n;
        }
    }
    let mut held = [0i64; 3];
    let mut war_points = [0i64; 3];
    let sectors: Vec<Value> = (0..SECTOR_COUNT as usize)
        .map(|i| {
            let turn = (i / SECTORS_PER_TURN as usize) as i16 + 1;
            let p = tide_by_index.get(&i).copied();
            let state = if turn < period.turn {
                "closed"
            } else if turn == period.turn {
                "contested"
            } else {
                "upcoming"
            };
            let lead = p.as_ref().and_then(leader);
            if let (Some(l), Some(_)) = (lead, p) {
                held[l] += 1;
            }
            if let Some(p) = p {
                for k in 0..3 {
                    war_points[k] += p[k];
                }
            }
            let pts = p.unwrap_or([0; 3]);
            let share = if turn <= period.turn { sector_share(&pts, turn) } else { [0, 0, 0] };
            json!({
                "index": i,
                "turn": turn,
                "slot": i % SECTORS_PER_TURN as usize + 1,
                "name": cfg.sector_names.get(i).cloned().unwrap_or_else(|| default_sector_name(i)),
                "state": state,
                "played": p.is_some(),
                "points": {"hsd": pts[0], "prf": pts[1], "rb": pts[2]},
                "share": {"hsd": share[0], "prf": share[1], "rb": share[2]},
                "leader": lead.map(|l| SIDES[l]),
                "battles": sector_battles.get(&i).copied().unwrap_or(0),
            })
        })
        .collect();
    // Only rows of this round's 24 sectors count (a row with another map
    // number would otherwise shrink the unplayed share).
    let in_round: Vec<SectorTide> =
        snap.sectors.iter().filter(|t| (0..SECTOR_COUNT).contains(&(t.map_id - cfg.map_base))).cloned().collect();
    let infl = influence(&in_round, period.turn);

    // Rankings: kill points, then kills, then the shown name.
    let mut rows: Vec<(String, &Standing)> =
        snap.standings.iter().map(|s| (shown(&s.player, s.name.as_deref()), s)).collect();
    rows.sort_by(|a, b| {
        b.1.stats[3].cmp(&a.1.stats[3]).then(b.1.stats[2].cmp(&a.1.stats[2])).then(a.0.cmp(&b.0))
    });
    let row = |rank: usize, name: &str, s: &Standing| {
        json!({
            "rank": rank,
            "pilot": name,
            "side": s.faction.as_deref().filter(|f| side_index(f).is_some()),
            "points": s.stats[3],
            "kills": s.stats[2],
            "deaths": s.stats[4],
            "wins": s.stats[5],
            "sorties": s.stats[6],
            // W524 name for stat8 (base captures); kept as it is (schema 1 only adds fields).
            "captures": s.stats[7],
            "hits": s.stats[0],
            "shots": s.stats[1],
            "base_captures": s.stats[7],
            "capture_count": s.stats[8],
            "tippings": s.stats[9],
        })
    };
    let board = |filter: Option<&str>| -> Vec<Value> {
        rows.iter()
            .filter(|(_, s)| filter.is_none_or(|f| s.faction.as_deref() == Some(f)))
            .take(cfg.leaderboard_size)
            .enumerate()
            .map(|(i, (n, s))| row(i + 1, n, s))
            .collect()
    };
    let mut boards = serde_json::Map::new();
    boards.insert("overall".into(), Value::Array(board(None)));
    for side in SIDES {
        boards.insert(side.into(), Value::Array(board(Some(side))));
    }

    // Full ranking list: every ranked pilot (up to `ranking_pilots`) with the
    // round and the current turn, so the page can rank any category and side.
    let listed = ranking_selection(&rows.iter().map(|(_, s)| *s).collect::<Vec<_>>(), cfg.ranking_pilots);
    // W527: slugs (profile links) for the listed pilots, unique in the document.
    let slugs: HashMap<Uuid, String> = assign_slugs(
        &rows
            .iter()
            .enumerate()
            .filter(|(i, _)| listed.contains(i))
            .map(|(_, (n, s))| (s.player, n.clone(), clean_name(s.name.as_deref()).is_none()))
            .collect::<Vec<_>>(),
    );
    let mut turn_names: HashMap<Uuid, String> = rows.iter().map(|(n, s)| (s.player, n.clone())).collect();
    for r in &snap.turn_rows {
        if !turn_names.contains_key(&r.player) {
            let n = names.label(&r.player);
            turn_names.insert(r.player, n);
        }
    }

    // W527: awards of every closed turn, recomputed from the leaderboard with the
    // rules of give_turn_award (the `awards` table loses a row once the pilot
    // claims the award in the game, so it is no history).
    let mut by_turn: HashMap<i16, Vec<&TurnRow>> = HashMap::new();
    for r in &snap.turn_rows {
        by_turn.entry(r.turn).or_default().push(r);
    }
    let closed: Vec<i16> = (1..period.turn.min(TURNS_PER_ROUND + 1)).collect();
    // player -> [(turn, category, place)]
    let mut awards_of: HashMap<Uuid, Vec<(i16, usize, usize)>> = HashMap::new();
    for &t in &closed {
        let Some(list) = by_turn.get(&t) else { continue };
        let stats: Vec<[i64; 10]> = list.iter().map(|r| r.stats).collect();
        let nm: Vec<&str> = list.iter().map(|r| turn_names[&r.player].as_str()).collect();
        for (cat, places) in turn_award_places(&stats, &nm).iter().enumerate() {
            for (place, &i) in places.iter().enumerate() {
                awards_of.entry(list[i].player).or_default().push((t, cat, place + 1));
            }
        }
    }
    for v in awards_of.values_mut() {
        v.sort();
    }

    let pilots: Vec<Value> = rows
        .iter()
        .enumerate()
        .filter(|(i, _)| listed.contains(i))
        .map(|(_, (n, s))| {
            let mut p = json!({
                "pilot": n,
                "side": s.faction.as_deref().filter(|f| side_index(f).is_some()),
                "round": stat_block(&s.stats),
                "turn": if s.turn_stats.iter().any(|&v| v != 0) { stat_block(&s.turn_stats) } else { Value::Null },
                "slug": slugs[&s.player],
            });
            if let Some(a) = awards_of.get(&s.player) {
                // Compact: [turn, category index (AWARD_CATEGORIES), place].
                p["awards"] = a.iter().map(|&(t, c, r)| json!([t, c, r])).collect();
            }
            p
        })
        .collect();
    let ranked_turn = snap.standings.iter().filter(|s| s.turn_stats.iter().any(|&v| v != 0)).count();

    // Side summaries.
    let mut pilot_points = [0i64; 3];
    for s in &snap.standings {
        if let Some(i) = s.faction.as_deref().and_then(side_index) {
            pilot_points[i] += s.stats[3];
        }
    }
    // W531: most driven VTs per side (battles with a known VT).
    let top_vts = |side: &str| -> Vec<Value> {
        let mut v: Vec<(String, i64)> =
            snap.side_vts.iter().filter(|(f, _, _)| f == side).map(|&(_, t, n)| (vt_name(t), n)).collect();
        v.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        v.into_iter().take(TOP_VTS).map(|(n, k)| json!({"vt": n, "battles": k})).collect()
    };
    let sides: Vec<Value> = SIDES
        .iter()
        .enumerate()
        .map(|(i, side)| {
            let c = snap.sides.get(*side).cloned().unwrap_or_default();
            json!({
                "id": side,
                "name": cfg.side_names[i],
                "joins_turn": if i == 2 { RB_START_TURN } else { 1 },
                "enlisted": c.enlisted,
                "active": c.active,
                "pilot_points": pilot_points[i],
                "war_points": war_points[i],
                "sectors_held": held[i],
                "influence": infl[i],
                "trend": {"last_24h": c.last_24h, "prev_24h": c.prev_24h},
                "top_vts": top_vts(side),
            })
        })
        .collect();

    // Latest battles.
    let battles: Vec<Value> = snap
        .battles
        .iter()
        .map(|b| {
            let st = |i: usize| b.points.get(i).copied().unwrap_or(0);
            let result = if b.points.len() == 10 && st(6) > 0 {
                Value::from(if st(5) > 0 { "win" } else { "loss" })
            } else {
                Value::Null
            };
            json!({
                "at": minute(b.at),
                "turn": b.turn,
                "sector": sector_of(b.turn, b.map, cfg.map_base),
                "side": side_index(&b.faction).map(|i| SIDES[i]),
                "pilot": shown(&b.player, b.name.as_deref()),
                "vt": vt_of(b),
                "result": result,
                "points": st(3),
                "kills": st(2),
            })
        })
        .collect();

    // W529: the same results grouped into matches: one entry per battle with
    // its sides, pilots and the winning side.
    let groups = group_matches(&snap.match_rows, chrono::Duration::seconds(MATCH_WINDOW_SECS), snap.match_rows_complete);
    let matches: Vec<Value> = groups
        .iter()
        .take(cfg.matches)
        .map(|g| {
            let rows: Vec<&Battle> = g.iter().map(|&i| &snap.match_rows[i]).collect();
            let first = rows[0];
            let end = rows.iter().map(|b| b.at).max().unwrap_or(first.at);
            // Per side (game order): (pilots, points, kills, results).
            let mut per: Vec<(usize, Vec<Value>, i64, i64, Vec<Option<&'static str>>)> = Vec::new();
            let mut sorted = rows.clone();
            sorted.sort_by(|a, b| {
                let st = |x: &Battle, i: usize| x.points.get(i).copied().unwrap_or(0);
                st(b, 3).cmp(&st(a, 3)).then(st(b, 2).cmp(&st(a, 2))).then(
                    shown(&a.player, a.name.as_deref()).cmp(&shown(&b.player, b.name.as_deref())),
                )
            });
            for b in sorted {
                let Some(side) = side_index(&b.faction) else { continue };
                let st = |i: usize| b.points.get(i).copied().unwrap_or(0);
                let res = pilot_result(&b.points);
                let mut p = json!({
                    "pilot": shown(&b.player, b.name.as_deref()),
                    "vt": vt_of(b),
                    "result": res,
                    "points": st(3),
                    "kills": st(2),
                });
                if let Some(sl) = slugs.get(&b.player) {
                    p["slug"] = json!(sl);
                }
                let k = match per.iter().position(|e| e.0 == side) {
                    Some(k) => k,
                    None => {
                        per.push((side, Vec::new(), 0, 0, Vec::new()));
                        per.len() - 1
                    }
                };
                let e = &mut per[k];
                e.1.push(p);
                e.2 += st(3);
                e.3 += st(2);
                e.4.push(res);
            }
            per.sort_by_key(|e| e.0);
            let side_results: Vec<(usize, Option<&'static str>)> =
                per.iter().map(|e| (e.0, side_result(&e.4))).collect();
            let (winner, note) = match_winner(&side_results);
            let mut sides: Vec<(usize, Value)> = per
                .into_iter()
                .map(|(side, pilots, points, kills, res)| {
                    let v = json!({
                        "side": SIDES[side],
                        "result": side_result(&res),
                        "points": points,
                        "kills": kills,
                        "pilots": pilots,
                    });
                    (side, v)
                })
                .collect();
            // The winning side first, then the game order.
            if let Some(w) = winner.filter(|w| *w != "draw") {
                sides.sort_by_key(|(s, _)| (SIDES[*s] != w, *s));
            }
            let sides: Vec<Value> = sides.into_iter().map(|(_, v)| v).collect();
            json!({
                "at": minute(end),
                "turn": first.turn,
                "sector": sector_of(first.turn, first.map, cfg.map_base),
                "winner": winner,
                "note": note,
                "sides": sides,
            })
        })
        .collect();

    // W527: one entry per turn so far (war chart, turn reports).
    let ends: HashMap<i16, DateTime<Utc>> = snap.turn_ends.iter().copied().collect();
    let tb: HashMap<i16, i64> = snap.turn_battles.iter().copied().collect();
    let sector_name = |i: usize| cfg.sector_names.get(i).cloned().unwrap_or_else(|| default_sector_name(i));
    let side_name = |i: usize| md_plain(&cfg.side_names[i]);
    let turns: Vec<Value> = (1..=period.turn.min(TURNS_PER_ROUND))
        .map(|t| {
            let first = ((t - 1) * SECTORS_PER_TURN) as usize;
            let in_war = if t >= RB_START_TURN { 3 } else { 2 };
            let mut wp = [0i64; 3];
            for i in first..first + SECTORS_PER_TURN as usize {
                if let Some(p) = tide_by_index.get(&i) {
                    for k in 0..3 {
                        wp[k] += p[k];
                    }
                }
            }
            let upto: Vec<SectorTide> =
                in_round.iter().filter(|x| (x.map_id - cfg.map_base) / SECTORS_PER_TURN < t).cloned().collect();
            let inf = influence(&upto, t);
            let best = by_turn.get(&t).and_then(|list| {
                list.iter()
                    .filter(|r| r.stats[3] > 0)
                    .max_by(|a, b| {
                        a.stats[3]
                            .cmp(&b.stats[3])
                            .then(a.stats[2].cmp(&b.stats[2]))
                            .then(turn_names[&b.player].cmp(&turn_names[&a.player]))
                    })
                    .copied()
            });
            let pilot = best.map(|r| {
                let mut v = json!({
                    "pilot": turn_names[&r.player],
                    "side": r.faction.as_deref().filter(|f| side_index(f).is_some()),
                    "points": r.stats[3],
                    "kills": r.stats[2],
                });
                if let Some(sl) = slugs.get(&r.player) {
                    v["slug"] = json!(sl);
                }
                v
            });
            let close = closest_sector(&tide_by_index, t);
            let state = if t < period.turn { "closed" } else { "contested" };
            let battles_t = tb.get(&t).copied().unwrap_or(0);
            let mut e = json!({
                "turn": t,
                "state": state,
                "ends_at": ends.get(&t).map(|x| minute(*x)),
                "battles": battles_t,
                "war_points": {"hsd": wp[0], "prf": wp[1], "rb": wp[2]},
                "influence": {"hsd": inf[0], "prf": inf[1], "rb": inf[2]},
                "pilot": pilot,
                "closest": close.map(|(i, l, g, m)| json!({"sector": i, "leader": l.map(|l| SIDES[l]), "gap": g, "margin": m})),
            });
            if state == "closed" {
                // Ready-made Discord text for the steward; nothing posts it automatically.
                let mut md = format!("**Turn {t} report**");
                if !cfg.season.is_empty() {
                    md.push_str(&format!(" · {} · round {}", md_plain(&cfg.season), period.round));
                }
                md.push('\n');
                for i in first..first + SECTORS_PER_TURN as usize {
                    let line = match tide_by_index.get(&i) {
                        None => "not played".to_owned(),
                        Some(p) => {
                            let share = sector_share(p, t);
                            let who = leader(p).map(|l| format!("**{}**", side_name(l))).unwrap_or("drawn".into());
                            let parts: Vec<String> = (0..in_war).map(|k| format!("{} {}%", side_name(k), share[k])).collect();
                            format!("{who} ({})", parts.join(" / "))
                        }
                    };
                    md.push_str(&format!("- {}: {line}\n", md_plain(&sector_name(i))));
                }
                let inf_parts: Vec<String> = (0..in_war).map(|k| format!("{} {}%", side_name(k), inf[k])).collect();
                md.push_str(&format!("War influence after the turn: {}\n", inf_parts.join(" · ")));
                if let Some(r) = best {
                    let side =
                        r.faction.as_deref().and_then(side_index).map(|i| format!(" ({})", side_name(i))).unwrap_or_default();
                    md.push_str(&format!(
                        "Pilot of the turn: **{}**{side}, {} kill points\n",
                        md_plain(&turn_names[&r.player]),
                        thousands(r.stats[3])
                    ));
                }
                if let Some((i, l, g, _)) = close {
                    let how = match l {
                        Some(l) => format!("{} ahead by {} war points", side_name(l), thousands(g)),
                        None => "drawn".to_owned(),
                    };
                    md.push_str(&format!("Closest sector: {}, {how}\n", md_plain(&sector_name(i))));
                }
                md.push_str(&format!("Battles: {}", thousands(battles_t)));
                e["discord_md"] = json!(md);
            }
            e
        })
        .collect();

    // W527: one file per listed pilot (profile).
    let mut fav: HashMap<Uuid, (i64, usize)> = HashMap::new();
    for &(player, turn, map, n) in &snap.sector_counts {
        if let Some(idx) = sector_of(turn, map, cfg.map_base) {
            let e = fav.entry(player).or_insert((0, 0));
            // Most battles; on a tie the later sector.
            if (n, idx) > *e {
                *e = (n, idx);
            }
        }
    }
    let battle_json = |b: &Battle| {
        let st = |i: usize| b.points.get(i).copied().unwrap_or(0);
        let result =
            if b.points.len() == 10 && st(6) > 0 { Value::from(if st(5) > 0 { "win" } else { "loss" }) } else { Value::Null };
        json!({
            "at": minute(b.at),
            "turn": b.turn,
            "sector": sector_of(b.turn, b.map, cfg.map_base),
            "side": side_index(&b.faction).map(|i| SIDES[i]),
            "vt": vt_of(b),
            "result": result,
            "points": st(3),
            "kills": st(2),
        })
    };
    // W531: VT statistics per pilot (own VTs exact, destroyed VTs where clear).
    let vt_groups = group_matches(&snap.vt_rows, chrono::Duration::seconds(MATCH_WINDOW_SECS), snap.vt_rows_complete);
    let killed = killed_by_type(&snap.vt_rows, &vt_groups);
    let mut own_vts: HashMap<Uuid, Vec<(i16, i64, i64, i64, i64, i64)>> = HashMap::new();
    for &(player, t, driven, lost, wins, kills, points) in &snap.pilot_vts {
        own_vts.entry(player).or_default().push((t, driven, lost, wins, kills, points));
    }
    let mut pilot_docs: Vec<(String, Value)> = Vec::new();
    for (i, (n, s)) in rows.iter().enumerate() {
        if !listed.contains(&i) {
            continue;
        }
        let mut hist: Vec<&TurnRow> = snap.turn_rows.iter().filter(|r| r.player == s.player).collect();
        hist.sort_by_key(|r| r.turn);
        let history: Vec<Value> = hist
            .iter()
            .map(|r| {
                let mut v = stat_block(&r.stats);
                v["turn"] = json!(r.turn);
                v["side"] = json!(r.faction.as_deref().filter(|f| side_index(f).is_some()));
                v
            })
            .collect();
        let battles: Vec<Value> =
            snap.pilot_battles.iter().filter(|b| b.player == s.player).take(PILOT_BATTLES).map(battle_json).collect();
        let awards: Vec<Value> = awards_of
            .get(&s.player)
            .map(|a| a.iter().map(|&(t, c, r)| json!({"turn": t, "category": AWARD_CATEGORIES[c], "rank": r})).collect())
            .unwrap_or_default();
        // Most driven first; on a tie more wins, then the name.
        let mut mine: Vec<(String, i64, i64, i64, i64, i64)> = own_vts
            .get(&s.player)
            .map(|v| v.iter().map(|&(t, d, l, w, k, p)| (vt_name(t), d, l, w, k, p)).collect())
            .unwrap_or_default();
        mine.sort_by(|a, b| b.1.cmp(&a.1).then(b.3.cmp(&a.3)).then(a.0.cmp(&b.0)));
        let favorite_vt = mine.first().map(|m| json!({"vt": m.0, "battles": m.1}));
        let vts: Vec<Value> = mine
            .iter()
            .map(|(n, d, l, w, k, p)| json!({"vt": n, "driven": d, "lost": l, "wins": w, "kills": k, "points": p}))
            .collect();
        let mut kt: Vec<(String, i64)> =
            killed.get(&s.player).map(|m| m.iter().map(|(&t, &n)| (vt_name(t), n)).collect()).unwrap_or_default();
        kt.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        let killed_types: Vec<Value> = kt.iter().map(|(n, k)| json!({"vt": n, "kills": k})).collect();
        let slug = slugs[&s.player].clone();
        pilot_docs.push((
            slug.clone(),
            json!({
                "schema": SCHEMA,
                "campaign": {"id": cfg.campaign},
                "period": {"round": period.round, "turn": period.turn},
                "pilot": n,
                "slug": slug,
                "side": s.faction.as_deref().filter(|f| side_index(f).is_some()),
                "round": stat_block(&s.stats),
                "turn": if s.turn_stats.iter().any(|&v| v != 0) { stat_block(&s.turn_stats) } else { Value::Null },
                "history": history,
                "favorite_sector": fav.get(&s.player).map(|&(n, i)| json!({"sector": i, "battles": n})),
                "vts": vts,
                "favorite_vt": favorite_vt,
                "killed_by_type": killed_types,
                "battles": battles,
                "awards": awards,
            }),
        ));
    }

    let turn_minutes = cfg.turn_length.as_secs() / 60;
    let started = period.ends_at - chrono::Duration::seconds(cfg.turn_length.as_secs() as i64);
    let doc = json!({
        "schema": SCHEMA,
        "generated_at": ts(snap.now),
        "campaign": {"id": cfg.campaign, "name": cfg.name, "season": cfg.season},
        "period": {
            "round": period.round,
            "turn": period.turn,
            "turns_per_round": TURNS_PER_ROUND,
            "turn_minutes": turn_minutes,
            "turn_started_at": ts(started),
            "turn_ends_at": ts(period.ends_at),
        },
        "sides": sides,
        "sectors": sectors,
        "leaderboards": boards,
        "pilots": pilots,
        "battles": battles,
        "matches": matches,
        // W531: the VT of a battle is known from this day on (UTC); null = not yet.
        "vts_since": snap.vt_since.map(|t| t.format("%Y-%m-%d").to_string()),
        "totals": {
            "ranked_pilots": rows.len(),
            "ranked_pilots_turn": ranked_turn,
            "battles_round": snap.battles_round,
        },
        "turns": turns,
        "awards": {"categories": AWARD_CATEGORIES, "per_turn": AWARD_PLACES, "turns": closed},
    });
    let mut doc = doc;
    fit_matches(&mut doc, MATCH_FIT_BYTES);
    (doc, pilot_docs)
}

/// The ten sums of one pilot under their public names (`pilots[].round` / `.turn`).
fn stat_block(s: &[i64; 10]) -> Value {
    json!({
        "hits": s[0],
        "shots": s[1],
        "kills": s[2],
        "points": s[3],
        "deaths": s[4],
        "wins": s[5],
        "sorties": s[6],
        "base_captures": s[7],
        "capture_count": s[8],
        "tippings": s[9],
    })
}

/// Indices of the pilots in the full ranking list. All of them while they fit;
/// above `max` the best of every single sum (round and turn) are kept first,
/// so each category still has its leaders.
pub fn ranking_selection(standings: &[&Standing], max: usize) -> HashSet<usize> {
    if standings.len() <= max {
        return (0..standings.len()).collect();
    }
    let mut best = vec![usize::MAX; standings.len()];
    for period in 0..2 {
        for k in 0..10 {
            let value = |i: usize| if period == 0 { standings[i].stats[k] } else { standings[i].turn_stats[k] };
            let mut order: Vec<usize> = (0..standings.len()).filter(|&i| value(i) > 0).collect();
            order.sort_by(|&a, &b| value(b).cmp(&value(a)).then(a.cmp(&b)));
            for (rank, i) in order.into_iter().enumerate() {
                best[i] = best[i].min(rank);
            }
        }
    }
    let mut order: Vec<usize> = (0..standings.len()).collect();
    // `standings` comes sorted by kill points: the index breaks ties.
    order.sort_by_key(|&i| (best[i], i));
    order.into_iter().take(max).collect()
}

/// Serialize with the size limit (compact: the file is for programs, and the
/// full ranking list needs the room).
pub fn render(doc: &Value) -> Result<Vec<u8>, String> {
    let mut out = serde_json::to_vec(doc).map_err(|e| e.to_string())?;
    out.push(b'\n');
    if out.len() > MAX_BYTES {
        return Err(format!("public document too large ({} bytes, limit {MAX_BYTES})", out.len()));
    }
    Ok(out)
}

/// Write `path` atomically: temporary file in the same directory, flush, rename.
/// Readable for everyone (public data; steelnet runs as another user).
pub fn write_atomic(path: &Path, data: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let dir = path.parent().ok_or_else(|| std::io::Error::other("no parent directory"))?;
    std::fs::create_dir_all(dir)?;
    let name = path.file_name().and_then(|n| n.to_str()).ok_or_else(|| std::io::Error::other("bad file name"))?;
    let tmp = dir.join(format!(".{name}.tmp"));
    {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(data)?;
        f.sync_all()?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o644))?;
    }
    std::fs::rename(&tmp, path)
}

/// Directory of the pilot files: `<out_dir>/<campaign>/pilots`.
pub fn pilot_dir(cfg: &PublicConfig) -> PathBuf {
    cfg.out_dir.join(&cfg.campaign).join("pilots")
}

/// Render the pilot files (size limit each). Any error: nothing is written.
pub fn render_pilots(docs: &[(String, Value)]) -> Result<Vec<(String, Vec<u8>)>, String> {
    docs.iter()
        .map(|(slug, doc)| {
            if !valid_slug(slug) {
                return Err(format!("bad pilot slug {slug:?}"));
            }
            let mut out = serde_json::to_vec(doc).map_err(|e| e.to_string())?;
            out.push(b'\n');
            if out.len() > MAX_PILOT_BYTES {
                return Err(format!("pilot file {slug} too large ({} bytes)", out.len()));
            }
            Ok((slug.clone(), out))
        })
        .collect()
}

/// Write the pilot files: changed ones atomically (unchanged files keep their
/// time stamp, so their ETag stays), then remove the files of pilots that are
/// no longer listed. Only names of the slug form are ever touched.
/// Returns (written, removed).
pub fn sync_pilot_files(dir: &Path, files: &[(String, Vec<u8>)]) -> std::io::Result<(usize, usize)> {
    std::fs::create_dir_all(dir)?;
    let mut written = 0;
    let keep: HashSet<String> = files.iter().map(|(s, _)| format!("{s}.json")).collect();
    for (slug, data) in files {
        let path = dir.join(format!("{slug}.json"));
        if std::fs::read(&path).is_ok_and(|old| old == *data) {
            continue;
        }
        write_atomic(&path, data)?;
        written += 1;
    }
    let mut removed = 0;
    for e in std::fs::read_dir(dir)? {
        let e = e?;
        let Some(name) = e.file_name().to_str().map(str::to_owned) else { continue };
        let ours = name.strip_suffix(".json").is_some_and(valid_slug)
            || name.strip_prefix('.').and_then(|n| n.strip_suffix(".json.tmp")).is_some_and(valid_slug);
        if ours && !keep.contains(&name) && e.file_type()?.is_file() {
            std::fs::remove_file(e.path())?;
            removed += 1;
        }
    }
    Ok((written, removed))
}

/// Key for the placeholder numbers: read, or create once (32 random bytes, 0600).
pub fn load_or_create_key(path: &Path) -> std::io::Result<Vec<u8>> {
    match std::fs::read(path) {
        Ok(k) if k.len() >= 16 => return Ok(k),
        Ok(_) => return Err(std::io::Error::other(format!("{}: key too short", path.display()))),
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e),
        Err(_) => {}
    }
    let mut key = vec![0u8; 32];
    {
        use std::io::Read;
        std::fs::File::open("/dev/urandom")
            .and_then(|mut r| r.read_exact(&mut key))
            .map_err(|e| std::io::Error::other(format!("no random source for {}: {e}", path.display())))?;
    }
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(path)?;
        f.write_all(&key)?;
        f.sync_all()?;
    }
    #[cfg(not(unix))]
    std::fs::write(path, &key)?;
    Ok(key)
}

/// `sbcampaign <config> --public-export`: collect, build, write. Exit code != 0 on any error;
/// the previous file then stays as it was.
pub async fn run(pool: &PgPool, root: &toml::Table) -> Result<(), Box<dyn std::error::Error>> {
    let cfg = PublicConfig::from_toml(root)?;
    let key = load_or_create_key(&cfg.name_key)?;
    let snap = collect(pool, cfg.battles, None).await?;
    let (doc, pilots) = build_all(&cfg, &snap, &mut Placeholders::new(&key, &cfg.season));
    let bytes = render(&doc)?;
    let pilot_files = render_pilots(&pilots)?;
    // Pilot files first, so every slug in the new document has its file; old
    // files go only after the document no longer names them.
    let dir = pilot_dir(&cfg);
    let (written, _) = sync_pilot_files(&dir, &pilot_files)?;
    let path = cfg.out_file();
    write_atomic(&path, &bytes)?;
    let (_, removed) = sync_pilot_files(&dir, &pilot_files)?;
    eprintln!(
        "public export: {} round {} turn {}, {} pilots, {} battles, {} matches, {} bytes; pilot files {} ({} written, {} removed)",
        cfg.campaign,
        snap.period.round,
        snap.period.turn,
        snap.standings.len(),
        snap.battles.len(),
        doc["matches"].as_array().map_or(0, Vec::len),
        bytes.len(),
        pilot_files.len(),
        written,
        removed
    );
    Ok(())
}
