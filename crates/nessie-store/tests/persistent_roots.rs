//! #137: pins survive a restart. The pin set is a `Tree` in the CAS; `<path>/refs/roots`
//! names it. A bad ref refuses startup instead of silently dropping every pin.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use nessie_backend_core::{
    BackendError, CasBackend as _, Digest, PeerId, Tree, TreeEntry, TreeKind,
};
use nessie_store::cas_node::CasNode;
use nessie_store::config::{CasBackendKind, CasConfig, CasMode};

/// A disk-backed lru node with no budget and no grace: a pass evicts anything unpinned.
fn lru(dir: &Path) -> CasConfig {
    CasConfig {
        mode: CasMode::Lru,
        backend: CasBackendKind::Disk,
        path: Some(dir.to_path_buf()),
        byte_budget: Some(0),
        gc_grace_secs: 0,
        ..CasConfig::default()
    }
}

fn open(cfg: &CasConfig) -> CasNode {
    CasNode::from_config(cfg, PeerId::new("me")).expect("node opens")
}

fn ref_path(dir: &Path) -> PathBuf {
    dir.join("refs").join("roots")
}

fn put(node: &CasNode, bytes: &[u8]) -> Digest {
    node.put(&mut &bytes[..]).expect("put")
}

/// The root tree the ref names (one line: the tree's digest text form).
fn ref_tree(dir: &Path) -> Digest {
    let text = std::fs::read_to_string(ref_path(dir)).expect("ref file exists");
    text.trim_end().parse().expect("ref names a digest")
}

#[test]
fn pin_survives_restart_and_lru_eviction() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = lru(dir.path());
    let (pinned, upload) = {
        let node = open(&cfg);
        let pinned = put(&node, b"pinned manifest");
        // A plain upload (what the REAPI and S3 faces do) is not a root.
        let upload = put(&node, b"plain upload");
        node.pin(pinned.clone()).unwrap();
        (pinned, upload)
    };

    let node = open(&cfg);
    assert!(
        node.roots().pins().contains(&pinned),
        "the pin was reloaded"
    );
    node.maintain_once().unwrap();
    assert!(
        node.has(&pinned).unwrap(),
        "a pinned blob survives the restart"
    );
    assert!(!node.has(&upload).unwrap(), "an upload is not a root");
}

#[test]
fn unpin_survives_restart() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = lru(dir.path());
    let d = {
        let node = open(&cfg);
        let d = put(&node, b"briefly pinned");
        node.pin(d.clone()).unwrap();
        node.unpin(&d).unwrap();
        d
    };
    let node = open(&cfg);
    assert!(node.roots().pins().is_empty());
    node.maintain_once().unwrap();
    assert!(
        !node.has(&d).unwrap(),
        "unpinned before the restart, so evictable"
    );
}

#[test]
fn pin_then_unpin_restores_the_byte_identical_ref() {
    let dir = tempfile::tempdir().unwrap();
    let node = open(&lru(dir.path()));
    node.pin(Digest::compute(b"a")).unwrap();
    let before = std::fs::read(ref_path(dir.path())).expect("ref written by pin");

    let b = Digest::compute(b"b");
    node.pin(b.clone()).unwrap();
    assert_ne!(std::fs::read(ref_path(dir.path())).unwrap(), before);
    node.unpin(&b).unwrap();
    assert_eq!(std::fs::read(ref_path(dir.path())).unwrap(), before);
}

#[test]
fn current_root_tree_is_kept_and_a_superseded_one_is_evicted() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = lru(dir.path());
    let node = open(&cfg);
    node.pin(Digest::compute(b"a")).unwrap();
    let old = ref_tree(dir.path());
    node.pin(Digest::compute(b"b")).unwrap();
    let current = ref_tree(dir.path());

    node.maintain_once().unwrap();
    assert!(
        node.has(&current).unwrap(),
        "the tree the ref names is pinned"
    );
    assert!(
        !node.has(&old).unwrap(),
        "a superseded tree is an ordinary blob"
    );
    drop(node);
    assert_eq!(open(&cfg).roots().pins().len(), 2);
}

/// Point the ref at whatever `target` stores (or names), then expect startup to refuse
/// with an error naming the ref file.
fn assert_refuses(why: &str, target: impl Fn(&CasNode) -> String) {
    let dir = tempfile::tempdir().unwrap();
    let cfg = lru(dir.path());
    let line = target(&open(&cfg));
    std::fs::create_dir_all(dir.path().join("refs")).unwrap();
    std::fs::write(ref_path(dir.path()), format!("{line}\n")).unwrap();
    let err = CasNode::from_config(&cfg, PeerId::new("me"))
        .err()
        .unwrap_or_else(|| panic!("{why}: must refuse to start"));
    assert!(format!("{err:#}").contains("refs/roots"), "{why}: {err:#}");
}

#[test]
fn a_bad_ref_refuses_to_start() {
    assert_refuses("unparseable", |_| "garbage".into());
    assert_refuses("tree not held", |_| {
        Digest::compute(b"never stored").to_string()
    });
    assert_refuses("not a tree", |node| put(node, b"just bytes").to_string());
    assert_refuses("name is not the entry digest", |node| {
        let mut entries = BTreeMap::new();
        entries.insert(
            Digest::compute(b"one").to_string(),
            TreeEntry {
                digest: Digest::compute(b"another"),
                kind: TreeKind::File {
                    is_executable: false,
                },
            },
        );
        put(node, &Tree { entries }.to_canonical_bytes()).to_string()
    });
}

#[test]
fn a_pin_refused_by_capacity_changes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = CasConfig {
        mode: CasMode::WriteOnly,
        replication_factor: Some(1),
        byte_budget: Some(8),
        ..lru(dir.path())
    };
    let node = open(&cfg);
    put(&node, b"8 bytes!"); // capacity now full: no room for a root tree
    let d = Digest::compute(b"wanted");
    assert!(matches!(
        node.pin(d),
        Err(BackendError::CapacityExceeded { .. })
    ));
    assert!(
        node.roots().pins().is_empty(),
        "the in-memory set is unchanged"
    );
    assert!(!ref_path(dir.path()).exists(), "no ref was written");
}

#[tokio::test]
async fn the_pin_route_reports_a_pin_it_could_not_store() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = CasConfig {
        mode: CasMode::WriteOnly,
        replication_factor: Some(1),
        byte_budget: Some(8),
        ..lru(dir.path())
    };
    let node = std::sync::Arc::new(open(&cfg));
    put(&node, b"8 bytes!");
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!(
        "http://{}/cas/v1/roots/{}",
        l.local_addr().unwrap(),
        Digest::compute(b"wanted")
    );
    let app = nessie_store::cas_routes::peer_routes(node.clone());
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });

    let status = reqwest::Client::new()
        .post(url)
        .send()
        .await
        .unwrap()
        .status();
    assert_eq!(status, 507, "a pin that was not stored is not a 201");
    assert!(node.roots().pins().is_empty());
}
