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

fn quarantined_path(root: &Path, d: &Digest, slot: u32) -> PathBuf {
    let hex = d.to_string().split_once(':').unwrap().1.to_owned();
    root.join("quarantine")
        .join(d.algo().name())
        .join(format!("{hex}.{slot}"))
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
    // The quarantined copy stays charged, so a restore needs room for both.
    let body = b"healed from the peer";
    let cfg = CasConfig {
        byte_budget: Some(2 * body.len() as u64),
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
        std::fs::read(quarantined_path(&root, &d, 0)).unwrap(),
        vec![0xAA; body.len()],
        "the bad copy is kept, not deleted"
    );
    assert_eq!(
        a.node.store().committed_bytes(),
        2 * body.len() as u64,
        "live copy plus the retained quarantine file"
    );
    assert!(full(a.node.put(&mut &b"x"[..])), "no room left");
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
    assert!(quarantined_path(&root, &d, 0).exists());
    assert!(!blob_path(&root, &d).exists());
    assert!(matches!(
        read(&a.node, &d),
        Err(BackendError::BlobNotFound(_))
    ));
    assert_eq!(inventory(&a.node), (0, 0));

    // Out of the live inventory, but still reported: it is waiting for a peer.
    let s = a.node.maintain_once().unwrap().scrub().clone();
    assert_eq!(
        (s.scanned, s.quarantined_total, s.pending_restore),
        (0, 1, 1)
    );
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
        scrub_bytes_per_pass: 1, // every pass has room for exactly one blob
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

fn full(r: Result<Digest, BackendError>) -> bool {
    matches!(r, Err(BackendError::CapacityExceeded { .. }))
}

/// Round 2 (P1): quarantined bytes are still on disk, so they stay charged.
#[test]
fn quarantined_bytes_stay_charged_against_capacity() {
    let root = fresh_root();
    let body = b"exactly capacity";
    let cfg = CasConfig {
        byte_budget: Some(body.len() as u64),
        ..wo(1, &[])
    };
    let a = Daemon::start(&on_disk(cfg, &root));
    let d = a.node.put(&mut &body[..]).unwrap();
    corrupt(&root, &d);
    assert_eq!(a.node.maintain_once().unwrap().scrub().unrestored, 1);
    assert!(
        full(a.node.put(&mut &b"x"[..])),
        "no free byte: 2C would be retained"
    );
    let _ = std::fs::remove_dir_all(root);
}

/// Round 2 (P1): the credit is what was charged, not the corrupt file's length.
#[test]
fn size_changing_corruption_credits_the_charged_size() {
    for grown in [true, false] {
        let root = fresh_root();
        let cfg = CasConfig {
            byte_budget: Some(40),
            ..wo(1, &[])
        };
        let a = Daemon::start(&on_disk(cfg, &root));
        let bad = a.node.put(&mut &b"0123456789"[..]).unwrap();
        a.node.put(&mut &b"abcdefghij"[..]).unwrap();
        let len = if grown { 20 } else { 4 };
        std::fs::write(blob_path(&root, &bad), vec![0xAA; len]).unwrap();
        a.node.maintain_once().unwrap();
        // 10 healthy + `len` quarantined are retained: 40 - 10 - len left.
        let left = 40 - 10 - len;
        assert!(
            full(a.node.put(&mut vec![7u8; left + 1].as_slice())),
            "grown={grown}"
        );
        a.node.put(&mut vec![8u8; left].as_slice()).unwrap();
        let _ = std::fs::remove_dir_all(root);
    }
}

/// Round 2: a restore reserves new room, because the quarantined copy stays charged.
#[test]
fn restore_at_capacity_is_refused_and_with_room_it_is_admitted() {
    let body = b"restore needs room";
    let n = body.len() as u64;
    for (capacity, restored) in [(n, false), (2 * n, true)] {
        let root = fresh_root();
        let peer = Daemon::start(&wo(1, &[]));
        let cfg = CasConfig {
            byte_budget: Some(capacity),
            ..wo(2, &[&peer.url])
        };
        let a = Daemon::start(&on_disk(cfg, &root));
        let d = a.node.put(&mut &body[..]).unwrap();
        corrupt(&root, &d);
        let s = a.node.maintain_once().unwrap().scrub().clone();
        assert_eq!((s.restored > 0, s.unrestored > 0), (restored, !restored));
        assert_eq!(s.pending_restore, usize::from(!restored));
        assert_eq!(a.node.has(&d).unwrap(), restored);
        let live = if restored { n } else { 0 };
        assert_eq!(a.node.store().committed_bytes(), live + n);
        let _ = std::fs::remove_dir_all(root);
    }
}

/// Round 2: quarantine files are charged again after a restart.
#[test]
fn a_restart_charges_the_quarantine_files_it_finds() {
    let root = fresh_root();
    let cfg = on_disk(
        CasConfig {
            byte_budget: Some(64),
            ..wo(1, &[])
        },
        &root,
    );
    let d = {
        let a = Daemon::start(&cfg);
        let d = a.node.put(&mut &b"0123456789"[..]).unwrap();
        corrupt(&root, &d);
        a.node.maintain_once().unwrap();
        d
    };
    let again = Daemon::start(&cfg);
    assert_eq!(again.node.store().committed_bytes(), 10);
    assert!(!again.node.has(&d).unwrap());
    let _ = std::fs::remove_dir_all(root);
}

/// Round 2: a failed restore is retried by later passes, across a restart.
#[test]
fn a_pending_restore_finishes_when_the_peer_returns_even_across_a_restart() {
    let (root_a, root_p) = (fresh_root(), fresh_root());
    let mut peer = Daemon::start(&on_disk(wo(1, &[]), &root_p));
    let peer_url = peer.url.clone();
    let cfg = on_disk(wo(2, &[&peer_url]), &root_a);
    let body = b"peer holds the good copy";
    let d = {
        let a = Daemon::start(&cfg);
        let d = a.node.put(&mut &body[..]).unwrap();
        peer.kill(); // the peer goes away before the damage is found
        corrupt(&root_a, &d);
        let s = a.node.maintain_once().unwrap().scrub().clone();
        assert_eq!((s.unrestored, s.pending_restore), (1, 1));
        d
    }; // node A restarts here
    let a = Daemon::start(&cfg);
    let s = a.node.maintain_once().unwrap().scrub().clone();
    assert_eq!(
        (s.restored, s.pending_restore),
        (0, 1),
        "still pending, still visible"
    );

    let _peer = start_at(&peer_url, &on_disk(wo(1, &[]), &root_p));
    let s = a.node.maintain_once().unwrap().scrub().clone();
    assert_eq!(
        (s.restored, s.pending_restore, s.quarantined_total),
        (1, 0, 1)
    );
    assert_eq!(read(&a.node, &d).unwrap(), body);
    let _ = (
        std::fs::remove_dir_all(root_a),
        std::fs::remove_dir_all(root_p),
    );
}

/// Round 2: corrupt, restore, corrupt again — both bad copies are kept and charged.
#[test]
fn repeated_corruption_keeps_every_quarantined_copy_charged() {
    let root = fresh_root();
    let peer = Daemon::start(&wo(1, &[]));
    let body = b"rots twice";
    let n = body.len() as u64;
    let cfg = CasConfig {
        byte_budget: Some(3 * n),
        ..wo(2, &[&peer.url])
    };
    let a = Daemon::start(&on_disk(cfg, &root));
    let d = a.node.put(&mut &body[..]).unwrap();
    for _ in 0..2 {
        corrupt(&root, &d);
        assert_eq!(a.node.maintain_once().unwrap().scrub().restored, 1);
    }
    assert!(quarantined_path(&root, &d, 0).exists());
    assert!(quarantined_path(&root, &d, 1).exists());
    assert_eq!(
        a.node.store().committed_bytes(),
        3 * n,
        "live + two quarantined"
    );
    let _ = std::fs::remove_dir_all(root);
}

/// Round 2: the scrub budget is finite unless a deployment opts out, and 0 is off.
#[test]
fn the_default_budget_is_one_gib_and_zero_turns_the_scrub_off() {
    use nessie_store::config::DEFAULT_SCRUB_BYTES_PER_PASS;
    assert_eq!(CasConfig::default().scrub_bytes_per_pass, 1 << 30);
    assert_eq!(DEFAULT_SCRUB_BYTES_PER_PASS, 1 << 30);
    let parsed: CasConfig = toml::from_str("mode = \"lru\"\nbyte_budget = 1\n").unwrap();
    assert_eq!(
        parsed.scrub_bytes_per_pass,
        1 << 30,
        "unset is not unlimited"
    );

    let root = fresh_root();
    let cfg = on_disk(
        CasConfig {
            scrub_bytes_per_pass: 0,
            ..node_cfg(CasMode::Lru, None, &[])
        },
        &root,
    );
    let a = Daemon::start(&cfg);
    let d = a.node.put(&mut &b"unscrubbed"[..]).unwrap();
    corrupt(&root, &d);
    assert_eq!(a.node.maintain_once().unwrap().scrub().scanned, 0);
    let _ = std::fs::remove_dir_all(root);
}

/// Feeds `bytes`, but its first read waits for the test: `paused` says the writer is
/// mid-read, `resume` lets it finish.
struct PausedReader {
    bytes: std::io::Cursor<Vec<u8>>,
    gates: Option<(
        std::sync::Arc<std::sync::Barrier>,
        std::sync::Arc<std::sync::Barrier>,
    )>,
}

impl std::io::Read for PausedReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if let Some((paused, resume)) = self.gates.take() {
            paused.wait();
            resume.wait();
        }
        self.bytes.read(buf)
    }
}

/// Every byte on disk this node keeps: live blobs plus quarantine files.
fn retained(node: &CasNode) -> u64 {
    let live: u64 = node
        .store()
        .local_blobs()
        .unwrap()
        .iter()
        .map(|b| b.size_bytes)
        .sum();
    let held: u64 = node
        .store()
        .quarantined()
        .unwrap()
        .iter()
        .map(|q| q.size_bytes)
        .sum();
    live + held
}

/// Round 3 (P1): a re-put of a held digest is ungated. If scrub quarantines the digest
/// while that write is paused mid-read, the write must not publish a live copy that
/// nothing is charged for.
#[test]
fn a_reput_paused_across_a_quarantine_never_leaves_uncharged_bytes() {
    use std::sync::{Arc, Barrier};
    let root = fresh_root();
    let body = b"re-put while rotting";
    let n = body.len() as u64;
    let cfg = CasConfig {
        byte_budget: Some(n),
        ..wo(1, &[])
    };
    let a = Daemon::start(&on_disk(cfg, &root));
    let d = a.node.put(&mut &body[..]).unwrap();
    corrupt(&root, &d);

    let (paused, resume) = (Arc::new(Barrier::new(2)), Arc::new(Barrier::new(2)));
    let writer = {
        let (node, d) = (a.node.clone(), d.clone());
        let gates = Some((paused.clone(), resume.clone()));
        std::thread::spawn(move || {
            let mut src = PausedReader {
                bytes: std::io::Cursor::new(body.to_vec()),
                gates,
            };
            node.put_keyed_local(&d, &mut src)
        })
    };
    paused.wait(); // the re-put has decided it is a duplicate and is mid-read
    a.node.maintain_once().unwrap();
    resume.wait();
    writer.join().unwrap().unwrap();

    assert_eq!(
        a.node.store().committed_bytes(),
        retained(&a.node),
        "everything kept on disk is charged"
    );
    assert!(full(a.node.put(&mut &b"x"[..])), "no free byte at capacity");
    let _ = std::fs::remove_dir_all(root);
}
