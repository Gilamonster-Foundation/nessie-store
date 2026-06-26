//! Real-directory passthrough lifecycle, driven through the NFSFileSystem trait
//! (no kernel mount needed). Gated behind `live-fs` because it touches a real
//! temp directory — run it single-threaded in the release gate:
//!
//!   cargo test -p nessie-nfs --features live-fs -- --test-threads=1
#![cfg(feature = "live-fs")]

use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use nessie_nfs::PassthroughFs;
use nessie_nfsserve::UnixCred;
use nessie_nfsserve::nfs::{filename3, sattr3, set_uid3};
use nessie_nfsserve::vfs::NFSFileSystem;

/// A uid no test runner is likely to be, used to prove a chown actually moved
/// ownership (only assertable when the test process can chown — i.e. root).
const ALIEN_UID: u32 = 31_000;

static SEQ: AtomicU64 = AtomicU64::new(0);

fn scratch() -> PathBuf {
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("nessie-nfs-{}-{}", std::process::id(), n));
    std::fs::create_dir_all(&dir).expect("mk scratch");
    dir
}

fn name(s: &str) -> filename3 {
    filename3::from(s.as_bytes())
}

#[tokio::test]
async fn create_write_read_roundtrip() {
    let dir = scratch();
    let fs = PassthroughFs::new(&dir).unwrap();
    let root = fs.root_dir();

    let (fid, _) = fs
        .create(root, &name("hello.txt"), sattr3::default())
        .await
        .unwrap();
    let attr = fs.write(fid, 0, b"ontap-on-ramp").await.unwrap();
    assert_eq!(attr.size, 13);

    let (data, eof) = fs.read(fid, 0, 4096).await.unwrap();
    assert_eq!(data, b"ontap-on-ramp");
    assert!(eof);

    // lookup resolves the same stable fileid (inode) we created.
    assert_eq!(fs.lookup(root, &name("hello.txt")).await.unwrap(), fid);

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn mkdir_readdir_and_remove() {
    let dir = scratch();
    let fs = PassthroughFs::new(&dir).unwrap();
    let root = fs.root_dir();

    fs.mkdir(root, &name("sub")).await.unwrap();
    fs.create(root, &name("a"), sattr3::default())
        .await
        .unwrap();
    fs.create(root, &name("b"), sattr3::default())
        .await
        .unwrap();

    // Page through readdir with a tiny window: cookies must not drop/dup entries.
    let mut seen = Vec::new();
    let mut cookie = 0u64;
    loop {
        let r = fs.readdir(root, cookie, 1).await.unwrap();
        for e in &r.entries {
            seen.push(String::from_utf8_lossy(e.name.as_ref()).to_string());
            cookie = e.fileid;
        }
        if r.end || r.entries.is_empty() {
            break;
        }
    }
    seen.sort();
    assert_eq!(seen, vec!["a", "b", "sub"]);

    fs.remove(root, &name("a")).await.unwrap();
    assert!(fs.lookup(root, &name("a")).await.is_err());

    std::fs::remove_dir_all(&dir).ok();
}

// --- F2/F3: write durability + COMMIT -------------------------------------

#[tokio::test]
async fn stable_write_reports_file_sync_and_persists() {
    // F2: a FILE_SYNC/DATA_SYNC write must fsync before acknowledging and report
    // that the data is on stable storage (so the server answers FILE_SYNC, not a
    // FILE_SYNC *lie*). Regression for the no-fsync defect.
    let dir = scratch();
    let fs = PassthroughFs::new(&dir).unwrap();
    let root = fs.root_dir();
    let (fid, _) = fs
        .create(root, &name("durable"), sattr3::default())
        .await
        .unwrap();

    let (attr, on_stable) = fs
        .write_stable(fid, 0, b"committed-bytes", true)
        .await
        .unwrap();
    assert!(on_stable, "stable write must report data on stable storage");
    assert_eq!(attr.size, 15);

    // The bytes are retrievable and on disk.
    let (data, _) = fs.read(fid, 0, 4096).await.unwrap();
    assert_eq!(data, b"committed-bytes");
    assert_eq!(
        std::fs::read(dir.join("durable")).unwrap(),
        b"committed-bytes"
    );

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn unstable_write_reports_unstable_then_commit_succeeds() {
    // F3: an UNSTABLE write must NOT claim FILE_SYNC — it reports `false`, and a
    // subsequent COMMIT (client fsync) flushes it and succeeds. Regression for
    // the unimplemented-COMMIT + hardcoded-FILE_SYNC defect.
    let dir = scratch();
    let fs = PassthroughFs::new(&dir).unwrap();
    let root = fs.root_dir();
    let (fid, _) = fs
        .create(root, &name("deferred"), sattr3::default())
        .await
        .unwrap();

    let (_, on_stable) = fs
        .write_stable(fid, 0, b"unstable-bytes", false)
        .await
        .unwrap();
    assert!(!on_stable, "unstable write must not claim stable storage");

    // The plain `write` path is also UNSTABLE (delegates to write_stable(false)).
    let attr = fs.write(fid, 14, b"-more").await.unwrap();
    assert_eq!(attr.size, 19);

    // COMMIT flushes the deferred data and reports success (a real fsync).
    fs.commit(fid, 0, 0).await.unwrap();

    let (data, _) = fs.read(fid, 0, 4096).await.unwrap();
    assert_eq!(data, b"unstable-bytes-more");
    assert_eq!(
        std::fs::read(dir.join("deferred")).unwrap(),
        b"unstable-bytes-more"
    );

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn commit_on_missing_handle_is_stale() {
    // COMMIT for an inode the server has never resolved is STALE, not a panic.
    let dir = scratch();
    let fs = PassthroughFs::new(&dir).unwrap();
    assert!(fs.commit(999_999_999, 0, 0).await.is_err());
    std::fs::remove_dir_all(&dir).ok();
}

// --- F5: AUTH_UNIX ownership ----------------------------------------------

#[tokio::test]
async fn create_with_cred_owns_new_file_as_caller() {
    // F5: a file created over NFS must be owned by the calling client, not the
    // (root) daemon. The cross-uid assertion only holds when the test process is
    // privileged (root CI container); otherwise we verify the best-effort path
    // still creates the file rather than failing. Regression for root:root.
    let dir = scratch();
    let fs = PassthroughFs::new(&dir).unwrap();
    let root = fs.root_dir();

    let (_probe, _) = fs
        .create(root, &name("probe"), sattr3::default())
        .await
        .unwrap();
    let probe_path = dir.join("probe");
    let daemon_uid = std::fs::metadata(&probe_path).unwrap().uid();
    let daemon_gid = std::fs::metadata(&probe_path).unwrap().gid();
    let privileged = std::os::unix::fs::chown(&probe_path, Some(ALIEN_UID), None).is_ok();

    let cred = UnixCred {
        uid: ALIEN_UID,
        gid: daemon_gid,
        gids: vec![],
    };
    let (_fid, _) = fs
        .create_with_cred(root, &name("owned"), sattr3::default(), &cred)
        .await
        .unwrap();
    let owner = std::fs::metadata(dir.join("owned")).unwrap().uid();
    if privileged {
        assert_eq!(
            owner, ALIEN_UID,
            "privileged daemon owns the file as the caller"
        );
    } else {
        assert_eq!(
            owner, daemon_uid,
            "unprivileged: best-effort, file still created"
        );
    }

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn setgid_parent_preserves_inherited_group() {
    // F5 contract: under a set-GID directory (mode 2775) the child KEEPS the
    // inherited group — a caller's (possibly bogus) primary gid must not override
    // it. This is what lets a pod and the host co-own a shared workspace, and it
    // is provable without privilege (chown-to-self + gid left untouched).
    let dir = scratch();
    let fs = PassthroughFs::new(&dir).unwrap();
    let root = fs.root_dir();

    let (subid, _) = fs.mkdir(root, &name("shared")).await.unwrap();
    let subpath = dir.join("shared");
    std::fs::set_permissions(&subpath, std::fs::Permissions::from_mode(0o2775)).unwrap();
    let meta = std::fs::metadata(&subpath).unwrap();
    let (parent_uid, parent_gid) = (meta.uid(), meta.gid());

    // Caller's primary gid is one we are not a member of: under set-GID it is
    // ignored, so no privilege is needed and the child inherits parent_gid.
    let cred = UnixCred {
        uid: parent_uid,
        gid: 99_999,
        gids: vec![],
    };
    fs.create_with_cred(subid, &name("f"), sattr3::default(), &cred)
        .await
        .unwrap();
    let child = std::fs::metadata(subpath.join("f")).unwrap();
    assert_eq!(
        child.gid(),
        parent_gid,
        "set-GID parent: child keeps inherited group"
    );
    assert_ne!(
        child.gid(),
        99_999,
        "bogus caller gid must not override set-GID"
    );

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn setattr_applies_explicit_chown() {
    // F5: SETATTR carrying uid/gid must actually chown (fs_util refuses to). When
    // privileged the ownership changes; when not, the explicit request honestly
    // surfaces an error instead of silently succeeding.
    let dir = scratch();
    let fs = PassthroughFs::new(&dir).unwrap();
    let root = fs.root_dir();
    let (fid, _) = fs
        .create(root, &name("f"), sattr3::default())
        .await
        .unwrap();
    let path = dir.join("f");
    let daemon_uid = std::fs::metadata(&path).unwrap().uid();
    let privileged = std::os::unix::fs::chown(&path, Some(ALIEN_UID), None).is_ok();
    let _ = std::os::unix::fs::chown(&path, Some(daemon_uid), None); // reset

    let attr = sattr3 {
        uid: set_uid3::uid(ALIEN_UID),
        ..sattr3::default()
    };
    let res = fs.setattr(fid, attr).await;
    if privileged {
        assert!(res.is_ok());
        assert_eq!(std::fs::metadata(&path).unwrap().uid(), ALIEN_UID);
    } else {
        assert!(
            res.is_err(),
            "unprivileged explicit chown must surface an error"
        );
    }

    std::fs::remove_dir_all(&dir).ok();
}

// --- F4: handle aliasing / restart stability ------------------------------

#[tokio::test]
async fn distinct_files_have_distinct_restart_stable_fileids() {
    // F4: sibling files get distinct fileids, and a fresh server instance
    // (simulating a daemon restart) resolves each name to the SAME fileid — the
    // (dev,ino)-derived id is stable, so cached client handles keep working.
    let dir = scratch();
    let fs = PassthroughFs::new(&dir).unwrap();
    let root = fs.root_dir();
    let (a, _) = fs
        .create(root, &name("a"), sattr3::default())
        .await
        .unwrap();
    let (b, _) = fs
        .create(root, &name("b"), sattr3::default())
        .await
        .unwrap();
    assert_ne!(a, b, "distinct files must have distinct fileids");

    // Restart: a brand-new PassthroughFs over the same tree resolves the same ids.
    let fs2 = PassthroughFs::new(&dir).unwrap();
    let root2 = fs2.root_dir();
    assert_eq!(fs2.lookup(root2, &name("a")).await.unwrap(), a);
    assert_eq!(fs2.lookup(root2, &name("b")).await.unwrap(), b);

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn stable_handle_resolves_same_inode() {
    let dir = scratch();
    let fs = PassthroughFs::new(&dir).unwrap();
    let root = fs.root_dir();
    let (fid, _) = fs
        .create(root, &name("f"), sattr3::default())
        .await
        .unwrap();

    // The handle is just the inode — re-decoding it yields the same fileid, and
    // getattr on it works (the property that survives a daemon restart).
    let fh = fs.id_to_fh(fid);
    assert_eq!(fs.fh_to_id(&fh).unwrap(), fid);
    assert_eq!(fs.getattr(fid).await.unwrap().fileid, fid);

    std::fs::remove_dir_all(&dir).ok();
}
