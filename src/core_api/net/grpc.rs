// SPDX-License-Identifier: AGPL-3.0-only
//! gRPC binding: thin adapters from the tonic services to [`CoreApi`].

use super::{
    pb,
    service::{ApiError, Caller, Code, CoreApi},
};
use crate::core_api::profile::ProfileCore;
use std::sync::Arc;
use tonic::{Request, Response, Status};

/// Largest accepted request message.
pub const MAX_MESSAGE: usize = 64 * 1024;

pub struct Grpc<S: ProfileCore>(pub Arc<CoreApi<S>>);

impl<S: ProfileCore> Clone for Grpc<S> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

pub fn status(e: ApiError) -> Status {
    let code = match e.code {
        Code::InvalidArgument => tonic::Code::InvalidArgument,
        Code::Unauthenticated => tonic::Code::Unauthenticated,
        Code::PermissionDenied => tonic::Code::PermissionDenied,
        Code::NotFound => tonic::Code::NotFound,
        Code::AlreadyExists => tonic::Code::AlreadyExists,
        Code::ResourceExhausted => tonic::Code::ResourceExhausted,
        Code::FailedPrecondition => tonic::Code::FailedPrecondition,
        Code::Aborted => tonic::Code::Aborted,
        Code::Internal => tonic::Code::Internal,
        Code::Unavailable => tonic::Code::Unavailable,
    };
    Status::new(code, e.message)
}

fn caller<T>(req: &Request<T>) -> Caller {
    req.extensions().get::<Caller>().cloned().unwrap_or(Caller(None))
}

#[tonic::async_trait]
impl<S: ProfileCore> pb::identity_service_server::IdentityService for Grpc<S> {
    async fn resolve_identity(
        &self,
        req: Request<pb::ResolveIdentityRequest>,
    ) -> Result<Response<pb::ResolveIdentityResponse>, Status> {
        let c = caller(&req);
        self.0.resolve_identity(&c, req.into_inner()).await.map(Response::new).map_err(status)
    }
}

#[tonic::async_trait]
impl<S: ProfileCore> pb::player_service_server::PlayerService for Grpc<S> {
    async fn get_player(&self, req: Request<pb::GetPlayerRequest>) -> Result<Response<pb::GetPlayerResponse>, Status> {
        let c = caller(&req);
        self.0.get_player(&c, req.into_inner()).await.map(Response::new).map_err(status)
    }

    async fn check_display_name(
        &self,
        req: Request<pb::CheckDisplayNameRequest>,
    ) -> Result<Response<pb::CheckDisplayNameResponse>, Status> {
        let c = caller(&req);
        self.0.check_display_name(&c, req.into_inner()).await.map(Response::new).map_err(status)
    }

    async fn set_display_name(
        &self,
        req: Request<pb::SetDisplayNameRequest>,
    ) -> Result<Response<pb::SetDisplayNameResponse>, Status> {
        let c = caller(&req);
        self.0.set_display_name(&c, req.into_inner()).await.map(Response::new).map_err(status)
    }
}

#[tonic::async_trait]
impl<S: ProfileCore> pb::admin_service_server::AdminService for Grpc<S> {
    async fn set_identity_suspended(
        &self,
        req: Request<pb::SetIdentitySuspendedRequest>,
    ) -> Result<Response<pb::SetIdentitySuspendedResponse>, Status> {
        let c = caller(&req);
        self.0.set_identity_suspended(&c, req.into_inner()).await.map(Response::new).map_err(status)
    }
}

/// The gRPC services as one axum router.
pub fn router<S: ProfileCore>(api: Arc<CoreApi<S>>) -> axum::Router {
    let g = Grpc(api);
    tonic::service::Routes::new(
        pb::identity_service_server::IdentityServiceServer::new(g.clone()).max_decoding_message_size(MAX_MESSAGE),
    )
    .add_service(pb::player_service_server::PlayerServiceServer::new(g.clone()).max_decoding_message_size(MAX_MESSAGE))
    .add_service(pb::admin_service_server::AdminServiceServer::new(g).max_decoding_message_size(MAX_MESSAGE))
    .into_axum_router()
}
