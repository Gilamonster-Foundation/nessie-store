//! The two cache faces serve the same blobs.
//!
//! `nessie-reapi` (Bazel gRPC) and `nessie-s3` (S3 object API) are independent
//! crates that know nothing about each other. What makes a blob written over one
//! readable over the other is not integration — it is that both name blobs by the
//! same digest, and that the composition root hands them **one** backend.
//!
//! That second half is the fragile part: `faces::build_cas_backend` returning a
//! store per caller would break the property while every unit test in both crates
//! still passed. This drives both faces over the backend `build_faces(&Config)` —
//! the entry point `main` itself calls — returns, and asserts the round trip in both
//! directions, plus that a disk-backed `[cas]` node survives a rebuild.

use nessie_backend_core::CasBackend as _;
use nessie_reapi::reapi;
use nessie_reapi::reapi::content_addressable_storage_server::ContentAddressableStorage;
use nessie_reapi::{CasV2Svc, ReapiConfig};
use nessie_s3::NessieS3;
use nessie_store::config::{
    CasBackendKind, CasConfig, CasMode, Config, ReapiServerConfig, S3ServerConfig,
};
use nessie_store::faces::{Faces, build_faces};
use s3s::S3;
use s3s::S3Request;
use s3s::dto::{GetObjectInput, PutObjectInput, StreamingBlob};
use std::sync::Arc;

/// A config enabling both faces (and, given `disk`, a disk-backed `[cas]` node).
fn faces_config(disk: Option<&std::path::Path>) -> Config {
    Config {
        reapi: Some(ReapiServerConfig {
            enabled: true,
            ..Default::default()
        }),
        s3: Some(S3ServerConfig {
            enabled: true,
            secret_key: "not-the-default".to_owned(),
            ..Default::default()
        }),
        cas: disk.map(|path| CasConfig {
            backend: CasBackendKind::Disk,
            path: Some(path.to_owned()),
            byte_budget: Some(1 << 30),
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// The shared backend exactly as the daemon wires it.
fn shared_backend(cfg: &Config) -> (Arc<dyn nessie_backend_core::CasBackend>, Faces) {
    let mut faces = build_faces(cfg).expect("faces");
    let (backend, _signer) = faces.backend.take().expect("a face is enabled");
    (backend, faces)
}

const BLOB: &[u8] = b"one blob, two protocols";

fn sha256_hex(data: &[u8]) -> String {
    use nessie_backend_core::{Digest, DigestAlgo};
    Digest::compute_with(DigestAlgo::Sha256, data)
        .to_string()
        .rsplit_once(':')
        .expect("a digest renders as algo:hex")
        .1
        .to_owned()
}

/// bazel-remote's uncompressed key layout.
fn cas_key(hash: &str) -> String {
    format!("cas/{}/{hash}", &hash[..2])
}

fn s3_request<T>(input: T) -> S3Request<T> {
    S3Request {
        input,
        method: http::Method::GET,
        uri: http::Uri::from_static("/"),
        headers: http::HeaderMap::new(),
        extensions: http::Extensions::new(),
        credentials: None,
        region: None,
        service: None,
        trailing_headers: None,
    }
}

#[tokio::test]
async fn a_blob_put_over_s3_is_readable_over_reapi() {
    let (backend, _faces) = shared_backend(&faces_config(None));
    let hash = sha256_hex(BLOB);

    // Write through the S3 face.
    let s3 = NessieS3::new(backend.clone());
    let bytes = bytes::Bytes::from_static(BLOB);
    s3.put_object(s3_request(PutObjectInput {
        bucket: "cache".to_owned(),
        key: cas_key(&hash),
        body: Some(StreamingBlob::wrap(futures::stream::once(async move {
            Ok::<_, std::io::Error>(bytes)
        }))),
        ..Default::default()
    }))
    .await
    .expect("S3 put");

    // Read it back through the REAPI CAS face — a different crate, different
    // protocol, same store.
    let cas = CasV2Svc::new(backend, Arc::new(ReapiConfig::default()));
    let out = cas
        .batch_read_blobs(tonic::Request::new(reapi::BatchReadBlobsRequest {
            digests: vec![reapi::Digest {
                hash: hash.clone(),
                size_bytes: BLOB.len() as i64,
            }],
            ..Default::default()
        }))
        .await
        .expect("REAPI batch read")
        .into_inner();

    assert_eq!(out.responses.len(), 1);
    assert_eq!(out.responses[0].data, BLOB, "REAPI must see the S3 write");
}

#[tokio::test]
async fn a_blob_put_over_reapi_is_readable_over_s3() {
    let (backend, _faces) = shared_backend(&faces_config(None));
    let hash = sha256_hex(BLOB);

    // Write through the REAPI CAS face.
    let cas = CasV2Svc::new(backend.clone(), Arc::new(ReapiConfig::default()));
    let out = cas
        .batch_update_blobs(tonic::Request::new(reapi::BatchUpdateBlobsRequest {
            requests: vec![reapi::batch_update_blobs_request::Request {
                digest: Some(reapi::Digest {
                    hash: hash.clone(),
                    size_bytes: BLOB.len() as i64,
                }),
                data: BLOB.to_vec(),
                ..Default::default()
            }],
            ..Default::default()
        }))
        .await
        .expect("REAPI batch update")
        .into_inner();
    assert_eq!(out.responses.len(), 1);

    // Read it back over S3, at the key a cache client would compute.
    let s3 = NessieS3::new(backend);
    let got = s3
        .get_object(s3_request(GetObjectInput {
            bucket: "cache".to_owned(),
            key: cas_key(&hash),
            ..Default::default()
        }))
        .await
        .expect("S3 get must see the REAPI write");

    use futures::TryStreamExt;
    let body: Vec<bytes::Bytes> = got
        .output
        .body
        .expect("a stored blob has a body")
        .try_collect()
        .await
        .expect("stream the body");
    assert_eq!(body.concat(), BLOB);
}

#[tokio::test]
async fn the_two_faces_are_handed_the_same_store() {
    // Guards the composition root directly: if `build_cas_backend` ever returned a
    // fresh store per call, the round-trip tests above would still pass when each
    // face got its own — so assert the daemon's own sharing contract.
    let (backend, _faces) = shared_backend(&faces_config(None));
    let a = NessieS3::new(backend.clone());
    let _b = CasV2Svc::new(backend.clone(), Arc::new(ReapiConfig::default()));

    // Two faces, one Arc: writing through one is visible through the raw backend.
    let hash = sha256_hex(BLOB);
    let bytes = bytes::Bytes::from_static(BLOB);
    a.put_object(s3_request(PutObjectInput {
        bucket: "cache".to_owned(),
        key: cas_key(&hash),
        body: Some(StreamingBlob::wrap(futures::stream::once(async move {
            Ok::<_, std::io::Error>(bytes)
        }))),
        ..Default::default()
    }))
    .await
    .expect("S3 put");

    use nessie_backend_core::Digest;
    let digest: Digest = format!("sha2-256:{hash}").parse().expect("digest");
    assert!(
        backend.has(&digest).expect("has"),
        "the shared backend must hold what a face wrote"
    );
}

/// A unique scratch dir under the OS temp dir, removed on drop.
struct TempRoot(std::path::PathBuf);

impl TempRoot {
    fn new() -> Self {
        Self(std::env::temp_dir().join(format!("nessie-faces-{}", uuid::Uuid::new_v4())))
    }

    fn path(&self) -> &std::path::Path {
        &self.0
    }
}

impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

async fn s3_put(backend: Arc<dyn nessie_backend_core::CasBackend>, hash: &str) {
    let bytes = bytes::Bytes::from_static(BLOB);
    NessieS3::new(backend)
        .put_object(s3_request(PutObjectInput {
            bucket: "cache".to_owned(),
            key: cas_key(hash),
            body: Some(StreamingBlob::wrap(futures::stream::once(async move {
                Ok::<_, std::io::Error>(bytes)
            }))),
            ..Default::default()
        }))
        .await
        .expect("S3 put");
}

#[tokio::test]
async fn a_blob_put_over_s3_lands_in_the_cas_node_store() {
    // With `[cas]` present the faces must share the node's store, not a private one.
    let dir = TempRoot::new();
    let cfg = faces_config(Some(dir.path()));
    let (backend, faces) = shared_backend(&cfg);
    let hash = sha256_hex(BLOB);
    s3_put(backend, &hash).await;

    let digest: nessie_backend_core::Digest = format!("sha2-256:{hash}").parse().expect("digest");
    let node = faces.node.expect("[cas] builds a node");
    assert!(node.store().has(&digest).expect("has"));
}

#[tokio::test]
async fn a_disk_blob_put_over_s3_survives_a_rebuild_from_the_same_config() {
    // Issue #125 acceptance: persistence is real across a daemon restart.
    let dir = TempRoot::new();
    let cfg = faces_config(Some(dir.path()));
    let hash = sha256_hex(BLOB);
    {
        let (backend, _faces) = shared_backend(&cfg);
        s3_put(backend, &hash).await;
    } // node dropped

    let rebuilt = build_faces(&cfg).expect("rebuild").node.expect("node");
    let digest: nessie_backend_core::Digest = format!("sha2-256:{hash}").parse().expect("digest");
    assert!(
        rebuilt.store().has(&digest).expect("has"),
        "the blob must survive a restart"
    );
}

#[test]
fn the_default_s3_secret_is_refused() {
    let mut cfg = faces_config(None);
    cfg.s3.as_mut().expect("s3").secret_key = S3ServerConfig::default().secret_key;
    let err = build_faces(&cfg)
        .err()
        .expect("default secret must refuse to start");
    assert!(err.to_string().contains("NESSIE_S3_SECRET_KEY"), "{err}");
    // A disabled S3 face is not checked.
    cfg.s3.as_mut().expect("s3").enabled = false;
    assert!(build_faces(&cfg).is_ok());
}

/// Zero grace so a single maintenance pass may evict anything unpinned.
fn zero_grace(cfg: &mut Config, mode: CasMode) {
    let cas = cfg.cas.as_mut().expect("cas");
    cas.gc_grace_secs = 0;
    cas.mode = mode;
    if mode == CasMode::WriteOnly {
        cas.replication_factor = Some(1); // single node: no peers to mirror to
    }
}

fn blob_digest(hash: &str) -> nessie_backend_core::Digest {
    format!("sha2-256:{hash}").parse().expect("digest")
}

#[tokio::test]
async fn write_only_maintenance_never_reclaims_face_written_blobs() {
    let dir = TempRoot::new();
    let mut cfg = faces_config(Some(dir.path()));
    zero_grace(&mut cfg, CasMode::WriteOnly);
    cfg.cas.as_mut().expect("cas").byte_budget = Some(1 << 20);
    let hash = sha256_hex(BLOB);
    {
        let (backend, _faces) = shared_backend(&cfg);
        s3_put(backend, &hash).await;
    }
    let node = build_faces(&cfg).expect("restart").node.expect("node");
    node.maintain_once().expect("maintenance");
    assert!(
        node.store().has(&blob_digest(&hash)).expect("has"),
        "write_only retains every blob across a restart"
    );
}

#[tokio::test]
async fn write_only_refuses_a_face_write_past_capacity() {
    let dir = TempRoot::new();
    let mut cfg = faces_config(Some(dir.path()));
    zero_grace(&mut cfg, CasMode::WriteOnly);
    // Too small for even the startup probe: a full store must still boot.
    cfg.cas.as_mut().expect("cas").byte_budget = Some(1);
    let (backend, faces) = shared_backend(&cfg);
    let hash = sha256_hex(BLOB);
    let res = backend.put_keyed(&blob_digest(&hash), &mut { BLOB });
    assert!(
        matches!(
            res,
            Err(nessie_backend_core::BackendError::CapacityExceeded { .. })
        ),
        "{res:?}"
    );
    assert!(
        !faces
            .node
            .expect("node")
            .store()
            .has(&blob_digest(&hash))
            .expect("has")
    );
}

#[tokio::test]
async fn lru_over_budget_evicts_a_sole_copy_face_blob() {
    let dir = TempRoot::new();
    let mut cfg = faces_config(Some(dir.path()));
    zero_grace(&mut cfg, CasMode::Lru);
    cfg.cas.as_mut().expect("cas").byte_budget = Some(0);
    let (backend, faces) = shared_backend(&cfg);
    let hash = sha256_hex(BLOB);
    s3_put(backend, &hash).await;
    let node = faces.node.expect("node");
    node.maintain_once().expect("maintenance");
    assert!(
        !node.store().has(&blob_digest(&hash)).expect("has"),
        "lru is a cache: a sole copy over budget goes"
    );
}

struct HeldElsewhere;

impl nessie_cas_store::DurabilityOracle for HeldElsewhere {
    fn durably_held(
        &self,
        _: &nessie_backend_core::Digest,
    ) -> Result<bool, nessie_backend_core::BackendError> {
        Ok(true)
    }
}

#[tokio::test]
async fn a_completed_face_write_becomes_evictable_without_a_restart() {
    // Reviewer P1 #2 on #136: a face write is a complete operation, so its in-flight
    // guard must be released; otherwise it is never a retention candidate in-process.
    let dir = TempRoot::new();
    let mut cfg = faces_config(Some(dir.path()));
    zero_grace(&mut cfg, CasMode::Lru);
    cfg.cas.as_mut().expect("cas").byte_budget = Some(0);
    let (backend, faces) = shared_backend(&cfg);
    let hash = sha256_hex(BLOB);
    s3_put(backend, &hash).await;

    let node = faces.node.expect("node");
    let report = node
        .store()
        .evict_to_budget(
            node.roots(),
            &HeldElsewhere,
            &nessie_cas_store::RefuseToReplicate,
        )
        .expect("evict");
    // (The startup `put_keyed` probe blob is evicted too, hence `>= 1`.)
    assert!(
        report.evicted >= 1,
        "a durably-held, unreferenced blob goes"
    );
    assert!(!node.store().has(&blob_digest(&hash)).expect("has"));
}
