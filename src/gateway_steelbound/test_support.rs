// SPDX-License-Identifier: AGPL-3.0-only
//! Test helpers: a ticket issuer with a fixed TEST key, configurations, and a
//! minimal original-protocol client. Test data only.

use crate::ticket::TicketConfig;
use ed25519_dalek::{Signer, SigningKey};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Fixed TEST seed (never used outside tests).
pub const TEST_SEED: [u8; 32] = [0x42; 32];
pub const ISSUER: &str = "steelnet-test";
pub const CAMPAIGN: &str = "steelbound-test";
/// Start of the synthetic clock used by the tests.
pub const NOW: u64 = 1_800_000_000;

pub fn signing_key(seed: [u8; 32]) -> SigningKey {
    SigningKey::from_bytes(&seed)
}

pub fn public_hex(seed: [u8; 32]) -> String {
    signing_key(seed).verifying_key().to_bytes().iter().map(|b| format!("{b:02x}")).collect()
}

pub fn ticket_config() -> TicketConfig {
    let mut root = toml::Table::new();
    let mut t = toml::Table::new();
    t.insert("issuer".into(), ISSUER.into());
    t.insert("campaign".into(), CAMPAIGN.into());
    t.insert("public_keys".into(), toml::Value::Array(vec![public_hex(TEST_SEED).into()]));
    root.insert("ticket".into(), t.into());
    TicketConfig::from_toml(&root).unwrap().unwrap()
}

/// Fields of a ticket to sign.
#[derive(Clone)]
pub struct TicketSpec {
    pub seed: [u8; 32],
    pub issued_at: u64,
    pub expires_at: u64,
    pub account: u32,
    pub issuer: String,
    pub campaign: String,
    pub display_name: String,
}

impl TicketSpec {
    pub fn valid(account: u32) -> Self {
        TicketSpec {
            seed: TEST_SEED,
            issued_at: NOW - 10,
            expires_at: NOW + 1800,
            account,
            issuer: ISSUER.into(),
            campaign: CAMPAIGN.into(),
            display_name: "Test Pilot".into(),
        }
    }

    /// Ticket format v1 (see `crate::ticket`), signed.
    pub fn sign(&self) -> Vec<u8> {
        let key = signing_key(self.seed);
        let mut t = Vec::new();
        t.extend_from_slice(b"SBCT");
        t.push(1);
        t.extend_from_slice(&key.verifying_key().to_bytes()[..8]);
        t.extend_from_slice(&self.issued_at.to_be_bytes());
        t.extend_from_slice(&self.expires_at.to_be_bytes());
        t.extend_from_slice(&self.account.to_be_bytes());
        t.extend_from_slice(&((0x5342_4C01u64 << 32) | u64::from(self.account)).to_be_bytes());
        t.extend_from_slice(&[0xA5; 16]);
        for s in [self.issuer.as_str(), "steelbound", self.campaign.as_str(), self.display_name.as_str()] {
            t.push(s.len() as u8);
            t.extend_from_slice(s.as_bytes());
        }
        let sig = key.sign(&t);
        t.extend_from_slice(&sig.to_bytes());
        t
    }
}

// ---- Original protocol, client side --------------------------------------

pub async fn send<W: AsyncWrite + Unpin>(w: &mut W, cmd: u16, payload: &[u8]) {
    let mut f = Vec::with_capacity(4 + payload.len());
    f.extend_from_slice(&(payload.len() as u16).to_le_bytes());
    f.extend_from_slice(&cmd.to_le_bytes());
    f.extend_from_slice(payload);
    w.write_all(&f).await.unwrap();
}

/// Next frame (command, payload), None if the server closed the connection.
pub async fn recv<R: AsyncRead + Unpin>(r: &mut R) -> Option<(u16, Vec<u8>)> {
    let mut h = [0u8; 4];
    r.read_exact(&mut h).await.ok()?;
    let len = u16::from_le_bytes([h[0], h[1]]) as usize;
    let mut p = vec![0u8; len];
    r.read_exact(&mut p).await.ok()?;
    Some((u16::from_le_bytes([h[2], h[3]]), p))
}

/// Read the 6-byte prelude reply and return its status byte.
pub async fn prelude_status<R: AsyncRead + Unpin>(r: &mut R) -> Option<u8> {
    let mut b = [0u8; 6];
    r.read_exact(&mut b).await.ok()?;
    assert_eq!(&b[..5], b"SBGW\x01");
    Some(b[5])
}

/// Handshake as the game: Pong, then the given (ignored) qwUserID. Returns
/// the server's verdict command (0x3013 Accept, 0x3014 Reject, ...).
pub async fn handshake<S: AsyncRead + AsyncWrite + Unpin>(s: &mut S, reported_xuid: u64) -> u16 {
    assert_eq!(recv(s).await.unwrap().0, 0x1010);
    send(s, 0x2010, &[]).await;
    assert_eq!(recv(s).await.unwrap().0, 0x1020);
    send(s, 0x2020, &reported_xuid.to_le_bytes()).await;
    recv(s).await.map(|f| f.0).unwrap_or(0)
}

/// Pilot blob of the original size with a name at the name offset.
pub fn pilot(name: &str) -> Vec<u8> {
    let mut d = vec![0u8; 1816];
    d[40..40 + name.len()].copy_from_slice(name.as_bytes());
    d
}

fn name16(s: &str) -> [u8; 16] {
    let mut n = [0u8; 16];
    n[..s.len()].copy_from_slice(s.as_bytes());
    n
}

/// 0x5022 NewUserInfo payload.
pub fn new_user_info(pilot_name: &str, xlive_name: &str, faction: u8) -> Vec<u8> {
    let mut p = Vec::new();
    p.extend_from_slice(&name16(pilot_name));
    p.extend_from_slice(&name16(xlive_name));
    p.push(faction);
    p.extend_from_slice(&1816u16.to_le_bytes());
    p.extend_from_slice(&pilot(pilot_name));
    p
}

/// 0x5024 SetUserInfo payload.
pub fn set_user_info(pilot_name: &str) -> Vec<u8> {
    let mut p = Vec::new();
    p.extend_from_slice(&1816u16.to_le_bytes());
    p.extend_from_slice(&pilot(pilot_name));
    p
}

/// 0x5026 AddUserPoint payload.
pub fn add_user_point(round: u16, turn: u16, map: u8, stats: [u32; 10]) -> Vec<u8> {
    let mut p = Vec::new();
    p.extend_from_slice(&round.to_le_bytes());
    p.extend_from_slice(&turn.to_le_bytes());
    p.push(map);
    for s in stats {
        p.extend_from_slice(&s.to_le_bytes());
    }
    p
}

/// 0x5027 AddMissionPoint payload.
pub fn add_mission_point(round: u16, turn: u16, map: u8, points: u32, faction: u8) -> Vec<u8> {
    let mut p = Vec::new();
    p.extend_from_slice(&round.to_le_bytes());
    p.extend_from_slice(&turn.to_le_bytes());
    p.push(map);
    p.extend_from_slice(&points.to_le_bytes());
    p.push(faction);
    p
}

/// W531: 0x1047 VtSortie / 0x3047 VtReturn payload: u16 count, then
/// count x (u16 type, u32 serial).
pub fn vt_entries(vts: &[(u16, u32)]) -> Vec<u8> {
    let mut p = Vec::new();
    p.extend_from_slice(&(vts.len() as u16).to_le_bytes());
    for &(t, s) in vts {
        p.extend_from_slice(&t.to_le_bytes());
        p.extend_from_slice(&s.to_le_bytes());
    }
    p
}
