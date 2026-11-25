// SPDX-License-Identifier: AGPL-3.0-only
//! Generates the steelcore.v1 Core API code from `proto/` (one contract, two
//! bindings): prost messages + tonic services (gRPC) and pbjson serde impls
//! (proto3 JSON, used by the Connect-JSON binding). `protox` parses the
//! .proto files in Rust, so no `protoc` is needed to build.

use prost::Message;
use std::{env, path::PathBuf};

const PROTOS: &[&str] = &[
    "proto/steelcore/v1/identity.proto",
    "proto/steelcore/v1/player.proto",
    "proto/steelcore/v1/admin.proto",
];

fn main() -> Result<(), Box<dyn std::error::Error>> {
    for p in PROTOS {
        println!("cargo:rerun-if-changed={p}");
    }
    let out = PathBuf::from(env::var("OUT_DIR")?);
    let fds = protox::compile(PROTOS, ["proto"])?;
    let descriptor_bytes = fds.encode_to_vec();
    std::fs::write(out.join("steelcore_v1_descriptor.bin"), &descriptor_bytes)?;

    tonic_prost_build::configure()
        .build_server(true)
        .build_client(true)
        .build_transport(false)
        .compile_fds(fds)?;

    pbjson_build::Builder::new()
        .register_descriptors(&descriptor_bytes)?
        .ignore_unknown_fields()
        .build(&[".steelcore.v1"])?;
    Ok(())
}
