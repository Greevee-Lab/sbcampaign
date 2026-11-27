// SPDX-License-Identifier: AGPL-3.0-only
//! One gateway session: the original handshake and command loop, bound to
//! the identity from the ticket.

use super::{Gateway, SOURCE, limits};
use crate::{
    client::{Client, ClientError, Faction},
    command::{self, rx::RxCommand, tx::TxCommand},
    core_api::{
        CampaignCore, ResolvedIdentity, VerifiedBy, dispatch::dispatch,
        gateway::{GameWrite, GatewayCore, ResultKind, ResultRecord, SortieRecord},
        transport::BoxedTransport,
    },
    ticket::Ticket,
};
use tokio_util::codec::Framed;
use uuid::Uuid;

/// The name stored as the player's Xbox Live name (`accounts.xname`) when a
/// pilot is registered: the display name from the ticket, never the one the
/// game reports. Reduced to what the original 16-byte ASCII field can hold.
pub(crate) fn campaign_xname(display_name: &str) -> String {
    let mut name: String = display_name
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == ' ')
        .take(15)
        .collect();
    name = name.trim().to_owned();
    if name.is_empty() { "Pilot".to_owned() } else { name }
}

pub(super) async fn run<C: CampaignCore + GatewayCore>(
    gw: &Gateway<C>,
    stream: BoxedTransport,
    session_id: u32,
    ticket: &Ticket,
) {
    let mut client = Client {
        codec: Framed::new(stream, command::CampaignCodec),
        session_id,
        xuid: Uuid::nil(),
        xname: String::new(),
        faction: Faction::None,
        language: None,
        banned: false,
    };
    let account = ticket.account;
    let result = session(gw, &mut client, ticket).await.map_err(closed_is_disconnect);
    let reply = match result {
        Ok(()) | Err(ClientError::Disconnected) => None,
        Err(ClientError::Timeout) => Some(TxCommand::Retry),
        Err(err) => {
            let _ = systemd_log!(5, "gateway {:08x} account {}: {}", session_id, account, describe(&err));
            Some(TxCommand::Reject)
        }
    };
    if let Some(cmd) = reply {
        let _ = client.send_command(cmd).await;
    }
    let _ = futures::SinkExt::close(&mut client.codec).await;
}

/// A peer that closes TLS without a close_notify alert (common for clients
/// that just drop the connection after the answer) is a normal disconnect.
fn closed_is_disconnect(err: ClientError) -> ClientError {
    use std::io::ErrorKind::*;
    match err {
        ClientError::CodecError(command::CampaignCodecError::Io(e))
            if matches!(e.kind(), UnexpectedEof | ConnectionReset | ConnectionAborted | BrokenPipe) =>
        {
            ClientError::Disconnected
        }
        other => other,
    }
}

fn describe(err: &ClientError) -> String {
    match err {
        ClientError::CodecError(e) => format!("codec error: {e}"),
        ClientError::SQLError(_) => "storage error".into(),
        other => other.to_string(),
    }
}

async fn session<C: CampaignCore + GatewayCore>(
    gw: &Gateway<C>,
    client: &mut Client,
    ticket: &Ticket,
) -> Result<(), ClientError> {
    let core = &*gw.core;

    // Original handshake: Ping/Pong, XUID request/answer.
    client.send_command(TxCommand::Ping).await?;
    if !matches!(client.recv_command().await?, RxCommand::Pong) {
        return Err(ClientError::BadResponse);
    }
    client.send_command(TxCommand::XUID).await?;
    match client.recv_command().await? {
        // The reported qwUserID is ignored: the session belongs to the
        // identity from the ticket. A mismatch is normal for clients that
        // report a placeholder and is not an error.
        RxCommand::XUID(_reported) => {}
        _ => return Err(ClientError::BadResponse),
    }

    let (provider, external_id) = ticket.identity();
    let identity = core
        .resolve_identity(provider, &external_id, VerifiedBy::Ticket, SOURCE)
        .await
        .map_err(ClientError::from)?;
    if !core.open_session(client, &identity).await? {
        return Err(ClientError::Runtime("banned or suspended".into()));
    }
    client.send_command(TxCommand::Accept).await?;

    let mut commands = 0u32;
    loop {
        let rx = client.recv_command().await?;
        commands += 1;
        if commands > gw.limits.max_commands_per_connection {
            return Err(ClientError::Runtime("too many commands on one connection".into()));
        }
        let write = limits::write_name(&rx);
        if !gw.identity_commands.allow(&ticket.account) {
            audit(gw, &identity, write, "rate_limited").await;
            let _ = systemd_log!(5, "gateway {:08x} account {}: command rate limit", client.session_id, ticket.account);
            client.send_command(TxCommand::Retry).await?;
            return Ok(());
        }
        let tx = command(gw, client, ticket, &identity, rx, write).await?;
        client.send_command(tx).await?;
    }
}

/// Apply one command: plausibility, the core call, audit and ledger.
async fn command<C: CampaignCore + GatewayCore>(
    gw: &Gateway<C>,
    client: &mut Client,
    ticket: &Ticket,
    identity: &ResolvedIdentity,
    rx: RxCommand,
    write: Option<&'static str>,
) -> Result<TxCommand, ClientError> {
    let core = &*gw.core;
    let is_result = limits::is_result(&rx);

    let period = if is_result { current_period(core, client).await? } else { None };
    if let Err(reason) = limits::check(&gw.limits.plausibility, &rx, period, client.faction) {
        let _ = systemd_log!(5, "gateway {:08x} account {}: implausible {} ({})",
            client.session_id, ticket.account, write.unwrap_or("command"), reason);
        audit(gw, identity, write, "implausible").await;
        return match not_applied_answer(&rx) {
            Some(tx) => Ok(tx),
            None => Err(ClientError::BadResponse),
        };
    }
    if is_result && !gw.identity_results.allow(&ticket.account) {
        let _ = systemd_log!(5, "gateway {:08x} account {}: result rate limit", client.session_id, ticket.account);
        audit(gw, identity, write, "rate_limited").await;
        return Ok(not_applied_answer(&rx).expect("results have an answer"));
    }

    // What the ledger needs, taken before the command is moved.
    let record = match &rx {
        RxCommand::AddUserPoint { round, turn, map, points } => Some(ResultRecord {
            identity_id: identity.identity_id,
            player_id: identity.player_id,
            kind: ResultKind::UserPoints,
            round: *round,
            turn: *turn,
            map: *map,
            faction: client.faction,
            points: points.to_vec(),
        }),
        RxCommand::AddMissionPoint { round, turn, map, points, faction } => Some(ResultRecord {
            identity_id: identity.identity_id,
            player_id: identity.player_id,
            kind: ResultKind::MissionPoints,
            round: *round,
            turn: *turn,
            map: *map,
            faction: *faction,
            points: vec![*points],
        }),
        _ => None,
    };
    // W531: the VTs of a sortie, for the battle statistics.
    let sortie = match &rx {
        RxCommand::VtSortie { vt_list } if !vt_list.is_empty() => Some(SortieRecord {
            identity_id: identity.identity_id,
            player_id: identity.player_id,
            faction: client.faction,
            vts: vt_list.iter().map(|v| (v.id(), v.serial())).collect(),
        }),
        _ => None,
    };

    // The registered Xbox Live name is the display name from the ticket.
    let rx = match rx {
        RxCommand::NewUserInfo { pilot_name, xlive_name: _, faction, pilot_data } => RxCommand::NewUserInfo {
            pilot_name,
            xlive_name: campaign_xname(&ticket.display_name),
            faction,
            pilot_data,
        },
        other => other,
    };

    let result = dispatch(core, client, rx).await;
    match &result {
        Ok(_) => {
            audit(gw, identity, write, "ok").await;
            if let Some(record) = record {
                if let Err(e) = core.record_result(record).await {
                    let _ = systemd_log!(3, "gateway {:08x}: result ledger: {}", client.session_id, e);
                }
            }
            if let Some(sortie) = sortie {
                if let Err(e) = core.record_sortie(sortie).await {
                    let _ = systemd_log!(3, "gateway {:08x}: sortie log: {}", client.session_id, e);
                }
            }
        }
        Err(_) => audit(gw, identity, write, "error").await,
    }
    result
}

/// The normal answer for a result that was not applied: the game cannot tell
/// the difference (and ignores the status of these answers anyway).
fn not_applied_answer(rx: &RxCommand) -> Option<TxCommand> {
    match rx {
        RxCommand::AddUserPoint { .. } => Some(TxCommand::AddUserPoint { status: 0 }),
        RxCommand::AddMissionPoint { .. } => Some(TxCommand::AddMissionPoint { status: 0 }),
        _ => None,
    }
}

async fn current_period<C: CampaignCore>(core: &C, client: &mut Client) -> Result<Option<(u16, u16)>, ClientError> {
    Ok(match core.campaign_period(client).await? {
        TxCommand::CampaignPeriod { round, turn, .. } => Some((round, turn)),
        _ => None,
    })
}

async fn audit<C: GatewayCore>(gw: &Gateway<C>, identity: &ResolvedIdentity, write: Option<&'static str>, outcome: &'static str) {
    let Some(command) = write else { return };
    let entry = GameWrite {
        source: SOURCE,
        provider: identity.provider,
        identity_id: identity.identity_id,
        player_id: identity.player_id,
        command,
        outcome,
    };
    if let Err(e) = gw.core.audit_game_write(entry).await {
        let _ = systemd_log!(3, "gateway: audit row not written ({}): {}", command, e);
    }
}
