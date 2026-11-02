//! `ListObjectsV2` driven by a real S3 client (`object_store::aws::AmazonS3`).
//!
//! Issue #129 item 3: the listing walks the local CAS, shows sha2-256 blobs only,
//! emits the canonical `cas/{xx}/{hex}` key in lexicographic order, and paginates.

use futures::TryStreamExt;
use nessie_backend_core::{CasBackend, Digest, DigestAlgo};
use nessie_backend_mem::MemCas;
use object_store::ObjectStore;
use object_store::aws::AmazonS3Builder;
use object_store::path::Path;
use std::io::Cursor;
use std::sync::Arc;

fn sha256_hex(data: &[u8]) -> String {
    Digest::compute_with(DigestAlgo::Sha256, data)
        .to_string()
        .rsplit_once(':')
        .expect("a digest renders as algo:hex")
        .1
        .to_owned()
}

fn canonical(data: &[u8]) -> String {
    let h = sha256_hex(data);
    format!("cas/{}/{h}", &h[..2])
}

/// Serve the S3 face over `cas` on a loopback port; return a client pointed at it.
async fn client(cas: Arc<MemCas>) -> impl ObjectStore {
    let service =
        nessie_s3::build_service(nessie_s3::NessieS3::new(cas), "AKIDTEST", "secret-test");
    let app = axum::Router::new().fallback_service(
        axum::error_handling::HandleError::<_, _, ()>::new(service, |_e: s3s::HttpError| async {
            axum::http::StatusCode::INTERNAL_SERVER_ERROR
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move { axum::serve(listener, app).await });
    AmazonS3Builder::new()
        .with_endpoint(format!("http://127.0.0.1:{port}"))
        .with_allow_http(true)
        .with_region("us-east-1")
        .with_bucket_name("cache")
        .with_access_key_id("AKIDTEST")
        .with_secret_access_key("secret-test")
        .build()
        .expect("build the S3 client")
}

fn seed(cas: &MemCas, n: usize) -> Vec<(String, u64)> {
    let mut want: Vec<(String, u64)> = (0..n)
        .map(|i| {
            let data = format!("blob-{i}").into_bytes();
            cas.put(&mut Cursor::new(data.clone())).unwrap();
            // MemCas::put hashes with the native default (blake3); store the sha256
            // twin explicitly so the listing has something to show.
            let d: Digest = format!("sha2-256:{}", sha256_hex(&data)).parse().unwrap();
            cas.put_keyed(&d, &mut Cursor::new(data.clone())).unwrap();
            (canonical(&data), data.len() as u64)
        })
        .collect();
    want.sort();
    want
}

#[tokio::test]
async fn lists_sha256_blobs_sorted_with_sizes_and_hides_blake3() {
    let cas = Arc::new(MemCas::new());
    let want = seed(&cas, 5);
    let s3 = client(cas).await;

    let got: Vec<_> = s3.list(None).try_collect().await.expect("LIST");
    let got: Vec<(String, u64)> = got
        .into_iter()
        .map(|m| (m.location.to_string(), m.size))
        .collect();
    // seed() also stored 5 blake3 blobs; none may appear.
    assert_eq!(got, want);
}

#[tokio::test]
async fn paginates_across_more_than_one_page() {
    let cas = Arc::new(MemCas::new());
    let want = seed(&cas, 1005); // client default page is 1000 keys
    let s3 = client(cas).await;

    let got: Vec<String> = s3
        .list(None)
        .map_ok(|m| m.location.to_string())
        .try_collect()
        .await
        .expect("LIST across pages");
    assert_eq!(got, want.into_iter().map(|w| w.0).collect::<Vec<_>>());
}

#[tokio::test]
async fn prefix_and_offset_filter() {
    let cas = Arc::new(MemCas::new());
    let want = seed(&cas, 40);
    let s3 = client(cas).await;
    let shard = want[0].0.split('/').nth(1).unwrap().to_owned();

    let under: Vec<String> = s3
        .list(Some(&Path::from(format!("cas/{shard}"))))
        .map_ok(|m| m.location.to_string())
        .try_collect()
        .await
        .unwrap();
    let expect: Vec<String> = want
        .iter()
        .map(|w| w.0.clone())
        .filter(|k| k.starts_with(&format!("cas/{shard}/")))
        .collect();
    assert_eq!(under, expect);

    let offset = Path::from(want[9].0.as_str());
    let after: Vec<String> = s3
        .list_with_offset(None, &offset)
        .map_ok(|m| m.location.to_string())
        .try_collect()
        .await
        .unwrap();
    assert_eq!(
        after,
        want[10..].iter().map(|w| w.0.clone()).collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn delimiter_is_declined_not_silently_ignored() {
    let s3 = client(Arc::new(MemCas::new())).await;
    s3.list_with_delimiter(None)
        .await
        .expect_err("delimiter listing is not implemented and must say so");
}
