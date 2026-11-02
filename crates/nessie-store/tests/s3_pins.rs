//! #137: S3 writes can become pins. `[s3] pin_on_put` pins every PutObject; without it a
//! write pins only with `x-amz-meta-nessie-pin: true`. DeleteObject unpins and never
//! deletes bytes. Driven by the pinned `object_store` client against the daemon's
//! own `s3_app` over a disk-backed `lru` node.

use std::path::Path as FsPath;
use std::sync::Arc;

use nessie_backend_core::{CasBackend as _, Digest, DigestAlgo};
use nessie_store::cas_node::CasNode;
use nessie_store::config::{CasBackendKind, CasConfig, CasMode, Config, S3ServerConfig};
use object_store::aws::{AmazonS3, AmazonS3Builder};
use object_store::path::Path;
use object_store::{
    Attribute, Attributes, ObjectStore as _, ObjectStoreExt, PutOptions, PutPayload, RetryConfig,
};

const BLOB: &[u8] = b"kept by request";

/// S3 on, over a disk `lru` node with no budget and no grace (a pass evicts anything
/// unpinned).
fn config(dir: &FsPath, pin_on_put: bool) -> Config {
    Config {
        s3: Some(S3ServerConfig {
            enabled: true,
            secret_key: "not-the-default".to_owned(),
            pin_on_put,
            ..Default::default()
        }),
        cas: Some(CasConfig {
            mode: CasMode::Lru,
            backend: CasBackendKind::Disk,
            path: Some(dir.to_owned()),
            byte_budget: Some(0),
            gc_grace_secs: 0,
            ..Default::default()
        }),
        ..Default::default()
    }
}

struct Face {
    s3: AmazonS3,
    node: Arc<CasNode>,
    server: tokio::task::JoinHandle<()>,
}

/// Build the faces exactly as the daemon does and serve S3 on loopback.
async fn serve(cfg: &Config) -> Face {
    let faces = nessie_store::faces::build_faces(cfg).unwrap();
    let node = faces.node.clone().unwrap();
    let (backend, _signer) = faces.backend.unwrap();
    let sc = cfg.s3.clone().unwrap();
    let app = nessie_store::faces::s3_app(&sc, backend, Some(node.clone()));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    let s3 = AmazonS3Builder::new()
        .with_endpoint(format!("http://127.0.0.1:{port}"))
        .with_allow_http(true)
        .with_region("us-east-1")
        .with_bucket_name("cache")
        .with_access_key_id(&sc.access_key)
        .with_secret_access_key(&sc.secret_key)
        // `delete` as single DeleteObject (`DELETE /key`); the face declines bulk
        // DeleteObjects.
        .with_disable_bulk_delete(true)
        .with_retry(RetryConfig {
            max_retries: 0,
            ..Default::default()
        })
        .build()
        .unwrap();
    Face { s3, node, server }
}

impl Face {
    /// Stop serving and drop the node, as a daemon restart would.
    async fn stop(self) {
        self.server.abort();
        let _ = self.server.await;
    }
}

fn digest() -> Digest {
    Digest::compute_with(DigestAlgo::Sha256, BLOB)
}

fn key() -> Path {
    let d = digest().to_string();
    let hash = d.rsplit_once(':').unwrap().1;
    Path::from(format!("cas/{}/{hash}", &hash[..2]))
}

async fn put(s3: &AmazonS3, pin_meta: Option<&'static str>) {
    let mut attributes = Attributes::new();
    if let Some(v) = pin_meta {
        attributes.insert(Attribute::Metadata("nessie-pin".into()), v.into());
    }
    let opts = PutOptions {
        attributes,
        ..Default::default()
    };
    s3.put_opts(&key(), PutPayload::from_static(BLOB), opts)
        .await
        .expect("put");
}

/// Reopen the node from disk (a restart) and run one eviction pass.
fn reopen_and_evict(cfg: &Config) -> Arc<CasNode> {
    let node = nessie_store::faces::build_faces(cfg).unwrap().node.unwrap();
    node.maintain_once().unwrap();
    node
}

#[tokio::test(flavor = "multi_thread")]
async fn a_plain_put_is_not_pinned_by_default() {
    let dir = tempfile::tempdir().unwrap();
    let face = serve(&config(dir.path(), false)).await;
    put(&face.s3, None).await;
    put(&face.s3, Some("yes")).await; // only "true" opts in
    assert!(face.node.roots().pins().is_empty());
    face.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn the_pin_header_pins_and_the_pin_survives_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = config(dir.path(), false);
    let face = serve(&cfg).await;
    put(&face.s3, Some("true")).await;
    assert!(face.node.roots().pins().contains(&digest()));
    face.stop().await;

    let node = reopen_and_evict(&cfg);
    assert!(node.roots().pins().contains(&digest()), "reloaded");
    assert!(node.has(&digest()).unwrap(), "a pinned blob survives lru");
}

#[tokio::test(flavor = "multi_thread")]
async fn pin_on_put_pins_without_the_header() {
    let dir = tempfile::tempdir().unwrap();
    let face = serve(&config(dir.path(), true)).await;
    put(&face.s3, None).await;
    assert!(face.node.roots().pins().contains(&digest()));
    face.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn delete_object_unpins_and_lru_reclaims_after_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = config(dir.path(), true);
    let face = serve(&cfg).await;
    put(&face.s3, None).await;
    face.s3.delete(&key()).await.expect("delete is a 204");
    face.s3
        .delete(&key())
        .await
        .expect("deleting an unpinned key is still a 204");
    assert!(face.node.roots().pins().is_empty(), "unpinned");
    assert!(
        face.node.has(&digest()).unwrap(),
        "delete never removes bytes"
    );
    face.stop().await;

    let node = reopen_and_evict(&cfg);
    assert!(
        !node.has(&digest()).unwrap(),
        "unpinned, so lru reclaimed it"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_pin_that_cannot_be_stored_fails_the_put() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = config(dir.path(), true);
    let cas = cfg.cas.as_mut().unwrap();
    cas.mode = CasMode::WriteOnly;
    cas.replication_factor = Some(1);
    cas.byte_budget = Some(BLOB.len() as u64); // the blob fits; its root tree does not
    let face = serve(&cfg).await;
    let err = face
        .s3
        .put(&key(), PutPayload::from_static(BLOB))
        .await
        .expect_err("no 200 for a pin that was not persisted");
    assert!(format!("{err:?}").contains("507"), "{err:?}");
    assert!(face.node.roots().pins().is_empty());
    face.stop().await;
}
