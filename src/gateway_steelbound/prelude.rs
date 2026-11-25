// SPDX-License-Identifier: AGPL-3.0-only
//! The gateway prelude: what a Steelbound client sends after the TLS
//! handshake and before the original protocol.
//!
//! # Client -> server (version 1), big-endian
//!
//! | offset | size | field |
//! |---|---|---|
//! | 0 | 4 | magic `SBGW` |
//! | 4 | 1 | version = 1 |
//! | 5 | 1 | flags = 0 (reserved, must be 0) |
//! | 6 | 2 | ticket length `n` (0 = no ticket; at most [`ticket::MAX_TICKET_LEN`]) |
//! | 8 | n | sign-in ticket, format v1 of [`crate::ticket`] |
//!
//! # Server -> client, always 6 bytes
//!
//! | offset | size | field |
//! |---|---|---|
//! | 0 | 4 | magic `SBGW` |
//! | 4 | 1 | version the server speaks (1) |
//! | 5 | 1 | [`Status`] |
//!
//! After [`Status::Ok`] the server continues with the unchanged original
//! protocol (`0x1010` Ping first). After any other status it closes the
//! connection. The status never says *why* a ticket was refused (missing,
//! malformed, expired, forged, wrong campaign all give [`Status::Refused`]),
//! so the gateway is no oracle; a client that is refused fetches a new ticket
//! once and then gives up.

use crate::ticket;
use tokio::io::{AsyncRead, AsyncReadExt};

pub const MAGIC: &[u8; 4] = b"SBGW";
pub const VERSION: u8 = 1;
pub const HEADER_LEN: usize = 8;
pub const REPLY_LEN: usize = 6;

/// Answer to the prelude.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Status {
    /// Ticket accepted; the original protocol follows.
    Ok = 0,
    /// No valid ticket for this campaign.
    Refused = 1,
    /// Too many connections or requests; try again later.
    Busy = 2,
    /// Unknown magic, version or flags.
    Unsupported = 3,
}

/// The 6-byte reply for a status.
pub fn reply(status: Status) -> [u8; REPLY_LEN] {
    let mut r = [0u8; REPLY_LEN];
    r[..4].copy_from_slice(MAGIC);
    r[4] = VERSION;
    r[5] = status as u8;
    r
}

/// Build a prelude (used by tests and documented for clients).
pub fn encode(ticket: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(HEADER_LEN + ticket.len());
    out.extend_from_slice(MAGIC);
    out.push(VERSION);
    out.push(0);
    out.extend_from_slice(&(ticket.len() as u16).to_be_bytes());
    out.extend_from_slice(ticket);
    out
}

#[derive(Debug, PartialEq, Eq)]
pub enum PreludeError {
    /// Not a gateway prelude (wrong magic), unknown version or flags set.
    Unsupported,
    /// Ticket length 0 (no ticket) or above the limit.
    NoTicket,
    /// Connection closed or failed before the prelude was complete.
    Io,
}

/// Read the prelude and return the ticket bytes. The caller bounds the time.
pub async fn read<R: AsyncRead + Unpin>(r: &mut R) -> Result<Vec<u8>, PreludeError> {
    let mut header = [0u8; HEADER_LEN];
    r.read_exact(&mut header).await.map_err(|_| PreludeError::Io)?;
    if &header[..4] != MAGIC || header[4] != VERSION || header[5] != 0 {
        return Err(PreludeError::Unsupported);
    }
    let len = u16::from_be_bytes([header[6], header[7]]) as usize;
    if len == 0 || len > ticket::MAX_TICKET_LEN {
        return Err(PreludeError::NoTicket);
    }
    let mut ticket = vec![0u8; len];
    r.read_exact(&mut ticket).await.map_err(|_| PreludeError::Io)?;
    Ok(ticket)
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn parse(bytes: &[u8]) -> Result<Vec<u8>, PreludeError> {
        let mut r = bytes;
        read(&mut r).await
    }

    #[tokio::test]
    async fn round_trip() {
        let t = vec![7u8; 150];
        assert_eq!(parse(&encode(&t)).await.unwrap(), t);
    }

    #[tokio::test]
    async fn refuses_bad_headers() {
        let mut p = encode(&[1, 2, 3]);
        p[0] = b'X';
        assert_eq!(parse(&p).await, Err(PreludeError::Unsupported));
        let mut p = encode(&[1, 2, 3]);
        p[4] = 2;
        assert_eq!(parse(&p).await, Err(PreludeError::Unsupported));
        let mut p = encode(&[1, 2, 3]);
        p[5] = 1;
        assert_eq!(parse(&p).await, Err(PreludeError::Unsupported));
        // The original protocol's first client frame is not a prelude either.
        assert_eq!(parse(&[0, 0, 0x10, 0x20, 0, 0, 0, 0]).await, Err(PreludeError::Unsupported));
    }

    #[tokio::test]
    async fn missing_or_oversized_ticket() {
        assert_eq!(parse(&encode(&[])).await, Err(PreludeError::NoTicket));
        let mut p = encode(&[0u8; 4]);
        p[6..8].copy_from_slice(&((ticket::MAX_TICKET_LEN + 1) as u16).to_be_bytes());
        assert_eq!(parse(&p).await, Err(PreludeError::NoTicket));
    }

    #[tokio::test]
    async fn truncated_is_io() {
        let p = encode(&[9u8; 40]);
        assert_eq!(parse(&p[..20]).await, Err(PreludeError::Io));
        assert_eq!(parse(&p[..5]).await, Err(PreludeError::Io));
    }

    #[test]
    fn reply_layout() {
        assert_eq!(reply(Status::Busy), *b"SBGW\x01\x02");
    }
}
