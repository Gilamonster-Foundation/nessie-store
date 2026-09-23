//! The S3 object-API face over the nessie content-addressed store.
//!
//! A second protocol face beside `nessie-reapi`, over the **same** CAS: blobs a
//! Bazel client writes over REAPI gRPC are the blobs an S3 client reads, because
//! both name them by the same digest. That is not integration work — it is what
//! content addressing means.
//!
//! # What this face serves
//!
//! Object keys whose final segments are `{cas}/{xx}/{sha256-hex}` — the layout
//! used by cache clients that key blobs by their own digest, bazel-remote's S3
//! backend being the one this slice was built against. For those keys the S3 key
//! *is* the digest, so the face needs no bucket index, no name→digest table and no
//! configuration: [`ObjectKey::parse`] is pure and the CAS is the only state.
//!
//! Three of `s3s`'s 114 [`S3`](s3s::S3) trait methods are implemented —
//! `PutObject`, `GetObject`, `HeadObject`. The rest keep the trait's default
//! `NotImplemented`, which is the honest decline, not a gap to fill in later.
//!
//! # What it refuses, and why that is the point
//!
//! A write whose bytes do not hash to the digest in its key is **rejected**, not
//! stored. That makes two client configurations visible as errors rather than as
//! silent corruption:
//!
//! * framed/compressed objects (`cas.v2/…`, bazel-remote's default
//!   `--s3.storage_mode zstd`) — refused on write with a message naming the flag;
//! * keys addressed by something other than their content (`ac/…`, `raw/…`) —
//!   reported absent, so a client that also caches those keeps them elsewhere.
//!
//! Read paths report both shapes as a plain cache **miss**, so a misconfigured
//! client degrades to a cold cache instead of failing builds; the write path is
//! where the error is loud, because that is the log an operator reads.
//!
//! # Not yet here
//!
//! Object *listing* and mutable names. A general-purpose S3 client
//! (`aws s3 cp`, boto3) needs `ListObjectsV2` and arbitrary keys, which needs a
//! mutable name→digest layer over the CAS — the Merkle `Tree` type is the piece
//! that layer is built from. Digest-keyed cache clients need none of it, so it is
//! not in this slice.

#![forbid(unsafe_code)]

mod error;
mod key;
mod service;

pub use error::s3_error_from_backend;
pub use key::ObjectKey;
pub use service::NessieS3;

/// The transport-level error `S3Service` surfaces, re-exported so a daemon can name
/// it in an error handler without depending on `s3s` directly.
pub use s3s::HttpError;

use nessie_backend_core::CasBackend;
use s3s::auth::{SecretKey, SimpleAuth};
use s3s::service::{S3Service, S3ServiceBuilder};
use std::sync::Arc;

/// Build the S3 service over `cas`, authenticating a single access key.
///
/// The returned [`S3Service`] is a `tower::Service`, so the daemon mounts it the
/// way it mounts the REAPI tonic router — beside axum, not inside it.
///
/// SigV4 is `s3s`'s job; credentials are checked before any handler runs.
#[must_use]
pub fn build_service(
    cas: Arc<dyn CasBackend>,
    access_key: impl Into<String>,
    secret_key: impl Into<SecretKey>,
) -> S3Service {
    let mut builder = S3ServiceBuilder::new(NessieS3::new(cas));
    builder.set_auth(SimpleAuth::from_single(access_key, secret_key));
    builder.build()
}
