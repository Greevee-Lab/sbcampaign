// SPDX-License-Identifier: AGPL-3.0-only
//! The steelcore.v1 methods, independent of the binding. Both bindings (gRPC
//! and Connect-JSON) call exactly these functions, so they behave alike:
//! authentication, scopes, rate limit, idempotency and audit happen here.

use super::{
    access::{Access, Scope},
    pb,
    ratelimit::RateLimiter,
};
use crate::core_api::{
    Provider,
    names::{NamePolicy, NameStatus, Verdict},
    profile::{AuditEntry, IdempotencyClaim, PlayerView, ProfileCore, StoreError},
};
use prost::Message;
use sha2::{Digest, Sha256};
use std::{future::Future, sync::Arc};
use uuid::Uuid;

/// Error classes of the API; the same set as gRPC status codes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Code {
    InvalidArgument,
    Unauthenticated,
    PermissionDenied,
    NotFound,
    AlreadyExists,
    ResourceExhausted,
    FailedPrecondition,
    Aborted,
    Internal,
    Unavailable,
}

impl Code {
    /// Connect protocol name (also the gRPC name in snake case).
    pub fn as_str(self) -> &'static str {
        match self {
            Code::InvalidArgument => "invalid_argument",
            Code::Unauthenticated => "unauthenticated",
            Code::PermissionDenied => "permission_denied",
            Code::NotFound => "not_found",
            Code::AlreadyExists => "already_exists",
            Code::ResourceExhausted => "resource_exhausted",
            Code::FailedPrecondition => "failed_precondition",
            Code::Aborted => "aborted",
            Code::Internal => "internal",
            Code::Unavailable => "unavailable",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiError {
    pub code: Code,
    pub message: String,
}

impl ApiError {
    pub fn new(code: Code, message: impl Into<String>) -> Self {
        Self { code, message: message.into() }
    }
}

impl From<StoreError> for ApiError {
    fn from(err: StoreError) -> Self {
        match err {
            StoreError::InvalidExternalId => ApiError::new(Code::InvalidArgument, "invalid external_id"),
            StoreError::ProviderDisabled(p) => ApiError::new(Code::FailedPrecondition, format!("provider {p} is not enabled")),
            StoreError::Storage(e) => {
                let _ = e; // never leak storage details to callers
                ApiError::new(Code::Unavailable, "storage unavailable")
            }
        }
    }
}

pub type ApiResult<T> = Result<T, ApiError>;

/// Who is calling: the access matched by the client certificate, or None
/// for a certificate of the steelcore CA that is not configured.
#[derive(Debug, Clone)]
pub struct Caller(pub Option<Arc<Access>>);

/// Fully qualified method names (also the HTTP paths without the leading /).
pub mod method {
    pub const RESOLVE_IDENTITY: &str = "steelcore.v1.IdentityService/ResolveIdentity";
    pub const GET_PLAYER: &str = "steelcore.v1.PlayerService/GetPlayer";
    pub const CHECK_DISPLAY_NAME: &str = "steelcore.v1.PlayerService/CheckDisplayName";
    pub const SET_DISPLAY_NAME: &str = "steelcore.v1.PlayerService/SetDisplayName";
    pub const SET_IDENTITY_SUSPENDED: &str = "steelcore.v1.AdminService/SetIdentitySuspended";
}

/// Clock in unix seconds (replaceable in tests).
pub type Clock = Arc<dyn Fn() -> i64 + Send + Sync>;

pub fn system_clock() -> Clock {
    Arc::new(|| chrono::Utc::now().timestamp())
}

pub struct CoreApi<S: ProfileCore> {
    store: Arc<S>,
    policy: NamePolicy,
    limiter: RateLimiter,
    clock: Clock,
}

fn valid_idempotency_key(k: &str) -> bool {
    (8..=64).contains(&k.len()) && k.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

fn parse_player_id(s: &str) -> ApiResult<Uuid> {
    Uuid::parse_str(s).map_err(|_| ApiError::new(Code::InvalidArgument, "player_id must be a UUID"))
}

fn status_pb(s: NameStatus) -> pb::DisplayNameStatus {
    use pb::DisplayNameStatus as D;
    match s {
        NameStatus::Ok => D::Ok,
        NameStatus::InvalidLength => D::InvalidLength,
        NameStatus::InvalidCharacters => D::InvalidCharacters,
        NameStatus::Taken => D::Taken,
        NameStatus::Similar => D::Similar,
        NameStatus::Reserved => D::Reserved,
        NameStatus::Blocked => D::Blocked,
        NameStatus::ChangeLimit => D::ChangeLimit,
        NameStatus::Unchanged => D::Unchanged,
    }
}

fn player_pb(p: &PlayerView, policy: &NamePolicy) -> pb::Player {
    let next = match (p.display_name.as_ref(), p.display_name_changed_at) {
        (Some(_), Some(t)) => t + policy.change_interval,
        _ => 0,
    };
    pb::Player {
        player_id: p.player_id.to_string(),
        display_name: p.display_name.clone().unwrap_or_default(),
        display_name_changed_at: p.display_name_changed_at.unwrap_or(0),
        next_display_name_change_at: next,
        external_ids: p.external_ids.clone(),
    }
}

/// What a write call reports for its audit row.
#[derive(Default)]
struct AuditInfo {
    identity_id: Option<Uuid>,
    player_id: Option<Uuid>,
    outcome: &'static str,
}

impl<S: ProfileCore> CoreApi<S> {
    pub fn new(store: Arc<S>, policy: NamePolicy, clock: Clock) -> Self {
        Self { store, policy, limiter: RateLimiter::default(), clock }
    }

    pub fn store(&self) -> &Arc<S> {
        &self.store
    }

    fn now(&self) -> i64 {
        (self.clock)()
    }

    /// Authentication, scope and rate limit for one call.
    fn admit(&self, caller: &Caller, scope: Scope) -> ApiResult<Arc<Access>> {
        let access = caller
            .0
            .clone()
            .ok_or_else(|| ApiError::new(Code::Unauthenticated, "client certificate is not registered"))?;
        if !self.limiter.allow(&access.name, access.rate_per_second, access.burst) {
            return Err(ApiError::new(Code::ResourceExhausted, "rate limit"));
        }
        if !access.has(scope) {
            return Err(ApiError::new(Code::PermissionDenied, format!("scope {scope} required")));
        }
        Ok(access)
    }

    async fn audit(&self, access: &Access, method: &'static str, info: &AuditInfo) {
        let entry = AuditEntry {
            access: access.name.clone(),
            provider: access.provider,
            method,
            identity_id: info.identity_id,
            player_id: info.player_id,
            outcome: info.outcome.to_owned(),
        };
        if let Err(e) = self.store.audit(entry).await {
            let _ = systemd_log!(3, "core api: audit write failed: {}", e);
        }
    }

    /// Runs a write call: admission, idempotency, audit.
    ///
    /// `request` is the request with its idempotency key cleared; its encoding
    /// identifies the request for key reuse checks.
    async fn write<Req, Resp, F, Fut>(
        &self,
        caller: &Caller,
        scope: Scope,
        method: &'static str,
        key: &str,
        key_required: bool,
        request: &Req,
        run: F,
    ) -> ApiResult<Resp>
    where
        Req: Message,
        Resp: Message + Default,
        F: FnOnce(Arc<Access>) -> Fut,
        Fut: Future<Output = Result<(Resp, AuditInfo), (ApiError, AuditInfo)>>,
    {
        let access = match self.admit(caller, scope) {
            Ok(a) => a,
            Err(e) => {
                // Audit refused writes of known accesses (scope, rate).
                if let Some(a) = &caller.0 {
                    let outcome = e.code.as_str();
                    self.audit(a, method, &AuditInfo { outcome, ..Default::default() }).await;
                }
                return Err(e);
            }
        };
        if key.is_empty() && key_required {
            return Err(ApiError::new(Code::InvalidArgument, "idempotency_key required"));
        }
        if !key.is_empty() && !valid_idempotency_key(key) {
            return Err(ApiError::new(Code::InvalidArgument, "idempotency_key: 8..64 of A-Z a-z 0-9 _ -"));
        }

        if !key.is_empty() {
            let mut h = Sha256::new();
            h.update(method.as_bytes());
            h.update([0]);
            h.update(request.encode_to_vec());
            let hash = h.finalize();
            match self.store.idempotency_claim(&access.name, key, method, &hash, self.now()).await? {
                IdempotencyClaim::New => {}
                IdempotencyClaim::Replay(bytes) => {
                    return Resp::decode(bytes.as_slice())
                        .map_err(|_| ApiError::new(Code::Internal, "stored response unreadable"));
                }
                IdempotencyClaim::Mismatch => {
                    return Err(ApiError::new(Code::AlreadyExists, "idempotency_key was used for another request"));
                }
                IdempotencyClaim::InProgress => {
                    return Err(ApiError::new(Code::Aborted, "a call with this idempotency_key is in progress"));
                }
            }
        }

        let result = run(access.clone()).await;
        let info = match &result {
            Ok((_, i)) | Err((_, i)) => i,
        };
        self.audit(&access, method, info).await;
        match result {
            Ok((resp, _)) => {
                if !key.is_empty() {
                    self.store.idempotency_complete(&access.name, key, &resp.encode_to_vec()).await?;
                }
                Ok(resp)
            }
            Err((e, _)) => {
                if !key.is_empty() {
                    let _ = self.store.idempotency_release(&access.name, key).await;
                }
                Err(e)
            }
        }
    }

    // ---- IdentityService --------------------------------------------------

    pub async fn resolve_identity(
        &self,
        caller: &Caller,
        req: pb::ResolveIdentityRequest,
    ) -> ApiResult<pb::ResolveIdentityResponse> {
        let key = req.idempotency_key.clone();
        let mut keyless = req.clone();
        keyless.idempotency_key.clear();
        self.write(caller, Scope::IdentityWrite, method::RESOLVE_IDENTITY, &key, false, &keyless, |access| async move {
            // admit() guarantees a provider for identity:write (config check).
            let provider = access.provider.ok_or_else(|| {
                (ApiError::new(Code::PermissionDenied, "access has no provider"), AuditInfo { outcome: "permission_denied", ..Default::default() })
            })?;
            match self.store.resolve(provider, &req.external_id, &access.name).await {
                Ok(v) => Ok((
                    pb::ResolveIdentityResponse {
                        player_id: v.player_id.to_string(),
                        identity_id: v.identity_id.to_string(),
                        external_id: v.external_id,
                        created: v.created,
                        display_name: v.display_name.unwrap_or_default(),
                        suspended: v.suspended,
                    },
                    AuditInfo {
                        identity_id: Some(v.identity_id),
                        player_id: Some(v.player_id),
                        outcome: if v.created { "created" } else { "ok" },
                    },
                )),
                Err(e) => {
                    let e = ApiError::from(e);
                    let outcome = e.code.as_str();
                    Err((e, AuditInfo { outcome, ..Default::default() }))
                }
            }
        })
        .await
    }

    // ---- PlayerService ------------------------------------------------------

    pub async fn get_player(&self, caller: &Caller, req: pb::GetPlayerRequest) -> ApiResult<pb::GetPlayerResponse> {
        let access = self.admit(caller, Scope::ReadPublic)?;
        let id = parse_player_id(&req.player_id)?;
        let p = self
            .store
            .get_player(id, access.provider)
            .await?
            .ok_or_else(|| ApiError::new(Code::NotFound, "no such player"))?;
        Ok(pb::GetPlayerResponse { player: Some(player_pb(&p, &self.policy)) })
    }

    pub async fn check_display_name(
        &self,
        caller: &Caller,
        req: pb::CheckDisplayNameRequest,
    ) -> ApiResult<pb::CheckDisplayNameResponse> {
        self.admit(caller, Scope::ReadPublic)?;
        let player = match req.player_id.as_str() {
            "" => None,
            s => Some(parse_player_id(s)?),
        };
        let v: Verdict = self.store.check_display_name(&req.name, player, self.now(), &self.policy).await?;
        Ok(pb::CheckDisplayNameResponse {
            valid: v.valid,
            available: v.available,
            status: status_pb(v.status) as i32,
            next_change_at: v.next_change_at,
        })
    }

    pub async fn set_display_name(
        &self,
        caller: &Caller,
        req: pb::SetDisplayNameRequest,
    ) -> ApiResult<pb::SetDisplayNameResponse> {
        let key = req.idempotency_key.clone();
        let mut keyless = req.clone();
        keyless.idempotency_key.clear();
        let fail = |code, msg: &str| {
            (ApiError::new(code, msg), AuditInfo { outcome: Code::as_str(code), ..Default::default() })
        };
        self.write(caller, Scope::ProfileWrite, method::SET_DISPLAY_NAME, &key, true, &keyless, |access| async move {
            let id = Uuid::parse_str(&req.player_id).map_err(|_| fail(Code::InvalidArgument, "player_id must be a UUID"))?;
            let provider: Provider = access.provider.ok_or_else(|| fail(Code::PermissionDenied, "access has no provider"))?;
            // A provider may only rename players it has an identity for.
            let p = self.store.get_player(id, Some(provider)).await.map_err(|e| {
                let e = ApiError::from(e);
                let outcome = e.code.as_str();
                (e, AuditInfo { outcome, ..Default::default() })
            })?;
            match p {
                None => return Err(fail(Code::NotFound, "no such player")),
                Some(p) if p.external_ids.is_empty() => {
                    return Err((
                        ApiError::new(Code::PermissionDenied, "player has no identity at this provider"),
                        AuditInfo { player_id: Some(id), outcome: "permission_denied", ..Default::default() },
                    ));
                }
                Some(_) => {}
            }
            let out = self
                .store
                .set_display_name(id, &req.name, Some(provider), self.now(), &self.policy)
                .await
                .map_err(|e| {
                    let e = ApiError::from(e);
                    let outcome = e.code.as_str();
                    (e, AuditInfo { player_id: Some(id), outcome, ..Default::default() })
                })?
                .ok_or_else(|| fail(Code::NotFound, "no such player"))?;
            Ok((
                pb::SetDisplayNameResponse {
                    status: status_pb(out.verdict.status) as i32,
                    player: Some(player_pb(&out.player, &self.policy)),
                    next_change_at: out.verdict.next_change_at,
                },
                AuditInfo { player_id: Some(id), outcome: out.verdict.status.as_str(), ..Default::default() },
            ))
        })
        .await
    }

    // ---- AdminService -------------------------------------------------------

    pub async fn set_identity_suspended(
        &self,
        caller: &Caller,
        req: pb::SetIdentitySuspendedRequest,
    ) -> ApiResult<pb::SetIdentitySuspendedResponse> {
        let key = req.idempotency_key.clone();
        let mut keyless = req.clone();
        keyless.idempotency_key.clear();
        self.write(caller, Scope::Admin, method::SET_IDENTITY_SUSPENDED, &key, true, &keyless, |_access| async move {
            let bad = |code, msg: &str| (ApiError::new(code, msg), AuditInfo { outcome: Code::as_str(code), ..Default::default() });
            let provider = Provider::from_id(&req.provider).ok_or_else(|| bad(Code::InvalidArgument, "unknown provider"))?;
            match self.store.set_identity_suspended(provider, &req.external_id, req.suspended).await {
                Ok(Some((identity_id, player_id))) => Ok((
                    pb::SetIdentitySuspendedResponse {
                        identity_id: identity_id.to_string(),
                        player_id: player_id.to_string(),
                        suspended: req.suspended,
                    },
                    AuditInfo {
                        identity_id: Some(identity_id),
                        player_id: Some(player_id),
                        outcome: if req.suspended { "suspended" } else { "reinstated" },
                    },
                )),
                Ok(None) => Err(bad(Code::NotFound, "no such identity")),
                Err(e) => {
                    let e = ApiError::from(e);
                    let outcome = e.code.as_str();
                    Err((e, AuditInfo { outcome, ..Default::default() }))
                }
            }
        })
        .await
    }
}
