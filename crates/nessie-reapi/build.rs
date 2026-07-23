//! Codegen for the vendored (trimmed) REAPI v2 proto tree.
//!
//! The vendored `proto/` tree is the cache subset: the Execution service and its
//! `google.longrunning.Operation` return type are trimmed out (see
//! `proto/build/bazel/remote/execution/v2/remote_execution.proto`), so the only
//! non-well-known imports are `build/bazel/semver`, `google/rpc/status`,
//! `google/bytestream`, and the `google/api` HTTP-annotation extensions. The
//! `google/protobuf/*` well-known types map to `prost_types`.

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let protos = [
        "proto/build/bazel/remote/execution/v2/remote_execution.proto",
        "proto/google/bytestream/bytestream.proto",
    ];
    for p in &protos {
        println!("cargo:rerun-if-changed={p}");
    }
    tonic_build::configure()
        .build_server(true)
        .build_client(true) // the in-process conformance tests drive a client
        .compile_protos(&protos, &["proto"])?;
    Ok(())
}
