//! Real-directory passthrough lifecycle, driven through the NFSFileSystem trait
//! (no kernel mount needed). Gated behind `live-fs` because it touches a real
//! temp directory — run it single-threaded in the release gate:
//!
//!   cargo test -p nessie-nfs --features live-fs -- --test-threads=1
#![cfg(all(unix, feature = "live-fs"))]

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
            cookie = e.cookie;
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

// --- LINK (NFSPROC3_LINK) --------------------------------------------------

#[tokio::test]
async fn link_creates_a_second_name_for_the_same_inode() {
    // Regression: NFSPROC3_LINK was entirely unimplemented (fell into the
    // dispatch table's catch-all), so any client relying on hard links —
    // e.g. git's object-store dedup between forked repos — failed every
    // link() with NFS3ERR_NOTSUPP / "Operation not supported".
    let dir = scratch();
    let fs = PassthroughFs::new(&dir).unwrap();
    let root = fs.root_dir();

    let (fid, _) = fs
        .create(root, &name("original"), sattr3::default())
        .await
        .unwrap();
    fs.write(fid, 0, b"shared-bytes").await.unwrap();

    let attr = fs.link(fid, root, &name("linked")).await.unwrap();
    assert_eq!(attr.size, 12);

    // Same underlying inode: nlink is now 2, and both paths read identical bytes.
    let orig_meta = std::fs::metadata(dir.join("original")).unwrap();
    let link_meta = std::fs::metadata(dir.join("linked")).unwrap();
    assert_eq!(orig_meta.ino(), link_meta.ino());
    assert_eq!(link_meta.nlink(), 2);
    assert_eq!(std::fs::read(dir.join("linked")).unwrap(), b"shared-bytes");

    // Writing through the original is visible through the link (one inode).
    fs.write(fid, 0, b"changed-bytes").await.unwrap();
    assert_eq!(std::fs::read(dir.join("linked")).unwrap(), b"changed-bytes");

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn link_into_missing_directory_is_an_error() {
    let dir = scratch();
    let fs = PassthroughFs::new(&dir).unwrap();
    let root = fs.root_dir();
    let (fid, _) = fs
        .create(root, &name("f"), sattr3::default())
        .await
        .unwrap();

    // 999_999_999 is never a real fileid in a fresh scratch dir.
    assert!(fs.link(fid, 999_999_999, &name("f2")).await.is_err());

    std::fs::remove_dir_all(&dir).ok();
}

// --- Hard links vs directory enumeration and handles (PR #114 review) --------

/// Names listed by paging `readdir` with a window of `page` entries.
async fn list_names(fs: &PassthroughFs, dir: u64, page: usize) -> Vec<String> {
    let mut seen = Vec::new();
    let mut cookie = 0u64;
    loop {
        let r = fs.readdir(dir, cookie, page).await.unwrap();
        for e in &r.entries {
            seen.push(String::from_utf8_lossy(e.name.as_ref()).to_string());
            cookie = e.cookie;
        }
        if r.end || r.entries.is_empty() {
            break;
        }
    }
    seen.sort();
    seen
}

#[tokio::test]
async fn readdir_lists_every_hard_link_name_even_when_paged() {
    // Regression (PR #114 review, P1): hard links share a fileid, and readdir
    // keyed its entries by fileid, so only one of the names was listed; the
    // fileid was also the resume cookie, so a page boundary between two names
    // of one inode dropped the second.
    let dir = scratch();
    let fs = PassthroughFs::new(&dir).unwrap();
    let root = fs.root_dir();
    let (fid, _) = fs
        .create(root, &name("a"), sattr3::default())
        .await
        .unwrap();
    fs.link(fid, root, &name("b")).await.unwrap();
    fs.link(fid, root, &name("c")).await.unwrap();

    for page in [1, 2, 100] {
        assert_eq!(
            list_names(&fs, root, page).await,
            ["a", "b", "c"],
            "page={page}"
        );
    }
    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn removing_one_link_keeps_the_handle_valid_for_the_other() {
    // Regression (PR #114 review, P1): `remove` dropped the shared fileid's
    // table entry even though another name survived, so the client's handle went
    // NFS3ERR_STALE while the file still existed.
    for remove_first in ["orig", "linked"] {
        let dir = scratch();
        let fs = PassthroughFs::new(&dir).unwrap();
        let root = fs.root_dir();
        let (fid, _) = fs
            .create(root, &name("orig"), sattr3::default())
            .await
            .unwrap();
        fs.write(fid, 0, b"bytes").await.unwrap();
        fs.link(fid, root, &name("linked")).await.unwrap();

        fs.remove(root, &name(remove_first)).await.unwrap();
        let (data, _) = fs.read(fid, 0, 16).await.expect("handle must stay valid");
        assert_eq!(data, b"bytes", "removed {remove_first}");
        assert_eq!(fs.getattr(fid).await.unwrap().nlink, 1);

        // Removing the last name does retire the handle.
        let last = if remove_first == "orig" {
            "linked"
        } else {
            "orig"
        };
        fs.remove(root, &name(last)).await.unwrap();
        assert!(fs.getattr(fid).await.is_err());
        std::fs::remove_dir_all(&dir).ok();
    }
}

// --- Round 3 (PR #114 review): alias safety and positional cookies ----------

#[tokio::test]
async fn handle_never_resolves_to_a_file_that_replaced_its_name() {
    // Regression: create a (handle A), link a2, rename another file b over a,
    // remove a2. A's stored names were [a, a2] -> [a], but `a` now belongs to b;
    // read(A) served b's bytes instead of NFS3ERR_STALE.
    let dir = scratch();
    let fs = PassthroughFs::new(&dir).unwrap();
    let root = fs.root_dir();
    let (a, _) = fs
        .create(root, &name("a"), sattr3::default())
        .await
        .unwrap();
    fs.write(a, 0, b"A-bytes").await.unwrap();
    fs.link(a, root, &name("a2")).await.unwrap();
    let (b, _) = fs
        .create(root, &name("b"), sattr3::default())
        .await
        .unwrap();
    fs.write(b, 0, b"B-bytes").await.unwrap();

    fs.rename(root, &name("b"), root, &name("a")).await.unwrap();
    fs.remove(root, &name("a2")).await.unwrap();

    assert!(fs.read(a, 0, 16).await.is_err(), "A's last name is gone");
    assert!(fs.getattr(a).await.is_err());
    assert_eq!(fs.read(b, 0, 16).await.unwrap().0, b"B-bytes");
    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn renaming_between_two_names_of_one_inode_keeps_the_handle() {
    // Regression: rename(a, a2) with both names linked to one inode is a no-op
    // at the OS, but the source name was forgotten anyway; removing a2 then
    // staled a handle whose name `a` survives.
    let dir = scratch();
    let fs = PassthroughFs::new(&dir).unwrap();
    let root = fs.root_dir();
    let (a, _) = fs
        .create(root, &name("a"), sattr3::default())
        .await
        .unwrap();
    fs.write(a, 0, b"bytes").await.unwrap();
    fs.link(a, root, &name("a2")).await.unwrap();

    fs.rename(root, &name("a"), root, &name("a2"))
        .await
        .unwrap();
    fs.remove(root, &name("a2")).await.unwrap();

    assert_eq!(fs.read(a, 0, 16).await.unwrap().0, b"bytes");
    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn readdir_cookies_are_stable_unique_and_resumable() {
    // Cookies are a hash of the name: unique per name here, nonzero, ordered, and
    // identical across two listings; resuming at the largest ends cleanly.
    let dir = scratch();
    let fs = PassthroughFs::new(&dir).unwrap();
    let root = fs.root_dir();
    let (x, _) = fs
        .create(root, &name("x"), sattr3::default())
        .await
        .unwrap();
    for n in ["w", "y", "z"] {
        fs.link(x, root, &name(n)).await.unwrap();
    }
    fs.create(root, &name("a"), sattr3::default())
        .await
        .unwrap();

    let cookies = |r: &nessie_nfsserve::vfs::ReadDirResult| -> Vec<u64> {
        r.entries.iter().map(|e| e.cookie).collect()
    };
    let one = cookies(&fs.readdir(root, 0, 100).await.unwrap());
    let two = cookies(&fs.readdir(root, 0, 100).await.unwrap());
    assert_eq!(one, two);
    assert!(one.windows(2).all(|w| w[0] < w[1]) && one[0] > 0);
    let tail = fs.readdir(root, *one.last().unwrap(), 2).await.unwrap();
    assert!(tail.entries.is_empty() && tail.end);
    for page in [1, 2, 3] {
        assert_eq!(list_names(&fs, root, page).await, ["a", "w", "x", "y", "z"]);
    }
    std::fs::remove_dir_all(&dir).ok();
}

// --- Round 4 (PR #114 review): cookies survive directory changes ------------

/// Page through `root` one entry at a time; after the first page run `mutate`.
/// Returns every name listed, in order.
async fn page_with_change(
    fs: &PassthroughFs,
    root: u64,
    mutate: impl AsyncFnOnce(&str),
) -> Vec<String> {
    let first = fs.readdir(root, 0, 1).await.unwrap();
    let e = &first.entries[0];
    let first_name = String::from_utf8_lossy(e.name.as_ref()).to_string();
    mutate(&first_name).await;
    let mut seen = vec![first_name];
    let mut cookie = e.cookie;
    loop {
        let r = fs.readdir(root, cookie, 1).await.unwrap();
        for e in &r.entries {
            seen.push(String::from_utf8_lossy(e.name.as_ref()).to_string());
            cookie = e.cookie;
        }
        if r.end || r.entries.is_empty() {
            return seen;
        }
    }
}

async fn five_files() -> (PathBuf, PassthroughFs, u64) {
    let dir = scratch();
    let fs = PassthroughFs::new(&dir).unwrap();
    let root = fs.root_dir();
    for n in ["a", "b", "c", "d", "e"] {
        fs.create(root, &name(n), sattr3::default()).await.unwrap();
    }
    (dir, fs, root)
}

#[tokio::test]
async fn removing_an_already_listed_entry_between_pages_skips_nothing() {
    // Regression: positional cookies shifted when an earlier entry was removed,
    // so the next page skipped an entry the client never saw. nfsserve never
    // sends BAD_COOKIE, so the cookie itself must stay valid.
    let (dir, fs, root) = five_files().await;
    let seen = page_with_change(&fs, root, async |first| {
        fs.remove(root, &name(first)).await.unwrap();
    })
    .await;
    let mut sorted = seen.clone();
    sorted.sort();
    assert_eq!(sorted, ["a", "b", "c", "d", "e"], "listed: {seen:?}");
    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn inserting_between_pages_repeats_nothing_and_skips_nothing() {
    // Regression: a name inserted before the boundary shifted positions and
    // repeated an already-listed entry on the next page.
    let (dir, fs, root) = five_files().await;
    let seen = page_with_change(&fs, root, async |_| {
        for n in ["0ins", "zins"] {
            fs.create(root, &name(n), sattr3::default()).await.unwrap();
        }
    })
    .await;
    for n in ["a", "b", "c", "d", "e"] {
        assert_eq!(
            seen.iter().filter(|s| *s == n).count(),
            1,
            "{n} in {seen:?}"
        );
    }
    let mut dedup = seen.clone();
    dedup.dedup();
    dedup.sort();
    dedup.dedup();
    assert_eq!(dedup.len(), seen.len(), "repeats in {seen:?}");
    std::fs::remove_dir_all(&dir).ok();
}
