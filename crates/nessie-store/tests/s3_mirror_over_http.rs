//! The mirror guarantee seen from a real S3 client (#139 review, closed by #140):
//! with the peer down a PUT over HTTP is a 503, and once it returns a repair pass
//! brings the blob to the peer. The face is the daemon's own `s3_app`, bound on
//! loopback, driven by the pinned `object_store` client.

mod common;

use common::{dead_url, start_at, wo};
use nessie_backend_core::{CasBackend as _, Digest, DigestAlgo};
use nessie_store::config::{Config, S3ServerConfig};
use object_store::aws::AmazonS3Builder;
use object_store::path::Path;
use object_store::{ObjectStoreExt, PutPayload, RetryConfig};

const BLOB: &[u8] = b"mirrored, then repaired, over real S3";

#[tokio::test(flavor = "multi_thread")]
async fn a_503_with_the_peer_down_and_the_blob_reaches_the_peer_after_a_repair_pass() {
    let peer_url = dead_url();
    let s3_cfg = S3ServerConfig {
        enabled: true,
        secret_key: "not-the-default".to_owned(),
        ..Default::default()
    };
    let cfg = Config {
        s3: Some(s3_cfg.clone()),
        cas: Some(wo(2, &[&peer_url])),
        ..Default::default()
    };
    let faces = nessie_store::faces::build_faces(&cfg).unwrap();
    let node = faces.node.clone().unwrap();
    let (backend, _signer) = faces.backend.unwrap();

    let app = nessie_store::faces::s3_app(&s3_cfg, backend);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move { axum::serve(listener, app).await });
    let s3 = AmazonS3Builder::new()
        .with_endpoint(format!("http://127.0.0.1:{port}"))
        .with_allow_http(true)
        .with_region("us-east-1")
        .with_bucket_name("cache")
        .with_access_key_id(&s3_cfg.access_key)
        .with_secret_access_key(&s3_cfg.secret_key)
        // A 503 is retried by default; we want to see it.
        .with_retry(RetryConfig {
            max_retries: 0,
            ..Default::default()
        })
        .build()
        .unwrap();

    let digest = Digest::compute_with(DigestAlgo::Sha256, BLOB);
    let hash = digest.to_string().rsplit_once(':').unwrap().1.to_owned();
    let key = Path::from(format!("cas/{}/{hash}", &hash[..2]));

    let err = s3
        .put(&key, PutPayload::from_static(BLOB))
        .await
        .expect_err("peer down: the write is not acknowledged");
    assert!(format!("{err:?}").contains("503"), "{err:?}");
    assert!(node.has(&digest).unwrap(), "the local copy stays");

    let peer = start_at(&peer_url, &wo(1, &[]));
    assert!(!peer.node.has(&digest).unwrap());
    let n = node.clone();
    let report = tokio::task::spawn_blocking(move || n.maintain_once())
        .await
        .unwrap()
        .unwrap();
    // (The face's startup probe blob is held here too, so it is repaired alongside.)
    assert_eq!(report.scrub().under_replicated, 0);
    assert!(report.scrub().replicated >= 1);

    assert!(peer.node.has(&digest).unwrap(), "the peer holds it now");
    let got = s3.get(&key).await.unwrap().bytes().await.unwrap();
    assert_eq!(&got[..], BLOB);
}
