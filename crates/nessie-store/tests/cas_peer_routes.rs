//! The `/cas/v1/blobs/{digest}` peer wire, over a real loopback socket.

use std::sync::Arc;

use nessie_backend_core::{CasBackend as _, Digest, PeerId};
use nessie_store::cas_node::CasNode;
use nessie_store::cas_routes::peer_routes;
use nessie_store::config::CasConfig;

async fn serve() -> (String, Arc<CasNode>) {
    let node = Arc::new(CasNode::from_config(&CasConfig::default(), PeerId::new("n")).unwrap());
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
