pub mod rx;
pub mod tx;

use crate::{
    client::{Client, ClientError, Faction, Language},
    get_config, get_pool,
};
use futures::future::join_all;
use sqlx::{self, Postgres, QueryBuilder};
use std::{
    array,
    collections::HashMap,
    ffi::CStr,
    fmt,
    sync::LazyLock,
    time::Duration,
};
use thiserror::Error;
use tokio::{
    sync::Mutex,
    time::timeout,
};
use tx::TxCommand;
use uuid::Uuid;

pub struct CampaignCodec;

#[derive(Error, Debug)]
pub enum CampaignCodecError {
    #[error("Invalid header")]
    InvalidHeader,

    #[error("Invalid faction id: {0}")]
    InvalidFaction(u8),

    #[error("Invalid language id: {0}")]
    InvalidLanguage(u8),

    #[error("Invalid category id: {0}")]
    InvalidCategory(u8),

    #[error("Pilot data length was wrong: {0}")]
    InvalidPilotDataLength(usize),

    #[error("War Tide does not sum to 100: HSD {0}, PRF {1}, RB {2}")]
    InvalidWarTide(u8, u8, u8),

    #[error("Ranks must either be 0, 1, or 2: HSD {0}, PRF {1}, RB {2}")]
    InvalidRanks(u8, u8, u8),

    #[error("MOTD is too long: {0}")]
    InvalidMOTD(usize),

    #[error("Name is invalid. length: {0}, is_ascii: {1}")]
    InvalidName(usize, bool),

    #[error("Invalid VT List. length: {0}")]
    InvalidListSize(usize),

    #[error("Not enough data in payload")]
    InsufficientData(#[from] bytes::TryGetError),

    #[error("UTF-8 error occured")]
    Utf8(#[from] std::str::Utf8Error),

    #[error("Disk I/O error occurred")]
    Io(#[from] std::io::Error),
}

const NAME_CSTR_LEN: usize = 16;

const PILOT_DATA_NAME_OFFSET: usize = 40;
pub const PILOT_DATA_LENGTH: u16 = 1816;
type PilotData = [u8; PILOT_DATA_LENGTH as usize];

pub fn pilot_data_decode_name(data: &PilotData) -> Result<String, CampaignCodecError> {
    let mut name_bytes = [0u8; NAME_CSTR_LEN + 1];
    name_bytes[0..NAME_CSTR_LEN]
        .copy_from_slice(&data[PILOT_DATA_NAME_OFFSET..PILOT_DATA_NAME_OFFSET + NAME_CSTR_LEN]);
    let name_str = CStr::from_bytes_until_nul(&name_bytes)
        .expect("Missing null byte")
        .to_str()?;
    return Ok(name_str.to_owned());
}

pub fn pilot_data_from_slice(data: &[u8]) -> Result<PilotData, CampaignCodecError> {
    data.try_into()
        .map_err(|_| CampaignCodecError::InvalidPilotDataLength(data.len()))
}

pub type VTID = u16;

pub struct VTEntry {
    id: VTID,
    serial: u32,
}

impl VTEntry {
    pub fn new(id: VTID, serial: u32) -> Self {
        VTEntry { id, serial }
    }
}

pub type VTEntryList = Vec<VTEntry>;

pub type VTIDList = Vec<VTID>;

pub struct ShopEntry {
    id: VTID,
    limit: u32,
    cost: u16,
}

pub type ShopEntryList = Vec<ShopEntry>;

pub enum RankCategory {
    None = 0,
    WeaponAccuracy = 1, // Awarded
    KillCount = 2,
    KillPoints = 3, // Awarded
    DeathCount = 4,
    MissionVictories = 5, // Awarded
    BaseCaptures = 6,     // Awarded
    CaptureCount = 7,
    Tippings = 8, // Number of times fallen over
}

impl fmt::Display for RankCategory {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::None => "None",
            Self::WeaponAccuracy => "WeaponAccuracy",
            Self::KillCount => "KillCount",
            Self::KillPoints => "KillPoints",
            Self::DeathCount => "DeathCount",
            Self::MissionVictories => "MissionVictories",
            Self::BaseCaptures => "BaseCaptures",
            Self::CaptureCount => "CaptureCount",
            Self::Tippings => "Tippings",
        })
    }
}

impl RankCategory {
    fn query_sum_clause(&self) -> &'static str {
        match self {
            Self::None => "",

            Self::WeaponAccuracy => "(SUM(stat1) * 1000) as scoreNum, SUM(stat2) as scoreDiv",
            Self::KillPoints => "SUM(stat4) as scoreNum, 1 as scoreDiv",
            Self::MissionVictories => "(SUM(stat6) * 1000) as scoreNum, SUM(stat7) as scoreDiv",
            Self::BaseCaptures => "SUM(stat8) as scoreNum, 1 as scoreDiv",

            Self::KillCount => "SUM(stat3) as scoreNum, 1 as scoreDiv",
            Self::DeathCount => "SUM(stat5) as scoreNum, 1 as scoreDiv",
            Self::CaptureCount => "SUM(stat9) as scoreNum, 1 as scoreDiv",
            Self::Tippings => "SUM(stat10) as scoreNum, 1 as scoreDiv",
        }
    }

    fn query_where_clause(&self, is_turn: bool) -> &'static str {
        match self {
            Self::None => "",

            Self::WeaponAccuracy => {
                if is_turn {
                    "scoreDiv >= 50"
                } else {
                    "scoreDiv >= 200"
                }
            }
            Self::MissionVictories => {
                if is_turn {
                    "scoreDiv >= 5"
                } else {
                    "scoreDiv >= 20"
                }
            }

            Self::KillPoints
            | Self::BaseCaptures
            | Self::KillCount
            | Self::DeathCount
            | Self::CaptureCount
            | Self::Tippings => "scoreNum > 0",
        }
    }
}

pub struct RankEntry {
    rank: u32,
    pilot_name: String,
    xlive_name: String,
    score: u32,
}

pub type RankEntryList = Vec<RankEntry>;

pub struct AwardEntry {
    round: u16,
    turn: u16,
    id: VTID,
    faction: Faction,
}

pub type AwardEntryList = Vec<AwardEntry>;

pub struct AwardRankEntry {
    category: RankCategory,
    rank: u32,
}

pub type AwardRankEntryList = Vec<AwardRankEntry>;

const TURNS_PER_ROUND: u16 = 8;
const RB_START_TURN: u16 = 3;
const MAP_COUNT: usize = 24;

type CommandResult = Result<TxCommand, ClientError>;

// War Management
pub async fn turn_days(_client: &mut Client) -> CommandResult {
    //let pg = get_pool();

    // Recover sortied VTs from a disconnected match
    /*
    sqlx::query("UPDATE garage SET sortied = false WHERE xuid = $1")
        .bind(client.xuid)
        .execute(pg)
        .await?;
    */

    let cfg = get_config();

    Ok(TxCommand::TurnDays {
        days: cfg.turn_days,
    })
}

#[derive(sqlx::FromRow)]
struct RoundTurnTime {
    round: i16,
    turn: i16,
    remaining: i32,
}

pub async fn campaign_period(_client: &mut Client) -> CommandResult {
    let pg = get_pool();

    let round_turn_time: RoundTurnTime = sqlx::query_as("SELECT round, turn, EXTRACT(epoch FROM (period - now()))::int AS remaining FROM settings ORDER BY round DESC, turn DESC LIMIT 1").fetch_one(pg).await?;

    if round_turn_time.round < 0 {
        return Err(ClientError::Runtime("campaign_period.round < 0".to_owned()));
    }

    if round_turn_time.turn < 0 {
        return Err(ClientError::Runtime("campaign_period.turn < 0".to_owned()));
    }

    if round_turn_time.remaining < 0 {
        return Err(ClientError::Runtime(
            "campaign_period.remaining < 0".to_owned(),
        ));
    }

    let cfg = get_config();
    let turn_seconds = Duration::from_hours(cfg.turn_days as u64 * 24);

    // Add the remaining seconds in the current turn to the number of seconds left in all remaining turns
    let seconds = round_turn_time.remaining
        + (TURNS_PER_ROUND as i32 - round_turn_time.turn as i32) * turn_seconds.as_secs() as i32;

    Ok(TxCommand::CampaignPeriod {
        round: round_turn_time.round as u16,
        turn: round_turn_time.turn as u16,
        seconds,
    })
}

#[derive(sqlx::FromRow, Default)]
struct WarTide {
    hsd: i32,
    prf: i32,
    rb: i32,
}

fn war_tide_as_wins(war_tides: &[WarTide]) -> WarTide {
    let mut wins = WarTide {
        hsd: 0,
        prf: 0,
        rb: 0,
    };

    for tide in war_tides.iter() {
        if tide.hsd > tide.prf && tide.hsd > tide.rb {
            wins.hsd += 1;
        } else if tide.prf > tide.hsd && tide.prf > tide.rb {
            wins.prf += 1;
        } else if tide.rb > tide.prf && tide.rb > tide.hsd {
            wins.rb += 1;
        }
    }
    wins
}

pub async fn war_tide(_client: &mut Client, round: u16, turn: u16, map: u8) -> CommandResult {
    let pg = get_pool();

    let map_id = map as i16 + (turn as i16 - 1) * 3;

    let war_tide: WarTide =
        match sqlx::query_as("SELECT hsd, prf, rb FROM wartide WHERE round = $1 AND map = $2")
            .bind(round as i16)
            .bind(map_id)
            .fetch_one(pg)
            .await
        {
            Ok(wt) => wt,
            Err(sqlx::Error::RowNotFound) => WarTide::default(),
            err => err?,
        };

    // Give each faction some starting points so that early victories dont skew the pie-chart
    const STARTING_POINTS: f64 = 300.0;

    let hsd_points = war_tide.hsd as f64 + STARTING_POINTS;
    let prf_points = war_tide.prf as f64 + STARTING_POINTS;
    let rb_points = if turn >= RB_START_TURN {
        war_tide.rb as f64 + STARTING_POINTS
    } else {
        0.0
    };

    let total_points = hsd_points + prf_points + rb_points;

    let hsd = (100.0 * hsd_points / total_points) as u8;
    let prf = (100.0 * prf_points / total_points) as u8;
    let rb = (100.0 * rb_points / total_points) as u8;

    let error_percent = 100 - (hsd + prf + rb);

    Ok(TxCommand::WarTide {
        status: 0,
        hsd: hsd + error_percent,
        prf,
        rb,
    })
}

pub async fn war_influence(_client: &mut Client, round: u16, turn: u16) -> CommandResult {
    let pg = get_pool();

    let war_tides: Vec<WarTide> = if round > 0 {
        sqlx::query_as("SELECT hsd, prf, rb FROM wartide WHERE round = $1").bind(round as i16)
    } else {
        sqlx::query_as(
            "SELECT hsd, prf, rb FROM wartide WHERE round IN (SELECT MAX(round) FROM wartide)",
        )
    }
    .fetch_all(pg)
    .await?;

    let wins = war_tide_as_wins(&war_tides);

    let mut hsd_wins = wins.hsd as f64;
    let mut prf_wins = wins.prf as f64;
    let mut rb_wins = wins.rb as f64;

    let played_map_count = war_tides.iter().count();

    let mut unplayed = if played_map_count < MAP_COUNT {
        (MAP_COUNT - played_map_count) as f64
    } else {
        0.0
    };

    if turn >= RB_START_TURN {
        unplayed /= 3.0;
        hsd_wins += unplayed;
        prf_wins += unplayed;
        rb_wins += unplayed;
    } else {
        unplayed /= 2.0;
        hsd_wins += unplayed;
        prf_wins += unplayed;
    }

    let total_wins = hsd_wins + prf_wins + rb_wins;

    let hsd = (100.0 * hsd_wins / total_wins) as u8;
    let prf = (100.0 * prf_wins / total_wins) as u8;
    let rb = (100.0 * rb_wins / total_wins) as u8;

    let error_percent = 100 - (hsd + prf + rb);

    Ok(TxCommand::WarInfluence {
        status: 0,
        hsd: hsd + error_percent,
        prf,
        rb,
    })
}

pub async fn round_points(_client: &mut Client, round: u16) -> CommandResult {
    let pg = get_pool();

    let war_tides: Vec<WarTide> =
        sqlx::query_as("SELECT hsd, prf, rb FROM wartide WHERE round = $1")
            .bind(round as i16)
            .fetch_all(pg)
            .await?;

    let wins = war_tide_as_wins(&war_tides);

    // Ranks, 0 = 1st, 1 = 2nd, 2 = 3rd, all other values invalid
    let mut hsd = 2;
    let mut prf = 2;
    let mut rb = 2;

    if wins.hsd >= wins.prf {
        hsd -= 1;
    }
    if wins.hsd >= wins.rb {
        hsd -= 1;
    }

    if wins.prf >= wins.hsd {
        prf -= 1;
    }
    if wins.prf >= wins.rb {
        prf -= 1;
    }

    if wins.rb >= wins.hsd {
        rb -= 1;
    }
    if wins.rb >= wins.prf {
        rb -= 1;
    }

    let cfg = get_config();

    Ok(TxCommand::RoundPoints {
        status: 0,
        points: cfg.round_end_points,
        hsd,
        prf,
        rb,
    })
}

// Mission Management
pub async fn analyze(_client: &mut Client, _data: [u32; 10]) -> CommandResult {
    Ok(TxCommand::Analyze)
}

pub async fn add_user_point(
    client: &mut Client,
    round: u16,
    turn: u16,
    map: u8,
    points: [u32; 10],
) -> CommandResult {
    if matches!(client.faction, Faction::None) {
        return Err(ClientError::Runtime(
            "add_user_point.faction is invalid".to_owned(),
        ));
    }

    let pg = get_pool();

    let mut query = sqlx::query("
        INSERT INTO leaderboard (xuid, faction, round, turn, \"map\", stat1, stat2, stat3, stat4, stat5, stat6, stat7, stat8, stat9, stat10)
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15)
        ON CONFLICT (xuid, faction, round, turn, \"map\") DO UPDATE SET
            stat1 = leaderboard.stat1 + excluded.stat1, stat2 = leaderboard.stat2 + excluded.stat2,
            stat3 = leaderboard.stat3 + excluded.stat3, stat4 = leaderboard.stat4 + excluded.stat4,
            stat5 = leaderboard.stat5 + excluded.stat5, stat6 = leaderboard.stat6 + excluded.stat6,
            stat7 = leaderboard.stat7 + excluded.stat7, stat8 = leaderboard.stat8 + excluded.stat8,
            stat9 = leaderboard.stat9 + excluded.stat9, stat10 = leaderboard.stat10 + excluded.stat10
        ").bind(client.xuid).bind(client.faction).bind(round as i16).bind(turn as i16).bind(map as i16);

    for p in points {
        query = query.bind(p as i64)
    }

    query.execute(pg).await?;

    Ok(TxCommand::AddUserPoint { status: 0 })
}

pub async fn add_mission_point(
    _client: &mut Client,
    round: u16,
    turn: u16,
    map: u8,
    points: u32,
    faction: Faction,
) -> CommandResult {
    let pg = get_pool();

    let map_id = map as i16 + (turn as i16 - 1) * 3;

    let query = match faction {
        Faction::Hsd => sqlx::query(
            "INSERT INTO wartide (round, \"map\", hsd) VALUES ($1, $2, $3) ON CONFLICT (round, \"map\") DO UPDATE SET hsd = wartide.hsd + excluded.hsd",
        ),
        Faction::Prf => sqlx::query(
            "INSERT INTO wartide (round, \"map\", prf) VALUES ($1, $2, $3) ON CONFLICT (round, \"map\") DO UPDATE SET prf = wartide.prf + excluded.prf",
        ),
        Faction::Rb => sqlx::query(
            "INSERT INTO wartide (round, \"map\", rb) VALUES ($1, $2, $3) ON CONFLICT (round, \"map\") DO UPDATE SET rb = wartide.rb + excluded.rb",
        ),
        _ => {
            return Err(ClientError::Runtime(
                "add_user_point.faction is invalid".to_owned(),
            ));
        }
    };

    query
        .bind(round as i16)
        .bind(map_id)
        .bind(points as i64)
        .execute(pg)
        .await?;

    Ok(TxCommand::AddMissionPoint { status: 0 })
}

pub async fn free_message(client: &mut Client, language: Language) -> CommandResult {
    let pg = get_pool();

    sqlx::query("UPDATE accounts SET language = $2 WHERE xuid = $1")
        .bind(client.xuid)
        .bind(language)
        .execute(pg)
        .await?;

    client.language = Some(language);

    let cfg = get_config();

    let message = match language {
        Language::Ja => cfg.motd_ja.as_str(),
        Language::En => cfg.motd_en.as_str(),
    };

    Ok(TxCommand::FreeMessage {
        status: 0,
        message: message.to_owned(),
    })
}

pub async fn fixed_message(client: &mut Client) -> CommandResult {
    let pg = get_pool();

    let round_turn_time: RoundTurnTime = sqlx::query_as("SELECT round, turn, EXTRACT(epoch FROM (period - now()))::int AS remaining FROM settings ORDER BY round DESC, turn DESC LIMIT 1").fetch_one(pg).await?;

    if round_turn_time.round < 0 {
        return Err(ClientError::Runtime("fixed_message.round < 0".to_owned()));
    }

    if round_turn_time.turn < 0 {
        return Err(ClientError::Runtime("fixed_message.turn < 0".to_owned()));
    }

    let remaining = Duration::from_secs(
        u64::try_from(round_turn_time.remaining)
            .map_err(|_| ClientError::Runtime("fixed_message.remaining < 0".to_owned()))?,
    );

    #[derive(sqlx::FromRow)]
    struct AwardCount {
        count: i64,
    }

    let award_count: AwardCount = sqlx::query_as("SELECT COUNT(*) FROM awards WHERE xuid = $1")
        .bind(client.xuid)
        .fetch_one(pg)
        .await?;

    let cfg = get_config();

    let mut messages = Vec::new();

    if matches!(client.faction, Faction::None) {
        if round_turn_time.turn == 1 {
            messages.push(0); // A new round has begun, pick a faction
        } else {
            messages.push(8); // You can now transfer to another faction
        }
    }

    if round_turn_time.turn > 1 && remaining > Duration::from_hours((cfg.turn_days as u64 - 2) * 24)
    {
        messages.push(1); // A new turn has begun
    }

    if remaining < Duration::from_hours(2 * 24) {
        if round_turn_time.turn == 8 {
            if remaining < Duration::from_hours(24) {
                messages.push(4); // Round is over
            }
            messages.push(3); // Time remaining in round
        } else {
            messages.push(2); // Time remaining in turn
        }
    }

    if round_turn_time.turn == 3 {
        messages.push(7); // RB appears
    } else if round_turn_time.turn == 5 {
        messages.push(9); // Jar appears
    }

    if award_count.count > 0 {
        messages.push(10); // Awards can be claimed
    }

    Ok(TxCommand::FixedMessage {
        status: 0,
        messages,
    })
}

// Pilot Management
pub async fn get_user_info(client: &mut Client) -> CommandResult {
    let pg = get_pool();

    #[derive(sqlx::FromRow)]
    struct PilotDataRow {
        pilot: Option<Vec<u8>>,
    }

    let pilot_data_row: PilotDataRow = sqlx::query_as("SELECT pilot FROM accounts WHERE xuid = $1")
        .bind(client.xuid)
        .fetch_one(pg)
        .await?;

    if let Some(pilot_bytes) = pilot_data_row.pilot {
        let pilot_data = pilot_data_from_slice(&pilot_bytes)?;
        let pilot_name = pilot_data_decode_name(&pilot_data)?;

        Ok(TxCommand::GetUserInfo {
            status: 0,
            pilot_name,
            faction: client.faction,
            pilot_data,
        })
    } else {
        Ok(TxCommand::GetUserInfo {
            status: 2,
            pilot_name: String::new(),
            faction: Faction::None,
            pilot_data: [0; _],
        })
    }
}

pub async fn new_user_info(
    client: &mut Client,
    _pilot_name: String,
    xlive_name: String,
    faction: Faction,
    pilot_data: PilotData,
) -> CommandResult {
    let pg = get_pool();

    sqlx::query("UPDATE accounts SET xname = $2, faction = $3, pilot = $4 WHERE xuid = $1")
        .bind(client.xuid)
        .bind(xlive_name)
        .bind(faction)
        .bind(&pilot_data)
        .execute(pg)
        .await?;

    client.faction = faction;

    Ok(TxCommand::NewUserInfo { status: 0 })
}

pub async fn set_user_info(client: &mut Client, pilot_data: PilotData) -> CommandResult {
    let pg = get_pool();

    sqlx::query("UPDATE accounts SET pilot = $2 WHERE xuid = $1")
        .bind(client.xuid)
        .bind(&pilot_data)
        .execute(pg)
        .await?;

    Ok(TxCommand::SetUserInfo { status: 0 })
}

pub async fn del_user_info(client: &mut Client) -> CommandResult {
    let pg = get_pool();

    let mut transaction = pg.begin().await?;

    sqlx::query("INSERT INTO graveyard (xuid, pilot, tod) SELECT xuid, pilot, NOW() FROM accounts WHERE xuid = $1")
        .bind(client.xuid)
        .execute(&mut *transaction)
        .await?;

    sqlx::query("UPDATE accounts SET faction = 'none', pilot = NULL WHERE xuid = $1")
        .bind(client.xuid)
        .execute(&mut *transaction)
        .await?;

    transaction.commit().await?;

    client.faction = Faction::None;

    Ok(TxCommand::DelUserInfo { status: 0 })
}

pub async fn set_user_faction(client: &mut Client, faction: Faction) -> CommandResult {
    let pg = get_pool();

    sqlx::query("UPDATE accounts SET faction = $2 WHERE xuid = $1")
        .bind(client.xuid)
        .bind(faction)
        .execute(pg)
        .await?;

    client.faction = faction;

    // Return status is always 1 to kick the user out of Campaign so that their garage reloads correctly
    Ok(TxCommand::SetUserFaction { status: 1 })
}

#[derive(sqlx::FromRow)]
struct GarageTypeSerial {
    #[sqlx(rename = "type")]
    id: i16,
    serial: i64,
}

// Garage Management
pub async fn vt_owned(client: &mut Client) -> CommandResult {
    let pg = get_pool();

    let garage_list: Vec<GarageTypeSerial> = sqlx::query_as(
        "SELECT type, serial FROM garage WHERE sortied = false AND faction = $2 AND xuid = $1",
    )
    .bind(client.xuid)
    .bind(client.faction)
    .fetch_all(pg)
    .await?;

    let vt_list: VTEntryList = garage_list
        .iter()
        .map(|vt| VTEntry {
            id: vt.id as u16,
            serial: vt.serial as u32,
        })
        .collect();

    Ok(TxCommand::VtOwned { status: 0, vt_list })
}

pub async fn vt_add(client: &mut Client, vt_list_in: VTIDList) -> CommandResult {
    let mut vt_add_counts: HashMap<VTID, u16> = HashMap::with_capacity(35);

    // Count the number of each VT type to add
    for &id in vt_list_in.iter() {
        *vt_add_counts.entry(id).or_insert(0) += 1;
    }

    let mut vt_list = Vec::new();

    let pg = get_pool();

    #[derive(sqlx::FromRow)]
    struct ShopSerialStock {
        #[sqlx(rename = "type")]
        id: i16,
        serial: i64,
        vtstock: Option<i16>,
    }

    let mut transaction = pg.begin().await?;

    // Fetch all relevant VT entries from the shop and lock them to prevent duplicate VTs from being created
    let mut shop_query_builder: QueryBuilder<Postgres> =
        QueryBuilder::new("SELECT \"type\", \"serial\", vtstock FROM shop WHERE \"type\" IN (");

    let mut shop_query_separated = shop_query_builder.separated(", ");
    for &id in vt_list_in.iter() {
        shop_query_separated.push_bind(id as i16);
    }
    shop_query_separated.push_unseparated(") FOR UPDATE");

    let shop_query = shop_query_builder.build_query_as::<ShopSerialStock>();
    let shop_serial_stocks = shop_query.fetch_all(&mut *transaction).await?;

    for serial_stocks in shop_serial_stocks.iter() {
        let count = vt_add_counts[&(serial_stocks.id as u16)];

        // Insufficient stock to complete this transaction
        if let Some(vtstock) = serial_stocks.vtstock
            && vtstock < count as i16
        {
            return Err(ClientError::Runtime(
                "vt_add insufficient vt stock".to_owned(),
            ));
        }

        // Update the serial number and the remaining VT stock in the shop
        // NULL (unlimited) stays NULL; GREATEST(NULL - n, 0) would be 0 in PostgreSQL
        sqlx::query("UPDATE shop SET \"serial\" = \"serial\" + $2, vtstock = vtstock - LEAST(vtstock, $3) WHERE type = $1").bind(serial_stocks.id).bind(count as i64).bind(count as i16).execute(&mut *transaction).await?;

        // Insert the new VTs into the garage
        for i in 0..count {
            let serial = serial_stocks.serial + i as i64;
            sqlx::query(
                "INSERT INTO garage (xuid, \"type\", \"serial\", faction) VALUES ($1, $2, $3, $4)",
            )
            .bind(client.xuid)
            .bind(serial_stocks.id)
            .bind(serial)
            .bind(client.faction)
            .execute(&mut *transaction)
            .await?;

            vt_list.push(VTEntry {
                id: serial_stocks.id as u16,
                serial: serial as u32,
            });
        }
    }

    transaction.commit().await?;

    Ok(TxCommand::VtAdd { status: 0, vt_list })
}

pub async fn vt_del(client: &mut Client, vt_list_in: VTEntryList) -> CommandResult {
    let pg = get_pool();

    // Execute all queries in parallel. We want all of them to execute even if one fails
    let mut query_futures = Vec::with_capacity(vt_list_in.len());
    for vt in vt_list_in {
        query_futures.push(
        sqlx::query(
            "DELETE FROM garage WHERE xuid = $1 AND faction = $2 AND \"type\" = $3 AND \"serial\" = $4",
        ).bind(client.xuid).bind(client.faction).bind(vt.id as i16).bind(vt.serial as i64).execute(pg));
    }

    for query_result in join_all(query_futures).await {
        query_result?;
    }

    Ok(TxCommand::VtDel {
        status: 0,
        vt_list: Vec::new(),
    })
}

pub async fn vt_capture(
    client: &mut Client,
    pilot_name: String,
    vt_list_in: VTEntryList,
) -> CommandResult {
    let pg = get_pool();

    // Execute all queries in parallel. We want all of them to execute even if one fails
    let mut query_futures = Vec::with_capacity(vt_list_in.len());
    for vt in vt_list_in.iter() {
        query_futures.push(
            sqlx::query(
                "
                INSERT INTO garage (\"type\", \"serial\", xuid, faction)
                VALUES ($3, $4, $1, $2)
                ON CONFLICT (\"type\", \"serial\")
                DO UPDATE SET sortied = false, faction = excluded.faction, xuid = excluded.xuid
                ",
            )
            .bind(client.xuid)
            .bind(client.faction)
            .bind(vt.id as i16)
            .bind(vt.serial as i64)
            .execute(pg),
        );
    }

    for query_result in join_all(query_futures).await {
        query_result?;
    }

    Ok(TxCommand::VtCapture {
        status: 0,
        pilot_name,
        vt_list: vt_list_in,
    })
}

pub async fn vt_sortie(client: &mut Client, vt_list_in: VTEntryList) -> CommandResult {
    let pg = get_pool();

    let mut transaction = pg.begin().await?;

    for vt in vt_list_in {
        sqlx::query(
            "UPDATE garage SET sortied = true WHERE xuid = $1 AND faction = $2 AND \"type\" = $3 AND serial = $4"
        )
        .bind(client.xuid)
        .bind(client.faction)
        .bind(vt.id as i16)
        .bind(vt.serial as i64)
        .execute(&mut *transaction).await?;
    }

    transaction.commit().await?;

    Ok(TxCommand::VtSortie { status: 0 })
}

pub async fn vt_return(client: &mut Client, vt_list_in: VTEntryList) -> CommandResult {
    let pg = get_pool();

    let mut transaction = pg.begin().await?;

    for vt in vt_list_in.iter() {
        sqlx::query(
            "UPDATE garage SET sortied = false WHERE xuid = $1 AND faction = $2 AND \"type\" = $3 AND serial = $4"
        )
        .bind(client.xuid)
        .bind(client.faction)
        .bind(vt.id as i16)
        .bind(vt.serial as i64)
        .execute(&mut *transaction).await?;
    }

    transaction.commit().await?;

    Ok(TxCommand::VtReturn {
        status: 0,
        vt_list: vt_list_in,
    })
}

// Shop
pub async fn shop_list(client: &mut Client) -> CommandResult {
    let pg = get_pool();

    #[derive(sqlx::FromRow)]
    struct ShopCostStock {
        #[sqlx(rename = "type")]
        id: i16,
        cost: i32,
        vtstock: Option<i16>,
    }

    let shop_cost_stocks: Vec<ShopCostStock> = sqlx::query_as("SELECT \"type\", \"cost\", vtstock FROM shop WHERE faction = $1 AND COALESCE(vtstock, 1) > 0 AND turn <= (SELECT turn FROM settings ORDER BY round DESC, turn DESC LIMIT 1)").bind(client.faction).fetch_all(pg).await?;

    let mut shop_list: ShopEntryList = Vec::with_capacity(shop_cost_stocks.len());

    for cost_stock in shop_cost_stocks {
        shop_list.push(ShopEntry {
            id: cost_stock.id as u16,
            cost: cost_stock.cost as u16,
            limit: cost_stock.vtstock.map(|v| v as u32).unwrap_or(u32::MAX),
        });
    }

    Ok(TxCommand::ShopList {
        status: 0,
        shop_list,
    })
}

pub async fn vt_potential_cost(_client: &mut Client) -> CommandResult {
    Ok(TxCommand::VtPotentialCost)
}

// Message Board
pub async fn message_count(_client: &mut Client) -> CommandResult {
    // Message board is disabled, return an error code
    Ok(TxCommand::MessageCount {
        status: 1,
        start: 0,
        count: 0,
    })
}
pub async fn message_post(_client: &mut Client) -> CommandResult {
    // Message board is disabled, return an error code
    Ok(TxCommand::MessagePost { status: 1, id: 0 })
}

// Leaderboard
pub async fn rank_values(
    _client: &mut Client,
    round: u16,
    turn: u16,
    faction: Faction,
    category: RankCategory,
    start: u32,
    count: u16,
) -> CommandResult {
    let pg = get_pool();

    let is_turn = turn > 0;

    let mut query_builder: QueryBuilder<Postgres> = QueryBuilder::new(
        "SELECT xname, pilot, (scoreNum / scoreDiv)::smallint as score FROM (SELECT xuid, ",
    );

    query_builder.push(category.query_sum_clause());

    query_builder.push(" FROM leaderboard WHERE round = ");
    query_builder.push_bind(round as i16);

    if is_turn {
        query_builder.push(" AND turn = ");
        query_builder.push_bind(turn as i16);
    }

    if !matches!(faction, Faction::None) {
        query_builder.push(" AND faction = ");
        query_builder.push_bind(faction);
    }

    query_builder
        .push(" GROUP BY xuid) as lb INNER JOIN accounts ON lb.xuid = accounts.xuid WHERE ");
    query_builder.push(category.query_where_clause(is_turn));

    query_builder.push(" ORDER BY score DESC LIMIT ");
    query_builder.push_bind(count as i32);

    query_builder.push(" OFFSET ");
    query_builder.push_bind(start as i32 - 1);

    #[derive(sqlx::FromRow)]
    struct RankScore {
        xname: String,
        pilot: Vec<u8>,
        score: i16,
    }

    let query = query_builder.build_query_as::<RankScore>();
    let rank_scores = query.fetch_all(pg).await?;

    let mut rank_list = Vec::new();
    for (i, rank_score) in rank_scores.iter().enumerate() {
        let rank = start + i as u32;

        let pilot_data = pilot_data_from_slice(&rank_score.pilot)?;
        let pilot_name = pilot_data_decode_name(&pilot_data)?;

        rank_list.push(RankEntry {
            rank,
            pilot_name,
            xlive_name: rank_score.xname.clone(),
            score: rank_score.score as u32,
        });
    }

    Ok(TxCommand::RankValues {
        status: 0,
        rank_list,
    })
}

async fn rank_count_internal(
    count_opt: Option<&mut u32>,
    place_opt: Option<(&mut u32, Uuid)>,
    round: u16,
    turn: u16,
    category: RankCategory,
    faction: Faction,
) -> Result<(), ClientError> {
    let pg = get_pool();
    let is_turn = turn > 0;

    if let Some(count) = count_opt {
        let mut query_builder: QueryBuilder<Postgres> =
            QueryBuilder::new("SELECT COUNT(*) FROM (SELECT ");

        query_builder.push(category.query_sum_clause());

        query_builder.push(" FROM leaderboard WHERE round = ");
        query_builder.push_bind(round as i16);

        if is_turn {
            query_builder.push(" AND turn = ");
            query_builder.push_bind(turn as i16);
        }

        if !matches!(faction, Faction::None) {
            query_builder.push(" AND faction = ");
            query_builder.push_bind(faction);
        }

        query_builder.push(" GROUP BY xuid) as lb WHERE ");
        query_builder.push(category.query_where_clause(is_turn));

        #[derive(sqlx::FromRow)]
        struct ScoreCount {
            count: i64,
        }

        let query = query_builder.build_query_as::<ScoreCount>();
        let score_count = query.fetch_one(pg).await?;

        *count = score_count.count as u32;
    }

    if let Some((place, xuid)) = place_opt {
        let mut query_builder: QueryBuilder<Postgres> = QueryBuilder::new(
            "SELECT \"rank\" FROM (SELECT xuid, ROW_NUMBER() OVER(ORDER BY (scoreNum / scoreDiv) DESC) \"rank\" FROM (SELECT xuid, ",
        );

        query_builder.push(category.query_sum_clause());

        query_builder.push(" FROM leaderboard WHERE round = ");
        query_builder.push_bind(round as i16);

        if is_turn {
            query_builder.push(" AND turn = ");
            query_builder.push_bind(turn as i16);
        }

        if !matches!(faction, Faction::None) {
            query_builder.push(" AND faction = ");
            query_builder.push_bind(faction);
        }

        query_builder.push(" GROUP BY xuid) as lb WHERE ");
        query_builder.push(category.query_where_clause(is_turn));

        query_builder.push(" ) as lb_rank WHERE xuid = ");
        query_builder.push_bind(xuid);

        #[derive(sqlx::FromRow)]
        struct ScoreRank {
            rank: i64,
        }

        let query = query_builder.build_query_as::<ScoreRank>();
        let score_rank = match query.fetch_one(pg).await {
            Ok(sc) => sc,
            Err(sqlx::Error::RowNotFound) => ScoreRank { rank: 0 },
            err => err?,
        };

        *place = score_rank.rank as u32;
    }

    Ok(())
}

pub async fn rank_count_global(
    client: &mut Client,
    round: u16,
    turn: u16,
    category: RankCategory,
) -> CommandResult {
    let mut count = 0;
    let mut place = 0;
    rank_count_internal(
        Some(&mut count),
        Some((&mut place, client.xuid)),
        round,
        turn,
        category,
        Faction::None,
    )
    .await?;

    Ok(TxCommand::RankCountGlobal {
        status: 0,
        place,
        count,
    })
}

pub async fn rank_count_faction(
    client: &mut Client,
    round: u16,
    turn: u16,
    category: RankCategory,
    faction: Faction,
) -> CommandResult {
    let mut count = 0;
    let mut place = 0;
    rank_count_internal(
        Some(&mut count),
        Some((&mut place, client.xuid)),
        round,
        turn,
        category,
        faction,
    )
    .await?;

    Ok(TxCommand::RankCountFaction {
        status: 0,
        place,
        count,
    })
}

pub async fn rank_count_categories(_client: &mut Client, round: u16, turn: u16) -> CommandResult {
    let mut count = 0;
    rank_count_internal(
        Some(&mut count),
        None,
        round,
        turn,
        RankCategory::WeaponAccuracy,
        Faction::None,
    )
    .await?;

    Ok(TxCommand::RankCountCategories {
        status: 0,
        categories: array::from_fn(|_| count),
    })
}

// Awards
pub async fn award_available(client: &mut Client) -> CommandResult {
    let pg = get_pool();
    let cfg = get_config();

    #[derive(sqlx::FromRow)]
    struct AwardRow {
        round: i16,
        turn: i16,
        rank1: Option<i32>,
        rank2: Option<i32>,
        rank3: Option<i32>,
        rank4: Option<i32>,
    }

    let award_rows: Vec<AwardRow> = sqlx::query_as("SELECT round, turn, rank1, rank2, rank3, rank4 FROM awards WHERE xuid = $1 ORDER BY round ASC, turn ASC LIMIT 128").bind(client.xuid).fetch_all(pg).await?;

    const AWARD_NO_VT: VTID = 35;

    let mut award_list = Vec::new();
    for award in award_rows {
        if let Some(rank_val) = award
            .rank1
            .min(award.rank2.min(award.rank3.min(award.rank4)))
        {
            if rank_val < 0 {
                continue;
            }

            let vt_ids: &[VTID] = if award.turn >= 6 {
                // End of Turn Award
                match client.faction {
                    Faction::None => &[],
                    Faction::Hsd => &cfg.hsd_turn_awards,
                    Faction::Prf => &cfg.prf_turn_awards,
                    Faction::Rb => &cfg.rb_turn_awards,
                    Faction::Jar => &cfg.jar_turn_awards,
                }
            } else if award.turn == 0 {
                // End of Round Award
                match client.faction {
                    Faction::None => &[],
                    Faction::Hsd => &cfg.hsd_round_awards,
                    Faction::Prf => &cfg.prf_round_awards,
                    Faction::Rb => &cfg.rb_round_awards,
                    Faction::Jar => &cfg.jar_round_awards,
                }
            } else {
                &[]
            };

            award_list.push(AwardEntry {
                round: award.round as u16,
                turn: award.turn as u16,
                id: *vt_ids.get(rank_val as usize - 1).unwrap_or(&AWARD_NO_VT),
                faction: client.faction,
            })
        }
    }

    Ok(TxCommand::AwardAvailable {
        status: 0,
        award_list,
    })
}

pub async fn award_points(_client: &mut Client, is_turn: bool) -> CommandResult {
    let cfg = get_config();

    let points = if is_turn {
        cfg.turn_points.clone()
    } else {
        cfg.round_points.clone()
    };

    Ok(TxCommand::AwardPoints { status: 0, points })
}

pub async fn award_ranks(
    client: &mut Client,
    round: u16,
    turn: u16,
    _faction: Faction,
) -> CommandResult {
    let pg = get_pool();

    #[derive(sqlx::FromRow)]
    struct AwardRow {
        rank1: Option<i32>,
        rank2: Option<i32>,
        rank3: Option<i32>,
        rank4: Option<i32>,
    }

    let award_row: AwardRow = sqlx::query_as("SELECT rank1, rank2, rank3, rank4 FROM awards WHERE round = $2 AND turn = $3 AND xuid = $1").bind(client.xuid).bind(round as i16).bind(turn as i16).fetch_one(pg).await?;

    let mut award_list = Vec::new();

    if let Some(rank) = award_row.rank1 {
        award_list.push(AwardRankEntry {
            category: RankCategory::WeaponAccuracy,
            rank: rank as u32,
        })
    }

    if let Some(rank) = award_row.rank2 {
        award_list.push(AwardRankEntry {
            category: RankCategory::KillCount,
            rank: rank as u32,
        })
    }

    if let Some(rank) = award_row.rank3 {
        award_list.push(AwardRankEntry {
            category: RankCategory::MissionVictories,
            rank: rank as u32,
        })
    }

    if let Some(rank) = award_row.rank4 {
        award_list.push(AwardRankEntry {
            category: RankCategory::BaseCaptures,
            rank: rank as u32,
        })
    }

    Ok(TxCommand::AwardRanks {
        status: 0,
        award_list,
    })
}

pub async fn award_claim(
    client: &mut Client,
    round: u16,
    turn: u16,
    vt_list_in: VTIDList,
    pilot_data: PilotData,
) -> CommandResult {
    let pg = get_pool();

    let mut transaction = pg.begin().await?;

    // Claim the award
    sqlx::query("DELETE FROM awards WHERE xuid = $1 AND round = $2 AND turn = $3")
        .bind(client.xuid)
        .bind(round as i16)
        .bind(turn as i16)
        .execute(&mut *transaction)
        .await?;

    // Update pilot
    sqlx::query("UPDATE accounts SET pilot = $2 WHERE xuid = $1")
        .bind(client.xuid)
        .bind(pilot_data)
        .execute(&mut *transaction)
        .await?;

    // Only update VTs if requested
    let mut vt_list = Vec::new();
    if !vt_list_in.is_empty() {
        let mut vt_add_counts: HashMap<VTID, u16> = HashMap::with_capacity(35);

        // Count the number of each VT type to add
        for &id in vt_list_in.iter() {
            *vt_add_counts.entry(id).or_insert(0) += 1;
        }

        #[derive(sqlx::FromRow)]
        struct ShopSerial {
            #[sqlx(rename = "type")]
            id: i16,
            serial: i64,
        }

        // Fetch all relevant VT entries from the shop and lock them to prevent duplicate VTs from being created
        let mut shop_query_builder: QueryBuilder<Postgres> =
            QueryBuilder::new("SELECT \"type\", \"serial\" FROM shop WHERE \"type\" IN (");

        let mut shop_query_separated = shop_query_builder.separated(", ");
        for &id in vt_list_in.iter() {
            shop_query_separated.push_bind(id as i16);
        }
        shop_query_separated.push_unseparated(") FOR UPDATE");

        let shop_query = shop_query_builder.build_query_as::<ShopSerial>();
        let shop_serial_stocks = shop_query.fetch_all(&mut *transaction).await?;

        for serial_stocks in shop_serial_stocks.iter() {
            let count = vt_add_counts[&(serial_stocks.id as u16)];

            // Update the serial number in the shop
            sqlx::query("UPDATE shop SET \"serial\" = \"serial\" + $2 WHERE type = $1")
                .bind(serial_stocks.id)
                .bind(count as i64)
                .execute(&mut *transaction)
                .await?;

            // Insert the new VTs into the garage
            for i in 0..count {
                let serial = serial_stocks.serial + i as i64;
                sqlx::query(
                "INSERT INTO garage (xuid, \"type\", \"serial\", faction) VALUES ($1, $2, $3, $4)",
            )
            .bind(client.xuid)
            .bind(serial_stocks.id)
            .bind(serial)
            .bind(client.faction)
            .execute(&mut *transaction)
            .await?;

                vt_list.push(VTEntry {
                    id: serial_stocks.id as u16,
                    serial: serial as u32,
                });
            }
        }
    }

    transaction.commit().await?;

    Ok(TxCommand::AwardClaim { status: 0, vt_list })
}

// Trading
//
// Fork: the game trades in a chat room of the Neutral Zone. Both players agree
// over the peer-to-peer link; then one of them asks for a token (TradeToken,
// naming the partner's pilot), hands it to the partner over the same link, and
// both send TradeUserInfo and TradeVT with that token. Upstream paired the two
// commands with a barrier only, so a client could claim any VT by type and
// serial or pair a trade with itself over two connections. Here:
//
// - a token is bound to the identity that asked for it and to the named
//   partner pilot, expires, is used once per command, and is never 0 (the game
//   treats 0 as "no token");
// - exactly the issuer and the named partner meet, as two different identities
//   with a faction, both naming each other;
// - VTs change hands only if both sides report the same swap and the giving
//   side owns every VT it gives (not sortied); both sides of a command are
//   applied in one transaction by whoever arrives second.
type TradeToken = u32;

/// How long a token stays valid after TradeToken.
const TRADE_TOKEN_TTL: Duration = Duration::from_secs(180);
/// How long the first side waits for the partner's command.
const TRADE_PARTNER_WAIT: Duration = Duration::from_secs(10);
/// Upper bound of tokens kept at once (expired ones are dropped first).
const TRADE_TOKENS_MAX: usize = 4096;
/// Upper bound of VTs one side may give in one trade (the game shows 4 slots).
const TRADE_VTS_MAX: usize = 16;

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum TradeType {
    UserInfo = 0,
    VtList = 1,
}

#[derive(Clone, Debug)]
struct TradeTokenInfo {
    issuer: Uuid,
    issuer_name: String,
    partner_name: String,
    expires: tokio::time::Instant,
    used: [bool; 2],
}

/// One side of a trade command.
#[derive(Clone)]
struct TradeSide {
    xuid: Uuid,
    name: String,
    faction: Faction,
    partner_name: String,
    rx: Vec<(VTID, u32)>,
    tx: Vec<(VTID, u32)>,
    pilot: Option<PilotData>,
}

type TradeDone = tokio::sync::oneshot::Sender<Result<(), &'static str>>;
type TradeWait = tokio::sync::oneshot::Receiver<Result<(), &'static str>>;

struct TradePending {
    side: TradeSide,
    done: TradeDone,
}

static TRADE_TOKENS: LazyLock<Mutex<HashMap<TradeToken, TradeTokenInfo>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static TRADE_PENDING: LazyLock<Mutex<HashMap<(TradeToken, TradeType), TradePending>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static TRADE_TOKEN_NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// Pilot names as the game sends them: up to 16 bytes, padded with spaces.
fn trade_name(name: &str) -> String {
    name.trim_end_matches([' ', '\0']).to_ascii_lowercase()
}

/// Next token: never 0, not predictable from the previous trade alone.
fn trade_token_next() -> TradeToken {
    use std::sync::atomic::Ordering;
    let seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos() ^ (d.as_secs() as u32).rotate_left(13))
        .unwrap_or(1)
        | 1;
    let _ = TRADE_TOKEN_NEXT.compare_exchange(0, seed, Ordering::Relaxed, Ordering::Relaxed);
    loop {
        // Weyl sequence plus a bijective finalizer: distinct for 2^32 calls.
        let n = TRADE_TOKEN_NEXT.fetch_add(0x9E37_79B9, Ordering::Relaxed);
        let mut x = n ^ (n >> 16);
        x = x.wrapping_mul(0x7FEB_352D);
        x ^= x >> 15;
        x = x.wrapping_mul(0x846C_A68B);
        x ^= x >> 16;
        if x != 0 {
            return x;
        }
    }
}

/// Whether `side` may use the token: the issuer naming the partner, or the
/// partner (another identity, the named pilot) naming the issuer.
fn trade_token_admits(info: &TradeTokenInfo, side: &TradeSide) -> bool {
    if side.xuid == info.issuer {
        trade_name(&side.partner_name) == info.partner_name
    } else {
        trade_name(&side.name) == info.partner_name && trade_name(&side.partner_name) == info.issuer_name
    }
}

/// Whether two sides form one trade: two different identities with a faction
/// that name each other.
fn trade_pair_ok(a: &TradeSide, b: &TradeSide) -> Result<(), &'static str> {
    if a.xuid == b.xuid {
        return Err("same_identity");
    }
    if matches!(a.faction, Faction::None) || matches!(b.faction, Faction::None) {
        return Err("no_faction");
    }
    if trade_name(&a.partner_name) != trade_name(&b.name) || trade_name(&b.partner_name) != trade_name(&a.name) {
        return Err("partner_mismatch");
    }
    Ok(())
}

/// Both sides must report the same swap; nobody gives a VT twice.
fn trade_vt_swap_ok(a: &TradeSide, b: &TradeSide) -> Result<(), &'static str> {
    if a.tx.len() > TRADE_VTS_MAX || b.tx.len() > TRADE_VTS_MAX {
        return Err("too_many_vts");
    }
    let sorted = |v: &Vec<(VTID, u32)>| {
        let mut v = v.clone();
        v.sort_unstable();
        v
    };
    let (a_tx, b_tx) = (sorted(&a.tx), sorted(&b.tx));
    if a_tx.windows(2).any(|w| w[0] == w[1]) || b_tx.windows(2).any(|w| w[0] == w[1]) {
        return Err("duplicate_vt");
    }
    if a_tx != sorted(&b.rx) || b_tx != sorted(&a.rx) {
        return Err("swap_mismatch");
    }
    Ok(())
}

/// TradeToken: remember who asked for it and for which partner.
async fn trade_token_issue(issuer: Uuid, issuer_name: &str, partner_name: &str) -> Option<TradeToken> {
    let partner = trade_name(partner_name);
    if partner.is_empty() || partner == trade_name(issuer_name) {
        return None;
    }
    let now = tokio::time::Instant::now();
    let mut tokens = TRADE_TOKENS.lock().await;
    tokens.retain(|_, t| t.expires > now && t.issuer != issuer); // one open token per identity
    if tokens.len() >= TRADE_TOKENS_MAX {
        return None;
    }
    let token = loop {
        let t = trade_token_next();
        if !tokens.contains_key(&t) {
            break t;
        }
    };
    tokens.insert(
        token,
        TradeTokenInfo {
            issuer,
            issuer_name: trade_name(issuer_name),
            partner_name: partner,
            expires: now + TRADE_TOKEN_TTL,
            used: [false; 2],
        },
    );
    Some(token)
}

enum TradeMeet {
    /// Arrived second: the first side and its channel to report the result to.
    Second(TradeSide, TradeDone),
    /// Arrived first: wait for the result.
    First(TradeWait),
}

/// Bring the two sides of one command together (no database access).
async fn trade_meet(trade_type: TradeType, token: TradeToken, side: TradeSide) -> Result<TradeMeet, &'static str> {
    {
        let now = tokio::time::Instant::now();
        let tokens = TRADE_TOKENS.lock().await;
        let info = match tokens.get(&token) {
            Some(info) if info.expires > now => info,
            _ => return Err("unknown_token"),
        };
        if info.used[trade_type as usize] {
            return Err("token_used");
        }
        if !trade_token_admits(info, &side) {
            return Err("not_a_party");
        }
    }
    let mut pending = TRADE_PENDING.lock().await;
    match pending.remove(&(token, trade_type)) {
        Some(first) if !first.done.is_closed() => {
            if let Err(why) = trade_pair_ok(&first.side, &side) {
                let _ = first.done.send(Err(why));
                return Err(why);
            }
            if let Some(info) = TRADE_TOKENS.lock().await.get_mut(&token) {
                info.used[trade_type as usize] = true;
            }
            Ok(TradeMeet::Second(first.side, first.done))
        }
        _ => {
            // Nobody waiting (or the first side gave up already): wait here.
            let (done, wait) = tokio::sync::oneshot::channel();
            pending.insert((token, trade_type), TradePending { side, done });
            Ok(TradeMeet::First(wait))
        }
    }
}

/// Run one trade command: meet the partner, then `apply` both sides once.
async fn trade_run<F, Fut>(trade_type: TradeType, token: TradeToken, side: TradeSide, apply: F) -> Result<(), ClientError>
where
    F: FnOnce(TradeSide, TradeSide) -> Fut,
    Fut: std::future::Future<Output = Result<(), &'static str>>,
{
    let refused = |why: &str| ClientError::Runtime(format!("trade refused ({why})"));
    let me = side.xuid;
    match trade_meet(trade_type, token, side.clone()).await {
        Err(why) => Err(refused(why)),
        Ok(TradeMeet::First(wait)) => match timeout(TRADE_PARTNER_WAIT, wait).await {
            Ok(Ok(Ok(()))) => Ok(()),
            Ok(Ok(Err(why))) => Err(refused(why)),
            Ok(Err(_)) => Err(refused("partner_gone")),
            Err(_) => {
                let mut pending = TRADE_PENDING.lock().await;
                if pending.get(&(token, trade_type)).is_some_and(|p| p.side.xuid == me) {
                    pending.remove(&(token, trade_type));
                }
                Err(ClientError::Timeout)
            }
        },
        Ok(TradeMeet::Second(first, done)) => {
            let result = apply(first, side).await;
            let _ = done.send(result);
            result.map_err(refused)
        }
    }
}

/// Pilot name of an account as the game knows it (pilot data +0x28), not the gamertag (`xname`).
async fn trade_pilot_name(xuid: Uuid) -> Option<String> {
    let pilot: Option<Vec<u8>> = sqlx::query_scalar("SELECT pilot FROM accounts WHERE xuid = $1")
        .bind(xuid)
        .fetch_optional(get_pool())
        .await
        .ok()??;
    let name = pilot_data_decode_name(&pilot_data_from_slice(&pilot?).ok()?).ok()?;
    (!trade_name(&name).is_empty()).then_some(name)
}

async fn trade_side(xuid: Uuid, faction: Faction, partner_name: String) -> Result<TradeSide, ClientError> {
    let name = trade_pilot_name(xuid)
        .await
        .ok_or_else(|| ClientError::Runtime("trade refused (no_pilot)".to_owned()))?;
    Ok(TradeSide {
        xuid,
        name,
        faction,
        partner_name,
        rx: Vec::new(),
        tx: Vec::new(),
        pilot: None,
    })
}

pub async fn trade_token(client: &mut Client, pilot_name: String) -> CommandResult {
    let own = match trade_pilot_name(client.xuid).await {
        Some(name) if !matches!(client.faction, Faction::None) => name,
        _ => return Ok(TxCommand::TradeToken { status: 1, token: 0 }),
    };
    match trade_token_issue(client.xuid, &own, &pilot_name).await {
        Some(token) => Ok(TxCommand::TradeToken { status: 0, token }),
        None => Ok(TxCommand::TradeToken { status: 1, token: 0 }),
    }
}

pub async fn trade_user_info(
    client: &mut Client,
    pilot_name: String,
    token: u32,
    pilot_data: PilotData,
) -> CommandResult {
    let mut side = trade_side(client.xuid, client.faction, pilot_name).await?;
    side.pilot = Some(pilot_data);
    trade_run(TradeType::UserInfo, token, side, |a, b| async move {
        let pg = get_pool();
        let apply = async {
            let mut transaction = pg.begin().await?;
            for s in [&a, &b] {
                sqlx::query("UPDATE accounts SET pilot = $2 WHERE xuid = $1")
                    .bind(s.xuid)
                    .bind(s.pilot.expect("pilot data of a TradeUserInfo side"))
                    .execute(&mut *transaction)
                    .await?;
            }
            transaction.commit().await
        };
        apply.await.map_err(|_: sqlx::Error| "database")
    })
    .await?;

    Ok(TxCommand::TradeUserInfo {
        status: 0,
        delay: 0,
    })
}

pub async fn trade_vt(
    client: &mut Client,
    token: u32,
    pilot_name: String,
    rx_vt_list: VTEntryList,
    tx_vt_list: VTEntryList,
) -> CommandResult {
    let mut side = trade_side(client.xuid, client.faction, pilot_name).await?;
    side.rx = rx_vt_list.iter().map(|vt| (vt.id, vt.serial)).collect();
    side.tx = tx_vt_list.iter().map(|vt| (vt.id, vt.serial)).collect();
    trade_run(TradeType::VtList, token, side, |a, b| async move {
        trade_vt_swap_ok(&a, &b)?;
        let pg = get_pool();
        let mut transaction = pg.begin().await.map_err(|_| "database")?;
        for (giver, receiver) in [(&a, &b), (&b, &a)] {
            for &(id, serial) in giver.tx.iter() {
                let moved = sqlx::query(
                    "UPDATE garage SET xuid = $3, faction = $4, sortied = false \
                     WHERE xuid = $1 AND \"type\" = $2 AND \"serial\" = $5 AND NOT sortied",
                )
                .bind(giver.xuid)
                .bind(id as i16)
                .bind(receiver.xuid)
                .bind(receiver.faction)
                .bind(serial as i64)
                .execute(&mut *transaction)
                .await
                .map_err(|_| "database")?;
                if moved.rows_affected() != 1 {
                    return Err("vt_not_owned"); // dropping the transaction rolls back
                }
            }
        }
        transaction.commit().await.map_err(|_| "database")
    })
    .await?;

    Ok(TxCommand::TradeVT {
        status: 0,
        vt_list: rx_vt_list,
    })
}

#[cfg(test)]
mod trade_tests {
    use super::*;

    fn side(n: u128, name: &str, partner: &str) -> TradeSide {
        TradeSide {
            xuid: Uuid::from_u128(n),
            name: name.to_owned(),
            faction: Faction::Hsd,
            partner_name: partner.to_owned(),
            rx: Vec::new(),
            tx: Vec::new(),
            pilot: None,
        }
    }

    fn info(issuer: u128, issuer_name: &str, partner: &str) -> TradeTokenInfo {
        TradeTokenInfo {
            issuer: Uuid::from_u128(issuer),
            issuer_name: trade_name(issuer_name),
            partner_name: trade_name(partner),
            expires: tokio::time::Instant::now() + TRADE_TOKEN_TTL,
            used: [false; 2],
        }
    }

    #[test]
    fn tokens_are_never_zero_and_differ() {
        let mut seen = std::collections::HashSet::new();
        for _ in 0..10_000 {
            let t = trade_token_next();
            assert_ne!(t, 0);
            assert!(seen.insert(t), "token repeated");
        }
    }

    #[test]
    fn names_compare_like_the_game_pads_them() {
        assert_eq!(trade_name("AAA             "), "aaa");
        assert_eq!(trade_name("Bbb\0"), "bbb");
        assert_ne!(trade_name("AAA"), trade_name("AAB"));
    }

    #[test]
    fn only_issuer_and_named_partner_may_use_a_token() {
        let t = info(1, "AAA", "BBB");
        assert!(trade_token_admits(&t, &side(1, "AAA", "BBB ")));
        assert!(trade_token_admits(&t, &side(2, "BBB", "AAA")));
        assert!(!trade_token_admits(&t, &side(1, "AAA", "CCC")), "issuer naming someone else");
        assert!(!trade_token_admits(&t, &side(3, "CCC", "AAA")), "third pilot");
        assert!(!trade_token_admits(&t, &side(2, "BBB", "DDD")), "partner naming someone else");
    }

    #[test]
    fn a_trade_needs_two_identities_naming_each_other() {
        assert!(trade_pair_ok(&side(1, "AAA", "BBB"), &side(2, "BBB", "AAA")).is_ok());
        assert_eq!(trade_pair_ok(&side(1, "AAA", "BBB"), &side(1, "BBB", "AAA")), Err("same_identity"));
        assert_eq!(trade_pair_ok(&side(1, "AAA", "BBB"), &side(2, "BBB", "CCC")), Err("partner_mismatch"));
        let mut none = side(2, "BBB", "AAA");
        none.faction = Faction::None;
        assert_eq!(trade_pair_ok(&side(1, "AAA", "BBB"), &none), Err("no_faction"));
    }

    #[test]
    fn both_sides_must_report_the_same_swap() {
        let mut a = side(1, "AAA", "BBB");
        let mut b = side(2, "BBB", "AAA");
        a.tx = vec![(0, 7), (2, 9)];
        a.rx = vec![(5, 1)];
        b.tx = vec![(5, 1)];
        b.rx = vec![(2, 9), (0, 7)];
        assert!(trade_vt_swap_ok(&a, &b).is_ok());
        // A one-sided claim (the upstream hole): B says it receives a VT nobody gives.
        b.rx.push((3, 3));
        assert_eq!(trade_vt_swap_ok(&a, &b), Err("swap_mismatch"));
        b.rx.pop();
        a.tx.push((0, 7));
        b.rx.push((0, 7));
        assert_eq!(trade_vt_swap_ok(&a, &b), Err("duplicate_vt"));
        a.tx = (0..17).map(|i| (0, i)).collect();
        assert_eq!(trade_vt_swap_ok(&a, &b), Err("too_many_vts"));
    }

    #[test]
    fn rendezvous_pairs_issuer_and_partner_once() {
        tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
            let token = trade_token_issue(Uuid::from_u128(11), "AAA", "BBB").await.unwrap();
            // A stranger cannot join, the partner can; a second use is refused.
            assert!(matches!(trade_meet(TradeType::VtList, token, side(13, "CCC", "AAA")).await, Err("not_a_party")));
            let Ok(TradeMeet::First(wait)) = trade_meet(TradeType::VtList, token, side(11, "AAA", "BBB")).await else {
                panic!("issuer arrives first")
            };
            let Ok(TradeMeet::Second(other, done)) = trade_meet(TradeType::VtList, token, side(12, "BBB", "AAA")).await
            else {
                panic!("partner arrives second")
            };
            assert_eq!(other.xuid, Uuid::from_u128(11));
            done.send(Ok(())).unwrap();
            assert_eq!(wait.await.unwrap(), Ok(()));
            assert!(matches!(trade_meet(TradeType::VtList, token, side(11, "AAA", "BBB")).await, Err("token_used")));

            // The same identity on a second connection is no partner.
            let token = trade_token_issue(Uuid::from_u128(21), "DDD", "EEE").await.unwrap();
            let Ok(TradeMeet::First(wait)) = trade_meet(TradeType::UserInfo, token, side(21, "DDD", "EEE")).await
            else {
                panic!("issuer arrives first")
            };
            assert!(trade_meet(TradeType::UserInfo, token, side(21, "DDD", "EEE")).await.is_err());
            assert_eq!(wait.await.unwrap(), Err("same_identity"));

            // Token 0 ("no token" for the game) and unknown tokens are refused; no token for oneself.
            assert!(matches!(trade_meet(TradeType::UserInfo, 0, side(21, "DDD", "EEE")).await, Err("unknown_token")));
            assert!(trade_token_issue(Uuid::from_u128(31), "FFF", "fff ").await.is_none());
        });
    }
}
