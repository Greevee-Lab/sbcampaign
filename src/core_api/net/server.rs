// SPDX-License-Identifier: AGPL-3.0-only
//! The Core API listener: TLS 1.3 with mandatory client certificates of the
//! steelcore CA; one port for both bindings (gRPC is told apart by its
//! content type).

use super::{
    access::{AccessList, CoreApiConfig},
    grpc, json,
    service::{Caller, CoreApi, system_clock},
};
use crate::core_api::profile::ProfileCore;
use axum::http::{Request, header};
use hyper::body::Incoming;
use hyper_util::{
    rt::{TokioExecutor, TokioIo, TokioTimer},
    server::conn::auto,
    service::TowerToHyperService,
};
use rustls::{
    RootCertStore, ServerConfig,
    pki_types::{CertificateDer, PrivateKeyDer, pem::PemObject},
    server::WebPkiClientVerifier,
};
use std::{sync::Arc, time::Duration};
use tokio::{net::TcpListener, sync::Semaphore, task::JoinHandle};
use tokio_rustls::TlsAcceptor;
use tokio_util::sync::CancellationToken;
use toml::Table;
use tower::ServiceExt;

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// TLS configuration from PEM bytes: server chain + key, client CA.
pub fn tls_config(cert_pem: &[u8], key_pem: &[u8], client_ca_pem: &[u8]) -> Result<Arc<ServerConfig>, String> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let certs: Vec<CertificateDer<'static>> = CertificateDer::pem_slice_iter(cert_pem)
        .collect::<Result<_, _>>()
        .map_err(|e| format!("server certificate: {e}"))?;
    if certs.is_empty() {
        return Err("server certificate: no certificate in file".into());
    }
    let key = PrivateKeyDer::from_pem_slice(key_pem).map_err(|e| format!("server key: {e}"))?;
    let mut roots = RootCertStore::empty();
    for ca in CertificateDer::pem_slice_iter(client_ca_pem) {
        roots.add(ca.map_err(|e| format!("client CA: {e}"))?).map_err(|e| format!("client CA: {e}"))?;
    }
    if roots.is_empty() {
        return Err("client CA: no certificate in file".into());
    }
    let verifier = WebPkiClientVerifier::builder_with_provider(Arc::new(roots), provider.clone())
        .build()
        .map_err(|e| format!("client CA: {e}"))?;
    let mut cfg = ServerConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(|e| e.to_string())?
        .with_client_cert_verifier(verifier)
        .with_single_cert(certs, key)
        .map_err(|e| format!("server certificate/key: {e}"))?;
    cfg.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    Ok(Arc::new(cfg))
}

fn is_grpc<B>(req: &Request<B>) -> bool {
    req.headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.starts_with("application/grpc"))
}

/// Accept connections until `cancel` fires.
pub async fn serve<S: ProfileCore>(
    listener: TcpListener,
    tls: Arc<ServerConfig>,
    accesses: Arc<AccessList>,
    api: Arc<CoreApi<S>>,
    max_connections: usize,
    cancel: CancellationToken,
) {
    let acceptor = TlsAcceptor::from(tls);
    let grpc_router = grpc::router(api.clone());
    let json_router = json::router(api);
    let slots = Arc::new(Semaphore::new(max_connections));

    loop {
        let (tcp, _peer) = tokio::select! {
            _ = cancel.cancelled() => break,
            r = listener.accept() => match r {
                Ok(c) => c,
                Err(e) => {
                    let _ = systemd_log!(4, "core api: accept failed: {}", e);
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    continue;
                }
            },
        };
        let Ok(permit) = slots.clone().try_acquire_owned() else {
            drop(tcp); // over the connection limit
            continue;
        };
        let acceptor = acceptor.clone();
        let accesses = accesses.clone();
        let (grpc_router, json_router) = (grpc_router.clone(), json_router.clone());
        let cancel = cancel.clone();
        tokio::spawn(async move {
            let _permit = permit;
            let tls = match tokio::time::timeout(HANDSHAKE_TIMEOUT, acceptor.accept(tcp)).await {
                Ok(Ok(s)) => s,
                // No or foreign client certificate, wrong protocol, timeout.
                _ => return,
            };
            let access = tls
                .get_ref()
                .1
                .peer_certificates()
                .and_then(|c| c.first())
                .and_then(|der| accesses.lookup(der.as_ref()));
            if access.is_none() {
                let _ = systemd_log!(4, "core api: client certificate of the CA is not registered");
            }
            let caller = Caller(access);
            let svc = tower::service_fn(move |mut req: Request<Incoming>| {
                req.extensions_mut().insert(caller.clone());
                let router = if is_grpc(&req) { grpc_router.clone() } else { json_router.clone() };
                router.oneshot(req)
            });
            let mut builder = auto::Builder::new(TokioExecutor::new());
            builder.http1().timer(TokioTimer::new()).header_read_timeout(HANDSHAKE_TIMEOUT);
            builder.http2().max_concurrent_streams(32);
            let conn = builder.serve_connection(TokioIo::new(tls), TowerToHyperService::new(svc));
            tokio::select! {
                _ = conn => {}
                _ = cancel.cancelled() => {}
            }
        });
    }
}

/// Start the listener if `[core_api]` is configured. Called from `main`.
pub async fn start<S: ProfileCore>(
    root: &Table,
    store: Arc<S>,
    cancel: CancellationToken,
) -> Result<Option<JoinHandle<()>>, Box<dyn std::error::Error>> {
    let Some(cfg) = CoreApiConfig::from_root(root)? else {
        return Ok(None);
    };
    let read = |p: &std::path::Path| std::fs::read(p).map_err(|e| format!("core_api: {}: {e}", p.display()));
    let tls = tls_config(&read(&cfg.cert)?, &read(&cfg.key)?, &read(&cfg.client_ca)?)?;
    store.sync_accesses(&cfg.accesses.records()).await?;
    let listener = TcpListener::bind(cfg.bind).await?;
    let _ = systemd_log!(
        6,
        "core api: steelcore.v1 on {} (mTLS, {} access(es), gRPC + Connect-JSON)",
        cfg.bind,
        cfg.accesses.len()
    );
    let api = Arc::new(CoreApi::new(store, cfg.name_policy, system_clock()));
    Ok(Some(tokio::spawn(serve(
        listener,
        tls,
        Arc::new(cfg.accesses),
        api,
        cfg.max_connections,
        cancel,
    ))))
}
