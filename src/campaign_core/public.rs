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
/// Longest display name that is shown (the name rules allow 15 characters).
const MAX_NAME_CHARS: usize = 16;

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
    pub battles: usize,
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
            battles: count(p, "battles", 30, MAX_BATTLES)?,
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

/// Sums of one player in the current round. `stats[i]` = `stat{i+1}`.
#[derive(Debug, Clone)]
pub struct Standing {
    pub player: Uuid,
    pub name: Option<String>,
    pub faction: Option<String>,
    pub stats: [i64; 10],
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

    let rows: Vec<(Uuid, Option<String>, Option<String>, Vec<i64>)> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT l.xuid, d.name, a.faction::text,
                ARRAY[SUM(l.stat1), SUM(l.stat2), SUM(l.stat3), SUM(l.stat4), SUM(l.stat5),
                      SUM(l.stat6), SUM(l.stat7), SUM(l.stat8), SUM(l.stat9), SUM(l.stat10)]::bigint[]
         FROM public.leaderboard l
         LEFT JOIN public.accounts a ON a.xuid = l.xuid
         LEFT JOIN core.display_name d ON d.player_id = l.xuid
         WHERE l.round = $1 AND l.xuid NOT IN {HIDDEN}
         GROUP BY l.xuid, d.name, a.faction"
    )))
    .bind(round)
    .fetch_all(&mut *tx)
    .await?;
    let standings = rows
        .into_iter()
        .map(|(player, name, faction, s)| {
            let mut stats = [0i64; 10];
            for (i, v) in s.into_iter().take(10).enumerate() {
                stats[i] = v;
            }
            Standing { player, name, faction, stats }
        })
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
    let rows: Vec<(DateTime<Utc>, i16, i16, String, Uuid, Option<String>, Vec<i64>)> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT g.at, g.turn, g.\"map\", g.faction::text, g.player_id, d.name, g.points
         FROM public.gateway_result g
         LEFT JOIN core.display_name d ON d.player_id = g.player_id
         WHERE g.kind = 'user_points' AND {open}
         ORDER BY g.at DESC, g.result_id DESC LIMIT $2"
    )))
    .bind(round)
    .bind(battles as i64)
    .fetch_all(&mut *tx)
    .await?;
    let battles = rows
        .into_iter()
        .map(|(at, turn, map, faction, player, name, points)| Battle { at, turn, map, faction, player, name, points })
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
    tx.rollback().await?;
    Ok(Snapshot { now, period, standings, sectors, battles, battles_round, sides })
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

/// Build the public document. Pure: everything comes from the snapshot.
pub fn build(cfg: &PublicConfig, snap: &Snapshot, names: &mut Placeholders) -> Value {
    let period = &snap.period;
    // Placeholder numbers for everyone who may appear, before any is used.
    let mut everyone: Vec<Uuid> = snap.standings.iter().map(|s| s.player).collect();
    everyone.extend(snap.battles.iter().map(|b| b.player));
    names.assign(&everyone);
    let mut shown = |player: &Uuid, name: Option<&str>| clean_name(name).unwrap_or_else(|| names.name(player));

    // Sectors of the round.
    let mut tide_by_index: HashMap<usize, [i64; 3]> = HashMap::new();
    for t in &snap.sectors {
        let idx = t.map_id - cfg.map_base;
        if (0..SECTOR_COUNT).contains(&idx) {
            tide_by_index.insert(idx as usize, t.points);
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
            "captures": s.stats[7],
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

    // Side summaries.
    let mut pilot_points = [0i64; 3];
    for s in &snap.standings {
        if let Some(i) = s.faction.as_deref().and_then(side_index) {
            pilot_points[i] += s.stats[3];
        }
    }
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
                "at": ts(b.at.with_timezone(&Utc) - chrono::Duration::seconds(b.at.timestamp() % 60)),
                "turn": b.turn,
                "sector": sector_of(b.turn, b.map, cfg.map_base),
                "side": side_index(&b.faction).map(|i| SIDES[i]),
                "pilot": shown(&b.player, b.name.as_deref()),
                "result": result,
                "points": st(3),
                "kills": st(2),
            })
        })
        .collect();

    let turn_minutes = cfg.turn_length.as_secs() / 60;
    let started = period.ends_at - chrono::Duration::seconds(cfg.turn_length.as_secs() as i64);
    json!({
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
        "battles": battles,
        "totals": {"ranked_pilots": rows.len(), "battles_round": snap.battles_round},
    })
}

/// Serialize with the size limit.
pub fn render(doc: &Value) -> Result<Vec<u8>, String> {
    let mut out = serde_json::to_vec_pretty(doc).map_err(|e| e.to_string())?;
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
    let doc = build(&cfg, &snap, &mut Placeholders::new(&key, &cfg.season));
    let bytes = render(&doc)?;
    let path = cfg.out_file();
    write_atomic(&path, &bytes)?;
    eprintln!(
        "public export: {} round {} turn {}, {} pilots, {} battles, {} bytes",
        cfg.campaign,
        snap.period.round,
        snap.period.turn,
        snap.standings.len(),
        snap.battles.len(),
        bytes.len()
    );
    Ok(())
}
