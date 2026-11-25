// SPDX-License-Identifier: AGPL-3.0-only
//! Connect-JSON binding (unary calls of the Connect protocol): the same
//! contract as gRPC, as plain HTTP so a client needs only an HTTPS library.
//!
//! * Request: `POST /steelcore.v1.<Service>/<Method>`,
//!   `content-type: application/json`, body = request message in the proto3
//!   JSON mapping (lowerCamelCase field names, 64-bit integers as strings,
//!   enums by name). Unknown fields are ignored, as in protobuf binary.
//! * Success: HTTP 200, `application/json`, the response message.
//! * Error: HTTP status per the Connect protocol and a body
//!   `{"code": "<grpc code in snake_case>", "message": "..."}`.

use super::{
    grpc::MAX_MESSAGE,
    pb,
    service::{ApiError, ApiResult, Caller, Code, CoreApi, method},
};
use crate::core_api::profile::ProfileCore;
use axum::{
    Router,
    body::Bytes,
    extract::{DefaultBodyLimit, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
    routing::post,
};
use serde::{Serialize, de::DeserializeOwned};
use std::{future::Future, sync::Arc};

pub fn http_status(code: Code) -> StatusCode {
    match code {
        Code::InvalidArgument | Code::FailedPrecondition => StatusCode::BAD_REQUEST,
        Code::Unauthenticated => StatusCode::UNAUTHORIZED,
        Code::PermissionDenied => StatusCode::FORBIDDEN,
        Code::NotFound => StatusCode::NOT_FOUND,
        Code::AlreadyExists | Code::Aborted => StatusCode::CONFLICT,
        Code::ResourceExhausted => StatusCode::TOO_MANY_REQUESTS,
        Code::Internal => StatusCode::INTERNAL_SERVER_ERROR,
        Code::Unavailable => StatusCode::SERVICE_UNAVAILABLE,
    }
}

fn error_response(status: StatusCode, code: &str, message: &str) -> Response {
    let body = serde_json::json!({ "code": code, "message": message }).to_string();
    (status, [(header::CONTENT_TYPE, "application/json")], body).into_response()
}

fn api_error(e: ApiError) -> Response {
    error_response(http_status(e.code), e.code.as_str(), &e.message)
}

async fn unary<Req, Resp, F, Fut>(caller: Option<axum::Extension<Caller>>, headers: HeaderMap, body: Bytes, f: F) -> Response
where
    Req: DeserializeOwned,
    Resp: Serialize,
    F: FnOnce(Caller, Req) -> Fut,
    Fut: Future<Output = ApiResult<Resp>>,
{
    let caller = caller.map(|c| c.0).unwrap_or(Caller(None));
    let json = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.split(';').next().unwrap_or("").trim().eq_ignore_ascii_case("application/json"))
        .unwrap_or(false);
    if !json {
        return error_response(StatusCode::UNSUPPORTED_MEDIA_TYPE, "invalid_argument", "content-type must be application/json");
    }
    let req: Req = match serde_json::from_slice(if body.is_empty() { b"{}" } else { &body }) {
        Ok(r) => r,
        Err(_) => return error_response(StatusCode::BAD_REQUEST, "invalid_argument", "malformed request message"),
    };
    match f(caller, req).await {
        Ok(resp) => match serde_json::to_vec(&resp) {
            Ok(body) => (StatusCode::OK, [(header::CONTENT_TYPE, "application/json")], body).into_response(),
            Err(_) => api_error(ApiError::new(Code::Internal, "response encoding")),
        },
        Err(e) => api_error(e),
    }
}

macro_rules! route {
    ($router:expr, $path:expr, $method:ident, $req:ty) => {
        $router.route(
            &format!("/{}", $path),
            post(
                |State(api): State<Arc<CoreApi<S>>>, caller: Option<axum::Extension<Caller>>, headers: HeaderMap, body: Bytes| async move {
                    unary(caller, headers, body, |c, r: $req| async move { api.$method(&c, r).await }).await
                },
            ),
        )
    };
}

/// The Connect-JSON routes.
pub fn router<S: ProfileCore>(api: Arc<CoreApi<S>>) -> Router {
    let r: Router<Arc<CoreApi<S>>> = Router::new();
    let r = route!(r, method::RESOLVE_IDENTITY, resolve_identity, pb::ResolveIdentityRequest);
    let r = route!(r, method::GET_PLAYER, get_player, pb::GetPlayerRequest);
    let r = route!(r, method::CHECK_DISPLAY_NAME, check_display_name, pb::CheckDisplayNameRequest);
    let r = route!(r, method::SET_DISPLAY_NAME, set_display_name, pb::SetDisplayNameRequest);
    let r = route!(r, method::SET_IDENTITY_SUSPENDED, set_identity_suspended, pb::SetIdentitySuspendedRequest);
    r.fallback(|| async { error_response(StatusCode::NOT_FOUND, "unimplemented", "no such method") })
        .layer(DefaultBodyLimit::max(MAX_MESSAGE))
        .with_state(api)
}
