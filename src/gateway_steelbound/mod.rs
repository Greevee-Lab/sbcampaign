// SPDX-License-Identifier: AGPL-3.0-only
//! The Steelbound gateway: public TLS access to the campaign for Steelbound
//! players.
//!
//! The upstream listener trusts the XUID the game reports in `0x2020`. That
//! is fine for a trusted network service in front of it, but not for a
//! public port. The Steelbound gateway is a second listener that
//!
//! 1. accepts **TLS** only (the server key is pinned by the client),
//! 2. reads a short [`prelude`] with the sign-in **ticket** the identity
//!    provider (steelnet) issued, and checks it ([`crate::ticket`]),
//! 3. resolves the identity `(steelbound, <account id>)` through the core
//!    boundary and then speaks the **unchanged original protocol**,
//! 4. ignores the `qwUserID` the game reports in `0x2020`: the session
//!    always belongs to the identity from the ticket.
//!
//! Protection ([`limits`]): rate limits per source address and per identity,
//! connection and time limits, rough plausibility bounds for reported
//! results, an audit row per write command and a ledger of reported results
//! so they can be annulled per identity (`deploy/server/annul.sh`).
//!
//! Like every gateway it reaches the campaign only through the core boundary
//! ([`CampaignCore`], [`GatewayCore`]); it contains no database code.

pub mod limits;
pub mod prelude;
mod session;
pub mod tls;

#[cfg(test)]
pub(crate) mod test_support;
#[cfg(test)]
mod tests;

use crate::{
    core_api::{CampaignCore, gateway::GatewayCore, transport::BoxedTransport},
    ticket::{self, Ticket, TicketConfig},
};
use limits::{KeyedLimiter, Limits, SourceKey, SourceSlot, SourceSlots};
use prelude::Status;
use std::{
    net::{IpAddr, SocketAddr},
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicU32, AtomicU64, Ordering},
    },
};
use tokio::{
    io::AsyncWriteExt,
    net::TcpListener,
    sync::{OwnedSemaphorePermit, Semaphore},
    task::JoinHandle,
};
use tokio_rustls::TlsAcceptor;
use tokio_util::{sync::CancellationToken, task::TaskTracker};
use toml::Table;

/// Name of this gateway in audit rows and logs.
pub const SOURCE: &str = "gateway-steelbound";

/// Section `[gateway_steelbound]` of the server configuration.
#[derive(Debug, Clone)]
pub struct GatewayConfig {
    pub bind: SocketAddr,
    pub cert: PathBuf,
    pub key: PathBuf,
    pub tls13_only: bool,
    pub limits: Limits,
}

impl GatewayConfig {
    /// `Ok(None)` if the section is missing or `enabled = false`.
    ///
    /// Refuses to start the gateway while the upstream listener
    /// (`[network] bind_ip`) is not bound to a loopback address: the upstream
    /// listener believes any reported XUID, so on an instance with public
    /// Steelbound access it must never be reachable from outside.
    pub fn from_root(root: &Table) -> Result<Option<Self>, String> {
        let Some(section) = root.get("gateway_steelbound") else {
            return Ok(None);
        };
        let t = section.as_table().ok_or("gateway_steelbound: not a table")?;
        if t.get("enabled").and_then(|v| v.as_bool()) == Some(false) {
            return Ok(None);
        }
        let text = |name: &str| {
            t.get(name)
                .and_then(|v| v.as_str())
                .ok_or_else(|| format!("gateway_steelbound.{name} missing"))
        };
        let bind: SocketAddr = text("bind")?
            .parse()
            .map_err(|e| format!("gateway_steelbound.bind: {e}"))?;
        let tls13_only = match t.get("tls_min").and_then(|v| v.as_str()).unwrap_or("1.2") {
            "1.2" => false,
            "1.3" => true,
            other => return Err(format!("gateway_steelbound.tls_min: {other:?} (use \"1.2\" or \"1.3\")")),
        };
        let limits = Limits::from_toml(t.get("limits").and_then(|v| v.as_table()))
            .map_err(|e| format!("gateway_steelbound.{e}"))?;

        let upstream_ip = root
            .get("network")
            .and_then(|n| n.get("bind_ip"))
            .and_then(|v| v.as_str())
            .and_then(|s| s.parse::<IpAddr>().ok());
        if !upstream_ip.is_some_and(|ip| ip.is_loopback()) {
            return Err("gateway_steelbound: network.bind_ip must be a loopback address (127.0.0.1 or ::1) \
                        while the Steelbound gateway is enabled; the upstream listener trusts reported XUIDs"
                .into());
        }
        Ok(Some(GatewayConfig { bind, cert: text("cert")?.into(), key: text("key")?.into(), tls13_only, limits }))
    }
}

/// Counters for logs and tests.
#[derive(Default, Debug)]
pub struct Counters {
    pub accepted: AtomicU64,
    pub refused: AtomicU64,
    pub busy: AtomicU64,
    pub dropped: AtomicU64,
}

/// Unix time in seconds.
pub type Clock = Arc<dyn Fn() -> u64 + Send + Sync>;

pub fn system_clock() -> Clock {
    Arc::new(|| {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    })
}

/// Shared state of one gateway listener.
pub struct Gateway<C> {
    pub(crate) core: Arc<C>,
    pub(crate) tickets: TicketConfig,
    pub(crate) limits: Limits,
    pub(crate) clock: Clock,
    source_connects: KeyedLimiter<SourceKey>,
    source_failures: KeyedLimiter<SourceKey>,
    identity_connects: KeyedLimiter<u32>,
    pub(crate) identity_commands: KeyedLimiter<u32>,
    pub(crate) identity_results: KeyedLimiter<u32>,
    source_slots: Arc<SourceSlots>,
    connections: Arc<Semaphore>,
    session_ids: AtomicU32,
    pub counters: Counters,
}

/// A connection that passed the checks before TLS. Frees its slots on drop.
pub struct Admission {
    _permit: OwnedSemaphorePermit,
    _slot: SourceSlot,
    source: SourceKey,
}

impl<C> Gateway<C> {
    pub fn new(core: Arc<C>, tickets: TicketConfig, limits: Limits, clock: Clock) -> Self {
        let l = &limits;
        Gateway {
            source_connects: KeyedLimiter::new(l.source_connect_rate, l.source_connect_burst),
            source_failures: KeyedLimiter::new(l.source_failure_rate, l.source_failure_burst),
            identity_connects: KeyedLimiter::new(l.identity_connect_rate, l.identity_connect_burst),
            identity_commands: KeyedLimiter::new(l.identity_command_rate, l.identity_command_burst),
            identity_results: KeyedLimiter::new(l.identity_result_rate, l.identity_result_burst),
            source_slots: Arc::new(SourceSlots::default()),
            connections: Arc::new(Semaphore::new(l.max_connections)),
            session_ids: AtomicU32::new(0),
            counters: Counters::default(),
            core,
            tickets,
            limits,
            clock,
        }
    }

    /// Checks before TLS (cheap, nothing sent): total connections, refused
    /// tickets of the source, open connections and new connections per
    /// source. None = drop the connection.
    pub fn admit(&self, peer: IpAddr) -> Option<Admission> {
        let source = SourceKey::of(peer);
        let refuse = || {
            self.counters.dropped.fetch_add(1, Ordering::Relaxed);
            None
        };
        let Ok(permit) = self.connections.clone().try_acquire_owned() else { return refuse() };
        if !self.source_failures.has_tokens(&source) {
            return refuse();
        }
        let Some(slot) = self.source_slots.try_take(source, self.limits.max_connections_per_source) else {
            return refuse();
        };
        if !self.source_connects.allow(&source) {
            return refuse();
        }
        Some(Admission { _permit: permit, _slot: slot, source })
    }

    /// Check a ticket. Every failure is the same [`Status::Refused`] to the
    /// client; the reason only goes to the log.
    pub fn check_ticket(&self, bytes: &[u8]) -> Result<Ticket, ticket::TicketError> {
        ticket::verify_at(bytes, &self.tickets, (self.clock)())
    }

    fn refused(&self, source: SourceKey, session_id: u32, reason: &str) {
        self.counters.refused.fetch_add(1, Ordering::Relaxed);
        let _ = self.source_failures.allow(&source);
        let _ = systemd_log!(5, "gateway {:08x}: ticket refused ({})", session_id, reason);
    }
}

impl<C: CampaignCore + GatewayCore> Gateway<C> {
    /// Serve one connection after TLS: prelude, ticket, limits, then the
    /// original protocol for the ticket's identity.
    pub async fn serve(&self, mut stream: BoxedTransport, admission: Admission) {
        let session_id = self.session_ids.fetch_add(1, Ordering::Relaxed);
        let source = admission.source;
        let ticket = match tokio::time::timeout(self.limits.prelude_timeout, prelude::read(&mut stream)).await {
            Err(_) | Ok(Err(prelude::PreludeError::Io)) => return,
            Ok(Err(prelude::PreludeError::Unsupported)) => {
                self.finish(&mut stream, Status::Unsupported).await;
                return;
            }
            Ok(Err(prelude::PreludeError::NoTicket)) => {
                self.refused(source, session_id, "no_ticket");
                self.finish(&mut stream, Status::Refused).await;
                return;
            }
            Ok(Ok(bytes)) => match self.check_ticket(&bytes) {
                Ok(t) => t,
                Err(e) => {
                    self.refused(source, session_id, e.code());
                    self.finish(&mut stream, Status::Refused).await;
                    return;
                }
            },
        };
        if !self.identity_connects.allow(&ticket.account) {
            self.counters.busy.fetch_add(1, Ordering::Relaxed);
            let _ = systemd_log!(5, "gateway {:08x} account {}: busy (identity connection rate)", session_id, ticket.account);
            self.finish(&mut stream, Status::Busy).await;
            return;
        }
        if stream.write_all(&prelude::reply(Status::Ok)).await.is_err() {
            return;
        }
        self.counters.accepted.fetch_add(1, Ordering::Relaxed);
        let lifetime = self.limits.connection_lifetime;
        if tokio::time::timeout(lifetime, session::run(self, stream, session_id, &ticket)).await.is_err() {
            let _ = systemd_log!(6, "gateway {:08x} account {}: connection lifetime exceeded", session_id, ticket.account);
        }
        drop(admission);
    }

    async fn finish(&self, stream: &mut BoxedTransport, status: Status) {
        let _ = stream.write_all(&prelude::reply(status)).await;
        let _ = stream.shutdown().await;
    }
}

/// Accept TLS connections until `cancel` fires.
pub async fn serve<C: CampaignCore + GatewayCore>(
    listener: TcpListener,
    tls: Arc<rustls::ServerConfig>,
    gateway: Arc<Gateway<C>>,
    cancel: CancellationToken,
) {
    let acceptor = TlsAcceptor::from(tls);
    let tracker = TaskTracker::new();
    loop {
        let (tcp, peer) = tokio::select! {
            biased;
            _ = cancel.cancelled() => break,
            r = listener.accept() => match r {
                Ok(c) => c,
                Err(e) => {
                    let _ = systemd_log!(4, "gateway: accept failed: {}", e);
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    continue;
                }
            },
        };
        let Some(admission) = gateway.admit(peer.ip()) else {
            drop(tcp);
            continue;
        };
        let _ = tcp.set_nodelay(true);
        let (acceptor, gateway, cancel) = (acceptor.clone(), gateway.clone(), cancel.clone());
        tracker.spawn(async move {
            let handshake = gateway.limits.tls_handshake_timeout;
            let Ok(Ok(tls)) = tokio::time::timeout(handshake, acceptor.accept(tcp)).await else {
                return;
            };
            tokio::select! {
                _ = gateway.serve(Box::new(tls), admission) => {}
                _ = cancel.cancelled() => {}
            }
        });
    }
    tracker.close();
    tracker.wait().await;
    let _ = systemd_log!(7, "gateway: stopped");
}

/// Start the gateway if `[gateway_steelbound]` is configured. Called from `main`.
pub async fn start<C: CampaignCore + GatewayCore>(
    root: &Table,
    core: Arc<C>,
    cancel: CancellationToken,
) -> Result<Option<JoinHandle<()>>, Box<dyn std::error::Error>> {
    let Some(cfg) = GatewayConfig::from_root(root)? else {
        return Ok(None);
    };
    let tickets = TicketConfig::from_toml(root)?
        .ok_or("gateway_steelbound: the [ticket] section is missing")?;
    if tickets.key_ids().is_empty() {
        return Err("gateway_steelbound: [ticket] public_keys is empty".into());
    }
    let read = |p: &std::path::Path| std::fs::read(p).map_err(|e| format!("gateway_steelbound: {}: {e}", p.display()));
    let (tls, pin) = tls::server_config(&read(&cfg.cert)?, &read(&cfg.key)?, cfg.tls13_only)?;
    let listener = TcpListener::bind(cfg.bind).await?;
    let _ = systemd_log!(
        6,
        "gateway: Steelbound gateway on {} (TLS {}, campaign {}, issuer {}, key(s) {}, SPKI pin {})",
        cfg.bind,
        if cfg.tls13_only { "1.3" } else { "1.2+" },
        tickets.campaign,
        tickets.issuer,
        tickets.key_ids().join(","),
        pin
    );
    let gateway = Arc::new(Gateway::new(core, tickets, cfg.limits, system_clock()));
    Ok(Some(tokio::spawn(serve(listener, tls, gateway, cancel))))
}
