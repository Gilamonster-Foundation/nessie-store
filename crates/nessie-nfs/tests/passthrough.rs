//! Real-directory passthrough lifecycle, driven through the NFSFileSystem trait
//! (no kernel mount needed). Gated behind `live-fs` because it touches a real
//! temp directory — run it single-threaded in the release gate:
//!
//!   cargo test -p nessie-nfs --features live-fs -- --test-threads=1
#![cfg(feature = "live-fs")]

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use nessie_nfs::PassthroughFs;
use nessie_nfsserve::nfs::{filename3, sattr3};
use nessie_nfsserve::vfs::NFSFileSystem;

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
