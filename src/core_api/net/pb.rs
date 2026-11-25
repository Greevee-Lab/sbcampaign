// SPDX-License-Identifier: AGPL-3.0-only
//! Generated steelcore.v1 types (see `build.rs` and `proto/steelcore/v1`).

#![allow(clippy::all, clippy::pedantic, missing_docs)]

include!(concat!(env!("OUT_DIR"), "/steelcore.v1.rs"));
include!(concat!(env!("OUT_DIR"), "/steelcore.v1.serde.rs"));

/// Encoded FileDescriptorSet of the contract.
pub const DESCRIPTOR: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/steelcore_v1_descriptor.bin"));
