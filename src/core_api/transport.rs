// SPDX-License-Identifier: AGPL-3.0-only
//! Byte stream a campaign client is served over.
//!
//! The upstream listener serves plain TCP. Boxing the stream lets the same
//! protocol code run over other transports, for example the TLS stream of the
//! Steelbound gateway or an in-memory pipe in tests.

use tokio::io::{AsyncRead, AsyncWrite};

/// Any bidirectional byte stream that can be moved to another task.
pub trait Transport: AsyncRead + AsyncWrite + Unpin + Send {}

impl<T: AsyncRead + AsyncWrite + Unpin + Send> Transport for T {}

/// The stream type stored in [`crate::client::Client`].
pub type BoxedTransport = Box<dyn Transport>;
