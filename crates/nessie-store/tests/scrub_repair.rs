//! Scrub and repair (#140): a maintenance pass finds corrupt blobs and heals them
//! from a peer, and tops up copies a failed write left short. In-process nodes on
//! loopback; passes are driven explicitly, never by a clock.

mod common;

use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use common::{Daemon, dead_url, node_cfg, start_at, wo};
use nessie_backend_core::{BackendError, CasBackend as _, Digest};
use nessie_store::cas_node::CasNode;
use nessie_store::config::{CasBackendKind, CasConfig, CasMode};

fn fresh_root() -> PathBuf {
    static N: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "nessie-scrub-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

fn on_disk(cfg: CasConfig, root: &Path) -> CasConfig {
    CasConfig {
        backend: CasBackendKind::Disk,
        path: Some(root.to_owned()),
        ..cfg
    }
}

fn blob_path(root: &Path, d: &Digest) -> PathBuf {
    let hex = d.to_string().split_once(':').unwrap().1.to_owned();
    root.join(d.algo().name()).join(&hex[..2]).join(&hex[2..])
}

fn quarantined_path(root: &Path, d: &Digest) -> PathBuf {
    let hex = d.to_string().split_once(':').unwrap().1.to_owned();
    root.join("quarantine").join(d.algo().name()).join(hex)
}

/// Flip the bytes on disk without changing the length.
fn corrupt(root: &Path, d: &Digest) {
    let p = blob_path(root, d);
    let len = std::fs::metadata(&p).unwrap().len() as usize;
    std::fs::write(p, vec![0xAA; len]).unwrap();
}

fn inventory(node: &CasNode) -> (usize, u64) {
    let blobs = node.store().local_blobs().unwrap();
    (blobs.len(), blobs.iter().map(|b| b.size_bytes).sum())
}

fn read(node: &CasNode, d: &Digest) -> Result<Vec<u8>, BackendError> {
    let mut v = Vec::new();
    node.get(d)?.read_to_end(&mut v).unwrap();
    Ok(v)
}

#[test]
fn a_corrupt_blob_is_quarantined_refetched_and_restored() {
    let root = fresh_root();
    let peer = Daemon::start(&wo(1, &[]));
    // Capacity is exactly the blob: a restore double-counted against the corrupt
    // copy it replaces would be refused.
    let body = b"healed from the peer";
    let cfg = CasConfig {
        byte_budget: Some(body.len() as u64),
        ..wo(2, &[&peer.url])
    };
    let a = Daemon::start(&on_disk(cfg, &root));
    let d = a.node.put(&mut &body[..]).unwrap();
    let before = inventory(&a.node);
    corrupt(&root, &d);

    let report = a.node.maintain_once().unwrap();
    let s = report.scrub();
    assert_eq!(
        (s.scanned, s.corrupt, s.restored, s.unrestored),
        (1, 1, 1, 0)
    );
    assert_eq!(read(&a.node, &d).unwrap(), body, "served intact again");
    assert_eq!(inventory(&a.node), before, "inventory unchanged in total");
    assert_eq!(
        std::fs::read(quarantined_path(&root, &d)).unwrap(),
        vec![0xAA; body.len()],
        "the bad copy is kept, not deleted"
    );
    assert!(
        matches!(
            a.node.put(&mut &b"x"[..]),
            Err(BackendError::CapacityExceeded { .. })
        ),
        "accounting still sees exactly one copy"
    );
    let again = a.node.maintain_once().unwrap();
    assert_eq!(again.scrub().corrupt, 0, "healthy now");
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn with_no_peer_holding_it_a_corrupt_blob_stays_quarantined_and_is_never_served() {
    let root = fresh_root();
    let peer_url = dead_url();
    let a = Daemon::start(&on_disk(wo(2, &[&peer_url]), &root));
    // The peer was down at write time, so only A ever held it.
    let body = b"nobody else has me";
    assert!(a.node.put(&mut &body[..]).is_err());
    let d = Digest::compute(body);
    corrupt(&root, &d);

    let report = a.node.maintain_once().unwrap();
    let s = report.scrub();
    assert_eq!((s.corrupt, s.restored, s.unrestored), (1, 0, 1));
    assert!(quarantined_path(&root, &d).exists());
    assert!(!blob_path(&root, &d).exists());
    assert!(matches!(
        read(&a.node, &d),
        Err(BackendError::BlobNotFound(_))
    ));
    assert_eq!(inventory(&a.node), (0, 0));

    // Still reported on the next pass? It is out of the inventory, so the pass is
    // quiet — the quarantine directory is the record.
    assert_eq!(a.node.maintain_once().unwrap().scrub().scanned, 0);
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn an_lru_node_reclaims_a_corrupt_blob_and_a_later_get_refetches_it() {
    let root = fresh_root();
    let peer = Daemon::start(&wo(1, &[]));
    let body = b"a cache refills itself";
    let d = peer.node.put(&mut &body[..]).unwrap();
    let cfg = on_disk(
        CasConfig {
            peers: vec![common::peer(&peer.url)],
            ..node_cfg(CasMode::Lru, None, &[])
        },
        &root,
    );
    let a = Daemon::start(&cfg);
    assert_eq!(read(&a.node, &d).unwrap(), body, "fetch-through fills it");
    corrupt(&root, &d);

    let report = a.node.maintain_once().unwrap();
    let s = report.scrub();
    assert_eq!((s.corrupt, s.reclaimed, s.restored), (1, 1, 0));
    assert!(!blob_path(&root, &d).exists());
    assert!(!root.join("quarantine").exists(), "a cache just drops it");
    assert_eq!(
        read(&a.node, &d).unwrap(),
        body,
        "refetched through SwarmCas"
    );
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn a_blob_whose_push_failed_reaches_r_on_the_next_pass() {
    let peer_url = dead_url();
    let a = Daemon::start(&wo(2, &[&peer_url]));
    let body = b"the peer was down";
    assert!(a.node.put(&mut &body[..]).is_err());
    let d = Digest::compute(body);

    // Peer still down: the pass reports the shortfall and changes nothing.
    let s = a.node.maintain_once().unwrap().scrub().clone();
    assert_eq!((s.under_replicated, s.replicated), (1, 0));

    let peer = start_at(&peer_url, &wo(1, &[]));
    let s = a.node.maintain_once().unwrap().scrub().clone();
    assert_eq!((s.under_replicated, s.replicated), (0, 1));
    assert!(peer.node.has(&d).unwrap(), "two holders now");

    // Already at R: nothing to send.
    let s = a.node.maintain_once().unwrap().scrub().clone();
    assert_eq!((s.under_replicated, s.replicated), (0, 0));
}

#[test]
fn scrub_bytes_per_pass_bounds_a_pass_and_the_next_one_resumes() {
    let root = fresh_root();
    let cfg = CasConfig {
        scrub_bytes_per_pass: Some(1), // every pass has room for exactly one blob
        ..node_cfg(CasMode::Lru, None, &[])
    };
    let a = Daemon::start(&on_disk(cfg, &root));
    let mut digests: Vec<Digest> = (0..3)
        .map(|i| {
            a.node
                .put(&mut format!("blob number {i}").as_bytes())
                .unwrap()
        })
        .collect();
    digests.sort_by_key(ToString::to_string);
    let last = digests[2].clone();
    corrupt(&root, &last);

    for pass in 1..=2 {
        let s = a.node.maintain_once().unwrap().scrub().clone();
        assert_eq!(
            (s.scanned, s.corrupt),
            (1, 0),
            "pass {pass} stops at the budget"
        );
    }
    let s = a.node.maintain_once().unwrap().scrub().clone();
    assert_eq!(
        (s.scanned, s.corrupt),
        (1, 1),
        "pass 3 resumes and reaches it"
    );

    // The cursor wraps: the next pass starts over from the first blob.
    let s = a.node.maintain_once().unwrap().scrub().clone();
    assert_eq!(s.scanned, 1);
    let _ = std::fs::remove_dir_all(root);
}
