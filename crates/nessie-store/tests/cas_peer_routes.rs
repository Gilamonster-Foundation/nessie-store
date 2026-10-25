//! The `/cas/v1/blobs/{digest}` peer wire, over a real loopback socket.

use std::sync::Arc;

use nessie_backend_core::{CasBackend as _, Digest, PeerId};
use nessie_store::cas_node::CasNode;
use nessie_store::cas_routes::peer_routes;
use nessie_store::config::CasConfig;

async fn serve() -> (String, Arc<CasNode>) {
    let cfg = CasConfig {
        byte_budget: Some(1 << 20),
        ..CasConfig::default()
    };
    let node = Arc::new(CasNode::from_config(&cfg, PeerId::new("n")).unwrap());
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/cas/v1/blobs", l.local_addr().unwrap());
    let app = peer_routes(node.clone());
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    (url, node)
}

#[tokio::test]
async fn put_get_head_roundtrip_by_digest() {
    let (base, node) = serve().await;
    let c = reqwest::Client::new();
    let d = Digest::compute(b"over the wire");
    let url = format!("{base}/{d}");

    assert_eq!(c.head(&url).send().await.unwrap().status(), 404);
    assert_eq!(c.get(&url).send().await.unwrap().status(), 404);
    assert_eq!(
        c.put(&url)
            .body("over the wire")
            .send()
            .await
            .unwrap()
            .status(),
        201
    );
    assert_eq!(c.head(&url).send().await.unwrap().status(), 200);
    assert_eq!(
        c.get(&url).send().await.unwrap().bytes().await.unwrap(),
        "over the wire"
    );
    assert!(node.has(&d).unwrap(), "served from the node's own store");
}

#[tokio::test]
async fn tampered_put_is_rejected_and_stores_nothing() {
    let (base, node) = serve().await;
    let d = Digest::compute(b"honest");
    let r = reqwest::Client::new()
        .put(format!("{base}/{d}"))
        .body("forged")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 400);
    assert!(!node.has(&d).unwrap());
}

#[tokio::test]
async fn malformed_digest_is_400_and_encoded_colon_works() {
    let (base, _) = serve().await;
    let c = reqwest::Client::new();
    assert_eq!(
        c.get(format!("{base}/not-a-digest"))
            .send()
            .await
            .unwrap()
            .status(),
        400
    );
    let d = Digest::compute(b"x");
    c.put(format!("{base}/{d}")).body("x").send().await.unwrap();
    let encoded = d.to_string().replace(':', "%3A");
    assert_eq!(
        c.head(format!("{base}/{encoded}"))
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
}

#[tokio::test]
async fn head_and_get_report_the_serving_nodes_mode() {
    use nessie_store::config::CasMode;
    for (mode, expect) in [(CasMode::Lru, "lru"), (CasMode::WriteOnly, "write_only")] {
        let cfg = CasConfig {
            mode,
            byte_budget: Some(1 << 20),
            ..CasConfig::default()
        };
        let node = Arc::new(CasNode::from_config(&cfg, PeerId::new("n")).unwrap());
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}/cas/v1/blobs", l.local_addr().unwrap());
        let app = peer_routes(node.clone());
        tokio::spawn(async move { axum::serve(l, app).await.unwrap() });

        let d = Digest::compute(b"mode probe");
        let c = reqwest::Client::new();
        c.put(format!("{base}/{d}"))
            .body("mode probe")
            .send()
            .await
            .unwrap();
        for r in [
            c.head(format!("{base}/{d}")).send().await.unwrap(),
            c.get(format!("{base}/{d}")).send().await.unwrap(),
        ] {
            assert_eq!(r.headers()["x-nessie-mode"], expect);
        }
    }
}

/// An lru node with no budget and no grace: one maintenance pass evicts anything unpinned.
async fn serve_lru_over_budget() -> (String, Arc<CasNode>) {
    use nessie_store::config::CasMode;
    let cfg = CasConfig {
        mode: CasMode::Lru,
        byte_budget: Some(0),
        gc_grace_secs: 0,
        ..CasConfig::default()
    };
    let node = Arc::new(CasNode::from_config(&cfg, PeerId::new("n")).unwrap());
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}/cas/v1", l.local_addr().unwrap());
    let app = peer_routes(node.clone());
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    (base, node)
}

#[tokio::test]
async fn pinned_blob_survives_eviction_until_unpinned() {
    let (base, node) = serve_lru_over_budget().await;
    let c = reqwest::Client::new();
    let d = Digest::compute(b"offload manifest");
    node.put_keyed(&d, &mut &b"offload manifest"[..]).unwrap();

    assert_eq!(
        c.post(format!("{base}/roots/{d}"))
            .send()
            .await
            .unwrap()
            .status(),
        201
    );
    node.maintain_once().unwrap();
    assert!(node.has(&d).unwrap(), "a pinned blob is never evicted");

    // Unpin is idempotent.
    for _ in 0..2 {
        assert_eq!(
            c.delete(format!("{base}/roots/{d}"))
                .send()
                .await
                .unwrap()
                .status(),
            204
        );
    }
    node.maintain_once().unwrap();
    assert!(!node.has(&d).unwrap(), "evicted on the pass after unpin");
}

#[tokio::test]
async fn pinning_an_unheld_digest_is_accepted_and_malformed_is_400() {
    let (base, node) = serve_lru_over_budget().await;
    let c = reqwest::Client::new();
    let d = Digest::compute(b"arrives later");
    assert_eq!(
        c.post(format!("{base}/roots/{d}"))
            .send()
            .await
            .unwrap()
            .status(),
        201
    );
    node.put_keyed(&d, &mut &b"arrives later"[..]).unwrap();
    node.maintain_once().unwrap();
    assert!(
        node.has(&d).unwrap(),
        "the early pin protects the late blob"
    );

    for r in [
        c.post(format!("{base}/roots/nope")).send().await.unwrap(),
        c.delete(format!("{base}/roots/nope")).send().await.unwrap(),
    ] {
        assert_eq!(r.status(), 400);
    }
}
