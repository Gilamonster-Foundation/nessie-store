//! The prost/tonic-generated REAPI v2 proto module tree (cache subset).
//!
//! The generated code cross-references packages by relative `super::` paths, so the
//! modules must mirror the proto package hierarchy exactly; the nested tree below
//! does that. [`reapi`], [`bytestream`], and [`rpc`] are the ergonomic re-exports
//! the rest of the crate uses.

/// The `build.bazel.*` proto packages.
#[allow(missing_docs, clippy::all, clippy::pedantic, rustdoc::all)]
pub mod build {
    /// `build.bazel.*`
    pub mod bazel {
        /// `build.bazel.semver`
        pub mod semver {
            tonic::include_proto!("build.bazel.semver");
        }
        /// `build.bazel.remote.*`
        pub mod remote {
            /// `build.bazel.remote.execution.*`
            pub mod execution {
                /// `build.bazel.remote.execution.v2` — the REAPI wire types + services.
                pub mod v2 {
                    tonic::include_proto!("build.bazel.remote.execution.v2");
                }
            }
        }
    }
}

/// The `google.*` proto packages the REAPI types depend on.
#[allow(missing_docs, clippy::all, clippy::pedantic, rustdoc::all)]
pub mod google {
    /// `google.bytestream` — the ByteStream large-blob transfer service.
    pub mod bytestream {
        tonic::include_proto!("google.bytestream");
    }
    /// `google.rpc` — `Status`, used in batch responses.
    pub mod rpc {
        tonic::include_proto!("google.rpc");
    }
}

/// The REAPI v2 wire types + service traits (`build.bazel.remote.execution.v2`).
pub use build::bazel::remote::execution::v2 as reapi;
/// The ByteStream service (`google.bytestream`).
pub use google::bytestream;
/// `google.rpc` (`Status`).
pub use google::rpc;

#[cfg(test)]
mod tests {
    #[test]
    fn generated_reapi_digest_constructs() {
        // The codegen produced a usable REAPI Digest (hash + size_bytes).
        let d = super::reapi::Digest {
            hash: "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855".to_string(),
            size_bytes: 0,
        };
        assert_eq!(d.size_bytes, 0);
        assert_eq!(d.hash.len(), 64);
    }

    #[test]
    fn generated_capabilities_types_exist() {
        // Sanity: the Capabilities request type and the digest-function enum compiled.
        let _ = super::reapi::GetCapabilitiesRequest {
            instance_name: String::new(),
        };
        assert_eq!(super::reapi::digest_function::Value::Sha256 as i32, 1);
    }
}
