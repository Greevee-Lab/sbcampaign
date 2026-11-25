// SPDX-License-Identifier: AGPL-3.0-only
//! Protection of the Steelbound gateway: rate limits per source address and
//! per identity, connection limits, and rough plausibility bounds for
//! reported results.

use crate::{
    client::Faction,
    command::rx::RxCommand,
};
use std::{
    collections::HashMap,
    net::IpAddr,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use toml::Table;

/// Tunable limits, section `[gateway_steelbound.limits]`. Every field is
/// optional in the configuration; the defaults are documented in
/// `deploy/README.md` ("Steelbound gateway").
#[derive(Debug, Clone, PartialEq)]
pub struct Limits {
    /// Open connections in total.
    pub max_connections: usize,
    /// Open connections per source (IPv4 address or IPv6 /64).
    pub max_connections_per_source: usize,
    /// New connections per source: sustained per second, burst.
    pub source_connect_rate: f64,
    pub source_connect_burst: f64,
    /// Refused tickets per source; when used up, the source is refused
    /// before TLS until the bucket refills.
    pub source_failure_rate: f64,
    pub source_failure_burst: f64,
    /// Accepted connections (transactions) per identity.
    pub identity_connect_rate: f64,
    pub identity_connect_burst: f64,
    /// Campaign commands per identity.
    pub identity_command_rate: f64,
    pub identity_command_burst: f64,
    /// Reported results (AddUserPoint, AddMissionPoint) per identity.
    pub identity_result_rate: f64,
    pub identity_result_burst: f64,
    /// Commands per connection (the game sends one per transaction).
    pub max_commands_per_connection: u32,
    /// TLS handshake, prelude, whole connection.
    pub tls_handshake_timeout: Duration,
    pub prelude_timeout: Duration,
    pub connection_lifetime: Duration,
    pub plausibility: Plausibility,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            max_connections: 256,
            max_connections_per_source: 8,
            source_connect_rate: 2.0,
            source_connect_burst: 30.0,
            source_failure_rate: 0.1,
            source_failure_burst: 10.0,
            identity_connect_rate: 2.0,
            identity_connect_burst: 30.0,
            identity_command_rate: 5.0,
            identity_command_burst: 40.0,
            identity_result_rate: 1.0 / 60.0,
            identity_result_burst: 10.0,
            max_commands_per_connection: 8,
            tls_handshake_timeout: Duration::from_secs(10),
            prelude_timeout: Duration::from_secs(5),
            connection_lifetime: Duration::from_secs(60),
            plausibility: Plausibility::default(),
        }
    }
}

/// Rough bounds for what one report may contain. A report outside them is
/// not applied (the game still gets its normal answer) and audited as
/// "implausible".
#[derive(Debug, Clone, PartialEq)]
pub struct Plausibility {
    /// Each of the ten AddUserPoint stats, per report.
    pub max_user_stat: u32,
    /// AddMissionPoint points, per report.
    pub max_mission_points: u32,
    /// Highest map number within a turn.
    pub max_map: u8,
    /// Reports may name the current turn or up to this many turns before it
    /// (a mission that ends right after the turn changed), same round only.
    pub turn_grace: u16,
    /// Entries in a VT list (VtAdd, VtDel, VtSortie, ...).
    pub max_vt_list: usize,
}

impl Default for Plausibility {
    fn default() -> Self {
        Plausibility { max_user_stat: 100_000, max_mission_points: 10_000, max_map: 3, turn_grace: 1, max_vt_list: 64 }
    }
}

impl Limits {
    pub fn from_toml(section: Option<&Table>) -> Result<Self, String> {
        let mut l = Limits::default();
        let Some(t) = section else { return Ok(l) };
        let float = |name: &str, v: &mut f64| -> Result<(), String> {
            if let Some(x) = t.get(name) {
                let f = x.as_float().or_else(|| x.as_integer().map(|i| i as f64));
                match f {
                    Some(f) if f > 0.0 && f.is_finite() => *v = f,
                    _ => return Err(format!("limits.{name} must be a positive number")),
                }
            }
            Ok(())
        };
        let int = |name: &str| -> Result<Option<u64>, String> {
            match t.get(name) {
                None => Ok(None),
                Some(x) => x
                    .as_integer()
                    .and_then(|i| u64::try_from(i).ok())
                    .filter(|&i| i > 0)
                    .map(Some)
                    .ok_or_else(|| format!("limits.{name} must be a positive integer")),
            }
        };
        float("source_connect_rate", &mut l.source_connect_rate)?;
        float("source_connect_burst", &mut l.source_connect_burst)?;
        float("source_failure_rate", &mut l.source_failure_rate)?;
        float("source_failure_burst", &mut l.source_failure_burst)?;
        float("identity_connect_rate", &mut l.identity_connect_rate)?;
        float("identity_connect_burst", &mut l.identity_connect_burst)?;
        float("identity_command_rate", &mut l.identity_command_rate)?;
        float("identity_command_burst", &mut l.identity_command_burst)?;
        if let Some(per_hour) = int("results_per_hour")? {
            l.identity_result_rate = per_hour as f64 / 3600.0;
        }
        float("identity_result_burst", &mut l.identity_result_burst)?;
        if let Some(v) = int("max_connections")? {
            l.max_connections = v as usize;
        }
        if let Some(v) = int("max_connections_per_source")? {
            l.max_connections_per_source = v as usize;
        }
        if let Some(v) = int("max_commands_per_connection")? {
            l.max_commands_per_connection = v.min(u64::from(u32::MAX)) as u32;
        }
        if let Some(v) = int("tls_handshake_timeout_secs")? {
            l.tls_handshake_timeout = Duration::from_secs(v);
        }
        if let Some(v) = int("prelude_timeout_secs")? {
            l.prelude_timeout = Duration::from_secs(v);
        }
        if let Some(v) = int("connection_lifetime_secs")? {
            l.connection_lifetime = Duration::from_secs(v);
        }
        let p = &mut l.plausibility;
        if let Some(v) = int("max_user_stat")? {
            p.max_user_stat = v.min(i32::MAX as u64) as u32;
        }
        if let Some(v) = int("max_mission_points")? {
            p.max_mission_points = v.min(i32::MAX as u64) as u32;
        }
        if let Some(v) = int("max_map")? {
            p.max_map = v.min(255) as u8;
        }
        if let Some(v) = t.get("turn_grace") {
            p.turn_grace = v
                .as_integer()
                .and_then(|i| u16::try_from(i).ok())
                .ok_or("limits.turn_grace must be a non-negative integer")?;
        }
        if let Some(v) = int("max_vt_list")? {
            p.max_vt_list = v as usize;
        }
        Ok(l)
    }
}

// ---- Sources ------------------------------------------------------------

/// The unit addresses are limited by: an IPv4 address, or the /64 network of
/// an IPv6 address (one subscriber usually gets a whole /64). Never logged
/// or stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SourceKey {
    V4(u32),
    V6Net(u64),
}

impl SourceKey {
    pub fn of(ip: IpAddr) -> Self {
        match ip {
            IpAddr::V4(v4) => SourceKey::V4(u32::from(v4)),
            IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
                Some(v4) => SourceKey::V4(u32::from(v4)),
                None => SourceKey::V6Net((u128::from(v6) >> 64) as u64),
            },
        }
    }
}

// ---- Token buckets ------------------------------------------------------

/// Token buckets per key. Full buckets that were idle for a while are
/// dropped, so the map stays small under address churn.
pub struct KeyedLimiter<K> {
    rate: f64,
    burst: f64,
    buckets: Mutex<(HashMap<K, (f64, Instant)>, Instant)>,
}

const MAX_KEYS: usize = 100_000;
const PRUNE_EVERY: Duration = Duration::from_secs(30);

impl<K: std::hash::Hash + Eq + Clone> KeyedLimiter<K> {
    pub fn new(rate: f64, burst: f64) -> Self {
        KeyedLimiter { rate, burst, buckets: Mutex::new((HashMap::new(), Instant::now())) }
    }

    fn refill(&self, entry: &mut (f64, Instant), now: Instant) {
        entry.0 = (entry.0 + now.duration_since(entry.1).as_secs_f64() * self.rate).min(self.burst);
        entry.1 = now;
    }

    fn with_bucket<R>(&self, key: &K, f: impl FnOnce(&mut f64) -> R) -> R {
        let now = Instant::now();
        let mut guard = self.buckets.lock().unwrap_or_else(|e| e.into_inner());
        let (map, last_prune) = &mut *guard;
        if now.duration_since(*last_prune) >= PRUNE_EVERY || map.len() >= MAX_KEYS {
            let (rate, burst) = (self.rate, self.burst);
            map.retain(|_, (tokens, at)| *tokens + now.duration_since(*at).as_secs_f64() * rate < burst);
            *last_prune = now;
        }
        let entry = map.entry(key.clone()).or_insert((self.burst, now));
        self.refill(entry, now);
        f(&mut entry.0)
    }

    /// Take one token. False if the bucket is empty.
    pub fn allow(&self, key: &K) -> bool {
        self.with_bucket(key, |tokens| {
            if *tokens >= 1.0 {
                *tokens -= 1.0;
                true
            } else {
                false
            }
        })
    }

    /// True if at least one token is left (takes none).
    pub fn has_tokens(&self, key: &K) -> bool {
        self.with_bucket(key, |tokens| *tokens >= 1.0)
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.buckets.lock().unwrap().0.len()
    }
}

// ---- Concurrent connections per source ----------------------------------

#[derive(Default)]
pub struct SourceSlots {
    open: Mutex<HashMap<SourceKey, usize>>,
}

/// Held while a connection is open; frees its slot on drop.
pub struct SourceSlot {
    slots: Arc<SourceSlots>,
    key: SourceKey,
}

impl SourceSlots {
    pub fn try_take(self: &Arc<Self>, key: SourceKey, max: usize) -> Option<SourceSlot> {
        let mut open = self.open.lock().unwrap_or_else(|e| e.into_inner());
        let n = open.entry(key).or_insert(0);
        if *n >= max {
            return None;
        }
        *n += 1;
        Some(SourceSlot { slots: self.clone(), key })
    }

    #[cfg(test)]
    pub fn open(&self, key: SourceKey) -> usize {
        self.open.lock().unwrap().get(&key).copied().unwrap_or(0)
    }
}

impl Drop for SourceSlot {
    fn drop(&mut self) {
        let mut open = self.slots.open.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(n) = open.get_mut(&self.key) {
            *n -= 1;
            if *n == 0 {
                open.remove(&self.key);
            }
        }
    }
}

// ---- Plausibility -------------------------------------------------------

/// Whether a command reports a result (and needs the current period).
pub fn is_result(rx: &RxCommand) -> bool {
    matches!(rx, RxCommand::AddUserPoint { .. } | RxCommand::AddMissionPoint { .. })
}

/// Check one command against the bounds. `period` is the campaign's current
/// (round, turn); needed for results only. Err carries a short reason code
/// for the log.
pub fn check(p: &Plausibility, rx: &RxCommand, period: Option<(u16, u16)>, faction: Faction) -> Result<(), &'static str> {
    let in_period = |round: u16, turn: u16| match period {
        Some((cur_round, cur_turn)) => {
            round == cur_round && turn <= cur_turn && turn.saturating_add(p.turn_grace) >= cur_turn && turn >= 1
        }
        None => false,
    };
    let list = |n: usize| if n > p.max_vt_list { Err("vt_list_too_long") } else { Ok(()) };
    match rx {
        RxCommand::AddUserPoint { round, turn, map, points } => {
            if matches!(faction, Faction::None) {
                return Err("no_faction");
            }
            if !in_period(*round, *turn) {
                return Err("wrong_period");
            }
            if *map > p.max_map {
                return Err("bad_map");
            }
            if points.iter().any(|&s| s > p.max_user_stat) {
                return Err("stat_too_high");
            }
            Ok(())
        }
        RxCommand::AddMissionPoint { round, turn, map, points, faction: for_faction } => {
            if !matches!(for_faction, Faction::Hsd | Faction::Prf | Faction::Rb) {
                return Err("bad_faction");
            }
            if !in_period(*round, *turn) {
                return Err("wrong_period");
            }
            if *map > p.max_map {
                return Err("bad_map");
            }
            if *points > p.max_mission_points {
                return Err("points_too_high");
            }
            Ok(())
        }
        RxCommand::VtAdd { vt_list } | RxCommand::AwardClaim { vt_list, .. } => list(vt_list.len()),
        RxCommand::VtDel { vt_list }
        | RxCommand::VtSortie { vt_list }
        | RxCommand::VtReturn { vt_list }
        | RxCommand::VtCapture { vt_list, .. } => list(vt_list.len()),
        RxCommand::TradeVT { rx_vt_list, tx_vt_list, .. } => {
            list(rx_vt_list.len())?;
            list(tx_vt_list.len())
        }
        _ => Ok(()),
    }
}

/// Name of a command that changes campaign data, None for reads. Every write
/// gets an audit row.
pub fn write_name(rx: &RxCommand) -> Option<&'static str> {
    Some(match rx {
        RxCommand::AddUserPoint { .. } => "AddUserPoint",
        RxCommand::AddMissionPoint { .. } => "AddMissionPoint",
        RxCommand::NewUserInfo { .. } => "NewUserInfo",
        RxCommand::SetUserInfo { .. } => "SetUserInfo",
        RxCommand::DelUserInfo => "DelUserInfo",
        RxCommand::SetUserFaction { .. } => "SetUserFaction",
        RxCommand::VtAdd { .. } => "VtAdd",
        RxCommand::VtDel { .. } => "VtDel",
        RxCommand::VtCapture { .. } => "VtCapture",
        RxCommand::VtSortie { .. } => "VtSortie",
        RxCommand::VtReturn { .. } => "VtReturn",
        RxCommand::AwardClaim { .. } => "AwardClaim",
        RxCommand::TradeUserInfo { .. } => "TradeUserInfo",
        RxCommand::TradeVT { .. } => "TradeVT",
        _ => return None,
    })
}
