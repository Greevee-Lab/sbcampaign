// SPDX-License-Identifier: AGPL-3.0-only
//! Core API tests over real mutual TLS, both bindings, with an in-memory
//! store (the PostgreSQL store is tested in `campaign_core::tests`).
//! Certificates are generated per run; synthetic data only.

use super::{
    access::{Access, AccessList, Scope, cert_fingerprint},
    pb,
    server::{serve, tls_config},
    service::{CoreApi, method},
};
use crate::{
    campaign_core::identity::derive_player_id,
    core_api::{
        Provider,
        names::{self, BlockedTerm, NameFacts, NamePolicy, NameStatus, PlayerNameState, Verdict},
        profile::{
            AccessRecord, AuditEntry, IdempotencyClaim, IdentityView, PlayerView, ProfileCore, SetNameOutcome,
            StoreError,
        },
    },
};
use prost::Message;
use rcgen::{
    BasicConstraints, CertificateParams, CertifiedIssuer, DnType, ExtendedKeyUsagePurpose, IsCa, KeyPair,
    KeyUsagePurpose,
};
use rustls::{
    ClientConfig, RootCertStore,
    pki_types::{CertificateDer, PrivateKeyDer, ServerName},
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::{
        Arc, Mutex,
        atomic::{AtomicI64, Ordering},
    },
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};
use tokio_rustls::TlsConnector;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

// ---- In-memory store --------------------------------------------------------

#[derive(Default)]
struct MemState {
    identities: HashMap<(Provider, String), (Uuid, Uuid, bool)>,
    /// player -> (name, changed_at)
    players: HashMap<Uuid, Option<(String, i64)>>,
    /// (player, name_key, skeleton, until)
    reservations: Vec<(Uuid, String, String, i64)>,
    blocklist: Vec<BlockedTerm>,
    idempotency: HashMap<(String, String), (String, Vec<u8>, Option<Vec<u8>>)>,
    audit: Vec<AuditEntry>,
    accesses: Vec<AccessRecord>,
    insignia_enabled: bool,
}

#[derive(Default)]
pub struct MemoryCore {
    s: Mutex<MemState>,
}

impl MemoryCore {
    fn new() -> Self {
        let m = Self::default();
        m.s.lock().unwrap().blocklist = ["admin", "steelcore"]
            .iter()
            .map(|t| BlockedTerm { skeleton: names::skeleton(t), contains: true })
            .chain([BlockedTerm { skeleton: names::skeleton("system"), contains: false }])
            .collect();
        m
    }

    fn audit_rows(&self) -> Vec<AuditEntry> {
        self.s.lock().unwrap().audit.clone()
    }

    fn facts(s: &MemState, name: &str, now: i64) -> NameFacts {
        let key = names::name_key(name);
        let skel = names::skeleton(name);
        let mut f = NameFacts { blocked: names::is_blocked(&skel, &s.blocklist), ..Default::default() };
        for (id, n) in &s.players {
            if let Some((n, _)) = n {
                if names::name_key(n) == key {
                    f.exact_owners.push(*id);
                }
                if names::skeleton(n) == skel {
                    f.similar_owners.push(*id);
                }
            }
        }
        for (id, k, sk, until) in &s.reservations {
            if *sk == skel && *until > now {
                f.reservations.push((*id, *k == key));
            }
        }
        f
    }

    fn state(s: &MemState, id: Uuid) -> PlayerNameState {
        let n = s.players.get(&id).cloned().flatten();
        PlayerNameState { player_id: id, current: n.as_ref().map(|x| x.0.clone()), changed_at: n.map(|x| x.1) }
    }

    fn view(s: &MemState, id: Uuid, provider: Option<Provider>) -> Option<PlayerView> {
        let n = s.players.get(&id)?.clone();
        let mut external_ids: Vec<String> = s
            .identities
            .iter()
            .filter(|((p, _), (_, pid, _))| Some(*p) == provider && *pid == id)
            .map(|((_, e), _)| e.clone())
            .collect();
        external_ids.sort();
        Some(PlayerView {
            player_id: id,
            display_name: n.as_ref().map(|x| x.0.clone()),
            display_name_changed_at: n.map(|x| x.1),
            external_ids,
        })
    }
}

fn det_uuid(parts: &[&str]) -> Uuid {
    let h = Sha256::digest(parts.join("|").as_bytes());
    Uuid::from_slice(&h[..16]).unwrap()
}

impl ProfileCore for MemoryCore {
    async fn resolve(&self, provider: Provider, external_id: &str, _source: &str) -> Result<IdentityView, StoreError> {
        let mut s = self.s.lock().unwrap();
        if provider == Provider::Insignia && !s.insignia_enabled {
            return Err(StoreError::ProviderDisabled(provider));
        }
        let canonical = provider.canonical_external_id(external_id)?;
        let key = (provider, canonical.clone());
        let created = !s.identities.contains_key(&key);
        if created {
            let pid = derive_player_id(provider, &canonical).ok_or(StoreError::InvalidExternalId)?;
            let iid = det_uuid(&[provider.id(), &canonical]);
            s.identities.insert(key.clone(), (iid, pid, false));
            s.players.entry(pid).or_insert(None);
        }
        let (iid, pid, suspended) = s.identities[&key];
        let display_name = s.players.get(&pid).cloned().flatten().map(|x| x.0);
        Ok(IdentityView { identity_id: iid, player_id: pid, external_id: canonical, created, display_name, suspended })
    }

    async fn get_player(&self, id: Uuid, provider: Option<Provider>) -> Result<Option<PlayerView>, StoreError> {
        Ok(Self::view(&self.s.lock().unwrap(), id, provider))
    }

    async fn check_display_name(
        &self,
        name: &str,
        player: Option<Uuid>,
        now: i64,
        policy: &NamePolicy,
    ) -> Result<Verdict, StoreError> {
        let s = self.s.lock().unwrap();
        let state = player.map(|id| Self::state(&s, id));
        Ok(names::decide(name, &Self::facts(&s, name, now), state.as_ref(), now, policy))
    }

    async fn set_display_name(
        &self,
        id: Uuid,
        name: &str,
        provider: Option<Provider>,
        now: i64,
        policy: &NamePolicy,
    ) -> Result<Option<SetNameOutcome>, StoreError> {
        let mut s = self.s.lock().unwrap();
        if !s.players.contains_key(&id) {
            return Ok(None);
        }
        let state = Self::state(&s, id);
        let verdict = names::decide(name, &Self::facts(&s, name, now), Some(&state), now, policy);
        if verdict.status == NameStatus::Ok {
            if let Some(old) = &state.current {
                let k = names::name_key(old);
                s.reservations.retain(|r| !(r.0 == id && r.1 == k));
                s.reservations.push((id, k, names::skeleton(old), now + policy.reservation));
            }
            let skel = names::skeleton(name);
            s.reservations.retain(|r| !(r.0 == id && r.2 == skel) && r.3 > now);
            s.players.insert(id, Some((name.to_owned(), now)));
        }
        let player = Self::view(&s, id, provider).unwrap();
        Ok(Some(SetNameOutcome { verdict, player }))
    }

    async fn set_identity_suspended(
        &self,
        provider: Provider,
        external_id: &str,
        suspended: bool,
    ) -> Result<Option<(Uuid, Uuid)>, StoreError> {
        let canonical = provider.canonical_external_id(external_id)?;
        let mut s = self.s.lock().unwrap();
        Ok(s.identities.get_mut(&(provider, canonical)).map(|e| {
            e.2 = suspended;
            (e.0, e.1)
        }))
    }

    async fn idempotency_claim(
        &self,
        access: &str,
        key: &str,
        method: &str,
        hash: &[u8],
        _now: i64,
    ) -> Result<IdempotencyClaim, StoreError> {
        let mut s = self.s.lock().unwrap();
        let k = (access.to_owned(), key.to_owned());
        Ok(match s.idempotency.get(&k) {
            None => {
                s.idempotency.insert(k, (method.to_owned(), hash.to_vec(), None));
                IdempotencyClaim::New
            }
            Some((m, h, _)) if m != method || h != hash => IdempotencyClaim::Mismatch,
            Some((_, _, None)) => IdempotencyClaim::InProgress,
            Some((_, _, Some(r))) => IdempotencyClaim::Replay(r.clone()),
        })
    }

    async fn idempotency_complete(&self, access: &str, key: &str, response: &[u8]) -> Result<(), StoreError> {
        if let Some(e) = self.s.lock().unwrap().idempotency.get_mut(&(access.to_owned(), key.to_owned())) {
            e.2 = Some(response.to_vec());
        }
        Ok(())
    }

    async fn idempotency_release(&self, access: &str, key: &str) -> Result<(), StoreError> {
        self.s.lock().unwrap().idempotency.remove(&(access.to_owned(), key.to_owned()));
        Ok(())
    }

    async fn audit(&self, entry: AuditEntry) -> Result<(), StoreError> {
        self.s.lock().unwrap().audit.push(entry);
        Ok(())
    }

    async fn sync_accesses(&self, accesses: &[AccessRecord]) -> Result<(), StoreError> {
        self.s.lock().unwrap().accesses = accesses.to_vec();
        Ok(())
    }
}

// ---- Certificates -------------------------------------------------------------

struct Pki {
    ca_pem: String,
    server_pem: String,
    server_key: String,
    /// name -> (cert DER, key DER)
    clients: HashMap<&'static str, (Vec<u8>, Vec<u8>)>,
}

fn ca(name: &str) -> CertifiedIssuer<'static, KeyPair> {
    let mut p = CertificateParams::new(Vec::<String>::new()).unwrap();
    p.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    p.distinguished_name.push(DnType::CommonName, name);
    p.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign, KeyUsagePurpose::DigitalSignature];
    CertifiedIssuer::self_signed(p, KeyPair::generate().unwrap()).unwrap()
}

fn leaf(issuer: &CertifiedIssuer<'static, KeyPair>, cn: &str, client: bool) -> (Vec<u8>, KeyPair) {
    let sans = if client { vec![] } else { vec!["localhost".to_owned()] };
    let mut p = CertificateParams::new(sans).unwrap();
    p.distinguished_name.push(DnType::CommonName, cn);
    p.extended_key_usages =
        vec![if client { ExtendedKeyUsagePurpose::ClientAuth } else { ExtendedKeyUsagePurpose::ServerAuth }];
    let key = KeyPair::generate().unwrap();
    let cert = p.signed_by(&key, issuer).unwrap();
    (cert.der().to_vec(), key)
}

fn pki() -> Pki {
    let ca = ca("steelcore test CA");
    let foreign = ca_foreign();
    let (server_der, server_key) = leaf(&ca, "steelcore", false);
    let server_pem = pem_encode("CERTIFICATE", &server_der);
    let mut clients = HashMap::new();
    for name in ["steelnet", "stats", "admin", "unregistered", "slow"] {
        let (der, key) = leaf(&ca, name, true);
        clients.insert(name, (der, key.serialize_der()));
    }
    let (der, key) = leaf(&foreign, "steelnet", true);
    clients.insert("foreign", (der, key.serialize_der()));
    Pki { ca_pem: ca.pem(), server_pem, server_key: server_key.serialize_pem(), clients }
}

fn ca_foreign() -> CertifiedIssuer<'static, KeyPair> {
    ca("some other CA")
}

fn pem_encode(label: &str, der: &[u8]) -> String {
    use std::fmt::Write;
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut b64 = String::new();
    for chunk in der.chunks(3) {
        let n = chunk.iter().fold(0u32, |a, &b| (a << 8) | u32::from(b)) << (8 * (3 - chunk.len()));
        for i in 0..4 {
            if i <= chunk.len() {
                b64.push(T[((n >> (18 - 6 * i)) & 63) as usize] as char);
            } else {
                b64.push('=');
            }
        }
    }
    let mut out = format!("-----BEGIN {label}-----\n");
    for line in b64.as_bytes().chunks(64) {
        writeln!(out, "{}", std::str::from_utf8(line).unwrap()).unwrap();
    }
    writeln!(out, "-----END {label}-----").unwrap();
    out
}

// ---- Test server ----------------------------------------------------------------

struct TestServer {
    addr: SocketAddr,
    pki: Arc<Pki>,
    store: Arc<MemoryCore>,
    clock: Arc<AtomicI64>,
    cancel: CancellationToken,
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

const T0: i64 = 1_800_000_000;

fn access(name: &str, pki: &Pki, provider: Option<Provider>, scopes: &[Scope], rate: f64, burst: f64) -> Access {
    Access {
        name: name.to_owned(),
        provider,
        cert_sha256: cert_fingerprint(&pki.clients[name].0),
        scopes: scopes.to_vec(),
        rate_per_second: rate,
        burst,
    }
}

async fn start(pki: Arc<Pki>) -> TestServer {
    let store = Arc::new(MemoryCore::new());
    let accesses = AccessList::new(vec![
        access(
            "steelnet",
            &pki,
            Some(Provider::Steelbound),
            &[Scope::IdentityWrite, Scope::ProfileWrite, Scope::ReadPublic],
            1000.0,
            1000.0,
        ),
        access("stats", &pki, None, &[Scope::ReadPublic], 1000.0, 1000.0),
        access("admin", &pki, None, &[Scope::Admin, Scope::ReadPublic], 1000.0, 1000.0),
        access("slow", &pki, Some(Provider::Steelbound), &[Scope::ReadPublic, Scope::IdentityWrite], 0.001, 3.0),
    ])
    .unwrap();
    store.sync_accesses(&accesses.records()).await.unwrap();
    let clock = Arc::new(AtomicI64::new(T0));
    let c2 = clock.clone();
    let api = Arc::new(CoreApi::new(store.clone(), NamePolicy::default(), Arc::new(move || c2.load(Ordering::SeqCst))));
    let tls = tls_config(pki.server_pem.as_bytes(), pki.server_key.as_bytes(), pki.ca_pem.as_bytes()).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let cancel = CancellationToken::new();
    tokio::spawn(serve(listener, tls, Arc::new(accesses), api, 16, cancel.clone()));
    TestServer { addr, pki, store, clock, cancel }
}

impl TestServer {
    fn advance_days(&self, days: i64) {
        self.clock.fetch_add(days * 86_400, Ordering::SeqCst);
    }

    fn connector(&self, client: Option<&str>, alpn: &[u8]) -> TlsConnector {
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let mut roots = RootCertStore::empty();
        for c in rustls::pki_types::pem::PemObject::pem_slice_iter(self.pki.ca_pem.as_bytes()) {
            roots.add(c.unwrap()).unwrap();
        }
        let b = ClientConfig::builder_with_provider(provider)
            .with_protocol_versions(&[&rustls::version::TLS13])
            .unwrap()
            .with_root_certificates(roots);
        let mut cfg = match client {
            Some(name) => {
                let (der, key) = &self.pki.clients[name];
                b.with_client_auth_cert(
                    vec![CertificateDer::from(der.clone())],
                    PrivateKeyDer::try_from(key.clone()).unwrap(),
                )
                .unwrap()
            }
            None => b.with_no_client_auth(),
        };
        cfg.alpn_protocols = vec![alpn.to_vec()];
        TlsConnector::from(Arc::new(cfg))
    }

    /// One Connect-JSON call over HTTP/1.1. Returns (HTTP status, body).
    async fn json(&self, client: Option<&str>, method: &str, body: Value) -> Result<(u16, Value), String> {
        let tcp = TcpStream::connect(self.addr).await.map_err(|e| e.to_string())?;
        let mut tls = self
            .connector(client, b"http/1.1")
            .connect(ServerName::try_from("localhost").unwrap(), tcp)
            .await
            .map_err(|e| format!("tls: {e}"))?;
        let body = body.to_string();
        let req = format!(
            "POST /{method} HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\n\
             Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        tls.write_all(req.as_bytes()).await.map_err(|e| format!("write: {e}"))?;
        let mut raw = Vec::new();
        // TLS 1.3 reports a refused client certificate only on the first read.
        tls.read_to_end(&mut raw).await.map_err(|e| format!("read: {e}"))?;
        let text = String::from_utf8(raw).map_err(|e| e.to_string())?;
        let (head, body) = text.split_once("\r\n\r\n").ok_or("no http response")?;
        let status: u16 = head.split(' ').nth(1).ok_or("status")?.parse().map_err(|_| "status")?;
        Ok((status, serde_json::from_str(body).unwrap_or(Value::Null)))
    }

    async fn channel(&self, client: Option<&str>) -> Result<tonic::transport::Channel, String> {
        let connector = self.connector(client, b"h2");
        let addr = self.addr;
        tonic::transport::Endpoint::from_static("http://localhost")
            .connect_with_connector(tower::service_fn(move |_| {
                let connector = connector.clone();
                async move {
                    let tcp = TcpStream::connect(addr).await?;
                    let tls = connector.connect(ServerName::try_from("localhost").unwrap(), tcp).await?;
                    Ok::<_, std::io::Error>(hyper_util::rt::TokioIo::new(tls))
                }
            }))
            .await
            .map_err(|e| format!("connect: {e}"))
    }
}

// ---- One call, either binding, normalized to JSON --------------------------------

#[derive(Clone, Copy, Debug, PartialEq)]
enum Binding {
    Grpc,
    Json,
}

fn grpc_code(c: tonic::Code) -> &'static str {
    match c {
        tonic::Code::InvalidArgument => "invalid_argument",
        tonic::Code::Unauthenticated => "unauthenticated",
        tonic::Code::PermissionDenied => "permission_denied",
        tonic::Code::NotFound => "not_found",
        tonic::Code::AlreadyExists => "already_exists",
        tonic::Code::ResourceExhausted => "resource_exhausted",
        tonic::Code::FailedPrecondition => "failed_precondition",
        tonic::Code::Aborted => "aborted",
        tonic::Code::Internal => "internal",
        tonic::Code::Unavailable => "unavailable",
        _ => "other",
    }
}

/// Call `method` with a JSON request. Result: Ok(response JSON) or
/// Err(error code); transport failures (TLS refused) as Err("transport").
async fn call(srv: &TestServer, b: Binding, client: Option<&str>, m: &str, req: Value) -> Result<Value, String> {
    match b {
        Binding::Json => match srv.json(client, m, req).await {
            Ok((200, v)) => Ok(v),
            Ok((_, v)) => Err(v["code"].as_str().unwrap_or("?").to_owned()),
            Err(_) => Err("transport".into()),
        },
        Binding::Grpc => {
            let ch = match srv.channel(client).await {
                Ok(c) => c,
                Err(_) => return Err("transport".into()),
            };
            macro_rules! go {
                ($client:ty, $f:ident, $req:ty) => {{
                    let r: $req = serde_json::from_value(req).unwrap();
                    match <$client>::new(ch).$f(r).await {
                        Ok(resp) => Ok(serde_json::to_value(resp.into_inner()).unwrap()),
                        // TLS 1.3: a refused client certificate shows up on the first request.
                        Err(s) if matches!(s.code(), tonic::Code::Unknown | tonic::Code::Unavailable | tonic::Code::Cancelled) => {
                            Err("transport".into())
                        }
                        Err(s) => Err(grpc_code(s.code()).to_owned()),
                    }
                }};
            }
            use pb::{
                admin_service_client::AdminServiceClient as A, identity_service_client::IdentityServiceClient as I,
                player_service_client::PlayerServiceClient as P,
            };
            type Ch = tonic::transport::Channel;
            match m {
                method::RESOLVE_IDENTITY => go!(I<Ch>, resolve_identity, pb::ResolveIdentityRequest),
                method::GET_PLAYER => go!(P<Ch>, get_player, pb::GetPlayerRequest),
                method::CHECK_DISPLAY_NAME => go!(P<Ch>, check_display_name, pb::CheckDisplayNameRequest),
                method::SET_DISPLAY_NAME => go!(P<Ch>, set_display_name, pb::SetDisplayNameRequest),
                method::SET_IDENTITY_SUSPENDED => go!(A<Ch>, set_identity_suspended, pb::SetIdentitySuspendedRequest),
                _ => unreachable!(),
            }
        }
    }
}

const BOTH: [Binding; 2] = [Binding::Grpc, Binding::Json];

fn status_of(v: &Value) -> &str {
    v["status"].as_str().unwrap_or("DISPLAY_NAME_STATUS_UNSPECIFIED")
}

// ---- Tests ------------------------------------------------------------------------

#[test]
fn contract_compiles_and_matches_the_method_table() {
    let fds = prost_types::FileDescriptorSet::decode(pb::DESCRIPTOR).unwrap();
    let mut methods = Vec::new();
    for f in &fds.file {
        assert_eq!(f.package(), "steelcore.v1");
        for s in &f.service {
            for m in &s.method {
                methods.push(format!("steelcore.v1.{}/{}", s.name(), m.name()));
            }
        }
    }
    methods.sort();
    let mut expected = vec![
        method::RESOLVE_IDENTITY,
        method::GET_PLAYER,
        method::CHECK_DISPLAY_NAME,
        method::SET_DISPLAY_NAME,
        method::SET_IDENTITY_SUSPENDED,
    ];
    expected.sort();
    assert_eq!(methods, expected);
}

/// A fixed sequence of calls covering every method, success and refusal.
async fn transcript(b: Binding) -> Vec<(String, Result<Value, String>)> {
    let srv = start(Arc::new(pki())).await;
    let mut out = Vec::new();
    let steps: Vec<(&str, &str, Value)> = vec![
        ("steelnet", method::RESOLVE_IDENTITY, json!({"externalId": "42"})),
        ("steelnet", method::RESOLVE_IDENTITY, json!({"externalId": "42"})),
        ("steelnet", method::RESOLVE_IDENTITY, json!({"externalId": "43"})),
        ("steelnet", method::RESOLVE_IDENTITY, json!({"externalId": "0042"})),
        ("steelnet", method::CHECK_DISPLAY_NAME, json!({"name": "Alice"})),
        ("steelnet", method::SET_DISPLAY_NAME, json!({"playerId": "00000000-0000-0000-5342-4c010000002a", "name": "Alice", "idempotencyKey": "key-0001"})),
        ("steelnet", method::SET_DISPLAY_NAME, json!({"playerId": "00000000-0000-0000-5342-4c010000002b", "name": "AIice", "idempotencyKey": "key-0002"})),
        ("steelnet", method::CHECK_DISPLAY_NAME, json!({"name": "Adm1n Bob"})),
        ("steelnet", method::GET_PLAYER, json!({"playerId": "00000000-0000-0000-5342-4c010000002a"})),
        ("stats", method::GET_PLAYER, json!({"playerId": "00000000-0000-0000-5342-4c010000002a"})),
        ("stats", method::RESOLVE_IDENTITY, json!({"externalId": "44"})),
        ("stats", method::SET_DISPLAY_NAME, json!({"playerId": "00000000-0000-0000-5342-4c010000002a", "name": "Eve", "idempotencyKey": "key-0003"})),
        ("steelnet", method::SET_IDENTITY_SUSPENDED, json!({"provider": "steelbound", "externalId": "43", "suspended": true, "idempotencyKey": "key-0004"})),
        ("admin", method::SET_IDENTITY_SUSPENDED, json!({"provider": "steelbound", "externalId": "43", "suspended": true, "idempotencyKey": "key-0005"})),
        ("steelnet", method::RESOLVE_IDENTITY, json!({"externalId": "43"})),
        ("unregistered", method::GET_PLAYER, json!({"playerId": "00000000-0000-0000-5342-4c010000002a"})),
        ("steelnet", method::GET_PLAYER, json!({"playerId": "not-a-uuid"})),
        ("steelnet", method::GET_PLAYER, json!({"playerId": "00000000-0000-0000-5342-4c01000000ff"})),
    ];
    for (who, m, req) in steps {
        let r = call(&srv, b, Some(who), m, req.clone()).await;
        out.push((format!("{who} {m} {req}"), r));
    }
    out
}

#[tokio::test]
async fn both_bindings_give_identical_results() {
    let g = transcript(Binding::Grpc).await;
    let j = transcript(Binding::Json).await;
    assert_eq!(g.len(), j.len());
    for (a, b) in g.iter().zip(&j) {
        assert_eq!(a, b, "bindings differ");
    }
    // Spot checks of the shared transcript.
    let r = |i: usize| g[i].1.clone();
    assert_eq!(r(0).unwrap()["created"], json!(true));
    assert_eq!(r(0).unwrap()["playerId"], json!("00000000-0000-0000-5342-4c010000002a"));
    assert_eq!(r(1).unwrap().get("created"), None, "false is the default and omitted");
    assert_eq!(r(3), Err("invalid_argument".into()), "external ids are canonical, no leading zeros");
    assert_eq!(status_of(&r(4).unwrap()), "DISPLAY_NAME_STATUS_OK");
    assert_eq!(status_of(&r(5).unwrap()), "DISPLAY_NAME_STATUS_OK");
    assert_eq!(status_of(&r(6).unwrap()), "DISPLAY_NAME_STATUS_SIMILAR");
    assert_eq!(status_of(&r(7).unwrap()), "DISPLAY_NAME_STATUS_BLOCKED");
    assert_eq!(r(8).unwrap()["player"]["externalIds"], json!(["42"]));
    assert_eq!(r(9).unwrap()["player"].get("externalIds"), None, "no provider, no external ids");
    assert_eq!(r(10), Err("permission_denied".into()));
    assert_eq!(r(11), Err("permission_denied".into()));
    assert_eq!(r(12), Err("permission_denied".into()));
    assert_eq!(r(13).unwrap()["suspended"], json!(true));
    assert_eq!(r(14).unwrap()["suspended"], json!(true));
    assert_eq!(r(15), Err("unauthenticated".into()));
    assert_eq!(r(16), Err("invalid_argument".into()));
    assert_eq!(r(17), Err("not_found".into()));
}

#[tokio::test]
async fn missing_or_foreign_certificates_are_refused() {
    let srv = start(Arc::new(pki())).await;
    for b in BOTH {
        for client in [None, Some("foreign")] {
            let r = call(&srv, b, client, method::GET_PLAYER, json!({"playerId": Uuid::nil().to_string()})).await;
            assert_eq!(r, Err("transport".into()), "{b:?} {client:?}");
        }
        let r = call(&srv, b, Some("unregistered"), method::RESOLVE_IDENTITY, json!({"externalId": "7"})).await;
        assert_eq!(r, Err("unauthenticated".into()), "{b:?}");
    }
    assert!(srv.store.s.lock().unwrap().identities.is_empty(), "nothing was written");
    assert!(srv.store.audit_rows().is_empty(), "unknown callers leave no audit rows");
}

#[tokio::test]
async fn scopes_are_enforced_per_method() {
    let srv = start(Arc::new(pki())).await;
    let pid = call(&srv, Binding::Json, Some("steelnet"), method::RESOLVE_IDENTITY, json!({"externalId": "5"}))
        .await
        .unwrap()["playerId"]
        .as_str()
        .unwrap()
        .to_owned();
    let set = json!({"playerId": pid, "name": "Carol", "idempotencyKey": "scope-0001"});
    let susp = json!({"provider": "steelbound", "externalId": "5", "suspended": true, "idempotencyKey": "scope-0002"});
    for b in BOTH {
        // stats: read only
        assert!(call(&srv, b, Some("stats"), method::GET_PLAYER, json!({"playerId": pid})).await.is_ok());
        assert!(call(&srv, b, Some("stats"), method::CHECK_DISPLAY_NAME, json!({"name": "Carol"})).await.is_ok());
        for (m, req) in [
            (method::RESOLVE_IDENTITY, json!({"externalId": "6"})),
            (method::SET_DISPLAY_NAME, set.clone()),
            (method::SET_IDENTITY_SUSPENDED, susp.clone()),
        ] {
            assert_eq!(call(&srv, b, Some("stats"), m, req).await, Err("permission_denied".into()), "{b:?} {m}");
        }
        // admin: no identity or profile writes
        assert_eq!(call(&srv, b, Some("admin"), method::SET_DISPLAY_NAME, set.clone()).await, Err("permission_denied".into()));
        assert_eq!(
            call(&srv, b, Some("admin"), method::RESOLVE_IDENTITY, json!({"externalId": "6"})).await,
            Err("permission_denied".into())
        );
        // steelnet: no admin
        assert_eq!(call(&srv, b, Some("steelnet"), method::SET_IDENTITY_SUSPENDED, susp.clone()).await, Err("permission_denied".into()));
    }
    let denied: Vec<_> = srv.store.audit_rows().into_iter().filter(|a| a.outcome == "permission_denied").collect();
    assert_eq!(denied.len(), 2 * 6, "every refused write is audited");
    assert!(denied.iter().all(|a| a.identity_id.is_none()));
}

#[tokio::test]
async fn idempotency_keys_replay_and_reject_reuse() {
    let srv = start(Arc::new(pki())).await;
    for (i, b) in BOTH.into_iter().enumerate() {
        let ext = format!("{}", 100 + i);
        let pid = call(&srv, b, Some("steelnet"), method::RESOLVE_IDENTITY, json!({"externalId": ext})).await.unwrap()
            ["playerId"]
            .clone();
        let key = format!("idem-key-{i}");
        let name = ["Dora", "Dana"][i];
        let first =
            call(&srv, b, Some("steelnet"), method::SET_DISPLAY_NAME, json!({"playerId": pid, "name": name, "idempotencyKey": key}))
                .await
                .unwrap();
        assert_eq!(status_of(&first), "DISPLAY_NAME_STATUS_OK");
        // Same key, same request: the first response again (not UNCHANGED).
        let again =
            call(&srv, b, Some("steelnet"), method::SET_DISPLAY_NAME, json!({"playerId": pid, "name": name, "idempotencyKey": key}))
                .await
                .unwrap();
        assert_eq!(again, first);
        // Same key, other request.
        let other =
            call(&srv, b, Some("steelnet"), method::SET_DISPLAY_NAME, json!({"playerId": pid, "name": "Zed", "idempotencyKey": key}))
                .await;
        assert_eq!(other, Err("already_exists".into()));
        // Key required for SetDisplayName, and well-formed.
        for bad in [json!(""), json!("short"), json!("has space in it")] {
            let r = call(&srv, b, Some("steelnet"), method::SET_DISPLAY_NAME, json!({"playerId": pid, "name": "Zed", "idempotencyKey": bad}))
                .await;
            assert_eq!(r, Err("invalid_argument".into()));
        }
        // ResolveIdentity with a key: replays "created".
        let k = format!("resolve-key-{i}");
        let ext2 = format!("{}", 200 + i);
        let a = call(&srv, b, Some("steelnet"), method::RESOLVE_IDENTITY, json!({"externalId": ext2, "idempotencyKey": k})).await.unwrap();
        let c = call(&srv, b, Some("steelnet"), method::RESOLVE_IDENTITY, json!({"externalId": ext2, "idempotencyKey": k})).await.unwrap();
        assert_eq!((a["created"].clone(), c["created"].clone()), (json!(true), json!(true)));
    }
    // Keys are per access: another access may use the same key.
    let audit = srv.store.audit_rows();
    let sets = audit.iter().filter(|a| a.method == method::SET_DISPLAY_NAME && a.outcome == "ok").count();
    assert_eq!(sets, 2, "replays do not run (or audit) the call again");
}

#[tokio::test]
async fn rate_limit_per_access() {
    let srv = start(Arc::new(pki())).await;
    for b in BOTH {
        let srv = start(srv.pki.clone()).await; // fresh bucket per binding
        let mut results = Vec::new();
        for _ in 0..4 {
            results.push(call(&srv, b, Some("slow"), method::CHECK_DISPLAY_NAME, json!({"name": "Erin"})).await.is_ok());
        }
        assert_eq!(results, [true, true, true, false], "{b:?}");
        assert_eq!(
            call(&srv, b, Some("slow"), method::CHECK_DISPLAY_NAME, json!({"name": "Erin"})).await,
            Err("resource_exhausted".into())
        );
        // Other accesses are not affected.
        assert!(call(&srv, b, Some("steelnet"), method::CHECK_DISPLAY_NAME, json!({"name": "Erin"})).await.is_ok());
        // A refused write of a known access is audited.
        let _ = call(&srv, b, Some("slow"), method::RESOLVE_IDENTITY, json!({"externalId": "9"})).await;
        assert!(srv.store.audit_rows().iter().any(|a| a.access == "slow" && a.outcome == "resource_exhausted"));
    }
}

#[tokio::test]
async fn audit_rows_name_who_and_what_but_no_contents() {
    let srv = start(Arc::new(pki())).await;
    let r = call(&srv, Binding::Grpc, Some("steelnet"), method::RESOLVE_IDENTITY, json!({"externalId": "77"})).await.unwrap();
    let pid = r["playerId"].clone();
    call(&srv, Binding::Json, Some("steelnet"), method::SET_DISPLAY_NAME, json!({"playerId": pid, "name": "Secret Name", "idempotencyKey": "audit-0001"}))
        .await
        .unwrap();
    call(&srv, Binding::Json, Some("steelnet"), method::CHECK_DISPLAY_NAME, json!({"name": "Other"})).await.unwrap();
    let rows = srv.store.audit_rows();
    assert_eq!(rows.len(), 2, "reads are not audited: {rows:?}");
    assert_eq!(rows[0].method, method::RESOLVE_IDENTITY);
    assert_eq!(rows[0].outcome, "created");
    assert!(rows[0].identity_id.is_some() && rows[0].player_id.is_some());
    assert_eq!(rows[1].method, method::SET_DISPLAY_NAME);
    assert_eq!(rows[1].outcome, "ok");
    for row in &rows {
        assert_eq!(row.access, "steelnet");
        assert_eq!(row.provider, Some(Provider::Steelbound));
        assert!(!format!("{row:?}").contains("Secret"), "no request contents in the audit");
    }
}

#[tokio::test]
async fn display_names_unique_limited_and_reserved() {
    let srv = start(Arc::new(pki())).await;
    let b = Binding::Json;
    let resolve = |ext: &'static str| {
        let srv = &srv;
        async move {
            call(srv, b, Some("steelnet"), method::RESOLVE_IDENTITY, json!({"externalId": ext})).await.unwrap()["playerId"]
                .as_str()
                .unwrap()
                .to_owned()
        }
    };
    let (a, bb) = (resolve("1").await, resolve("2").await);
    let mut n = 0;
    let mut set = async |pid: &str, name: &str| {
        n += 1;
        let r = call(&srv, b, Some("steelnet"), method::SET_DISPLAY_NAME, json!({"playerId": pid, "name": name, "idempotencyKey": format!("names-{n:04}")}))
            .await
            .unwrap();
        (status_of(&r).trim_start_matches("DISPLAY_NAME_STATUS_").to_owned(), r)
    };
    assert_eq!(set(&a, "Alice").await.0, "OK");
    assert_eq!(set(&bb, "alice").await.0, "TAKEN");
    assert_eq!(set(&bb, "A1ice").await.0, "SIMILAR");
    assert_eq!(set(&bb, "Al ice").await.0, "SIMILAR");
    assert_eq!(set(&bb, "1Alice").await.0, "INVALID_CHARACTERS");
    assert_eq!(set(&bb, "Sixteen chars xx").await.0, "INVALID_LENGTH");
    assert_eq!(set(&bb, "Steelcore Fan").await.0, "BLOCKED");
    assert_eq!(set(&a, "Alice").await.0, "UNCHANGED");
    // Change limit: 7 days.
    let (st, r) = set(&a, "Alicia").await;
    assert_eq!(st, "CHANGE_LIMIT");
    assert_eq!(r["nextChangeAt"], json!((T0 + 7 * 86_400).to_string()));
    assert_eq!(r["player"]["displayName"], json!("Alice"));
    srv.advance_days(8);
    assert_eq!(set(&a, "Alicia").await.0, "OK");
    // "Alice" is now reserved for a, for 30 days.
    assert_eq!(set(&bb, "Alice").await.0, "RESERVED");
    assert_eq!(set(&bb, "AIice").await.0, "SIMILAR");
    let chk = call(&srv, b, Some("steelnet"), method::CHECK_DISPLAY_NAME, json!({"name": "Alice", "playerId": a})).await.unwrap();
    assert_eq!(status_of(&chk), "DISPLAY_NAME_STATUS_CHANGE_LIMIT", "own reservation is free, but the limit applies");
    srv.advance_days(31);
    assert_eq!(set(&bb, "Alice").await.0, "OK");
    let p = call(&srv, b, Some("steelnet"), method::GET_PLAYER, json!({"playerId": bb})).await.unwrap();
    assert_eq!(p["player"]["displayName"], json!("Alice"));
}

#[tokio::test]
async fn providers_only_reach_their_own_players() {
    let srv = start(Arc::new(pki())).await;
    // An Insignia player (prepared provider), created directly in the store.
    srv.store.s.lock().unwrap().insignia_enabled = true;
    let x = srv.store.resolve(Provider::Insignia, "0009ffff00000001", "test").await.unwrap();
    for b in BOTH {
        let key = format!("other-prov-{b:?}");
        let r = call(&srv, b, Some("steelnet"), method::SET_DISPLAY_NAME, json!({"playerId": x.player_id.to_string(), "name": "Hijack", "idempotencyKey": key}))
            .await;
        assert_eq!(r, Err("permission_denied".into()), "{b:?}");
        let r = call(&srv, b, Some("steelnet"), method::SET_DISPLAY_NAME, json!({"playerId": Uuid::from_u128(5).to_string(), "name": "Nobody", "idempotencyKey": format!("{key}-2")}))
            .await;
        assert_eq!(r, Err("not_found".into()), "{b:?}");
        // GetPlayer works (public), but shows no foreign external ids.
        let p = call(&srv, b, Some("steelnet"), method::GET_PLAYER, json!({"playerId": x.player_id.to_string()})).await.unwrap();
        assert_eq!(p["player"].get("externalIds"), None);
    }
    // ResolveIdentity always uses the access's provider: "0009ffff00000001" is
    // no valid Steelbound account id.
    let r = call(&srv, Binding::Json, Some("steelnet"), method::RESOLVE_IDENTITY, json!({"externalId": "0009ffff00000001"})).await;
    assert_eq!(r, Err("invalid_argument".into()));
}

#[tokio::test]
async fn connect_json_edge_cases() {
    let srv = start(Arc::new(pki())).await;
    // Unknown method, wrong content type, malformed JSON, unknown fields.
    let (st, v) = srv.json(Some("steelnet"), "steelcore.v1.PlayerService/Nope", json!({})).await.unwrap();
    assert_eq!((st, v["code"].as_str()), (404, Some("unimplemented")));
    let (st, v) = srv.json(Some("steelnet"), method::CHECK_DISPLAY_NAME, json!({"name": "Ok", "futureField": 1})).await.unwrap();
    assert_eq!(st, 200, "{v}");
    let (st, _) = srv.json(Some("steelnet"), method::CHECK_DISPLAY_NAME, json!({"name": 5})).await.unwrap();
    assert_eq!(st, 400);
}
