//! Persistent on-disk [`CasBackend`]: the first durable [`ReclaimableCas`].
//!
//! Three properties, borrowed from 3FS's chunk engine: bytes land before the
//! metadata commit; uncommitted writes never survive a reopen; a read in flight
//! survives a concurrent delete.
//!
//! ```text
//! <root>/tmp/<pid>-<counter>                  in-flight writes
//! <root>/<algo name>/<hex[0..2]>/<hex[2..]>   committed blobs
//! ```
//!
//! Nothing is written in place under `<root>/<algo>/`; only `rename` lands a
//! blob there, so a crash leaves either no blob or a complete one.

use nessie_backend_core::{
    BackendError, CasBackend, Digest, DigestAlgo, DigestHasher, LocalBlob, ReclaimableCas,
};
use std::fs;
use std::io::{Cursor, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

const ALGOS: [DigestAlgo; 2] = [DigestAlgo::Blake3, DigestAlgo::Sha256];

/// A content-addressed blob store rooted at a directory.
#[derive(Debug)]
pub struct DiskCas {
    root: PathBuf,
}

/// Process-wide, so two stores in one process never mint the same tmp name.
static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

fn io_err(what: &str, e: std::io::Error) -> BackendError {
    BackendError::Internal(format!("cas-disk {what}: {e}"))
}

impl DiskCas {
    /// Open (creating if needed) a store at `root`, discarding any uncommitted
    /// `tmp/` writes left by a previous run.
    ///
    /// # Errors
    ///
    /// [`BackendError::Internal`] if the directories cannot be prepared.
    pub fn open(root: impl Into<PathBuf>) -> Result<Self, BackendError> {
        let root = root.into();
        fs::create_dir_all(&root).map_err(|e| io_err("create root", e))?;
        let tmp = root.join("tmp");
        match fs::remove_dir_all(&tmp) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(io_err("sweep tmp", e)),
        }
        fs::create_dir_all(&tmp).map_err(|e| io_err("create tmp", e))?;
        Ok(Self { root })
    }

    fn path(&self, d: &Digest) -> PathBuf {
        let hex = d.to_string();
        let hex = hex.split_once(':').map_or("", |(_, h)| h);
        self.root
            .join(d.algo().name())
            .join(&hex[..2])
            .join(&hex[2..])
    }

    /// Stream `src` into a fresh tmp file while hashing it; returns the tmp path
    /// and the digest of what was written.
    fn write_tmp(
        &self,
        src: &mut dyn Read,
        mut hasher: DigestHasher,
    ) -> Result<(PathBuf, Digest), BackendError> {
        let n = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let tmp = self
            .root
            .join("tmp")
            .join(format!("{}-{n}", std::process::id()));
        let result = (|| {
            let mut file = fs::File::create(&tmp).map_err(|e| io_err("create tmp file", e))?;
            let mut buf = [0u8; 8192];
            loop {
                let read = src.read(&mut buf).map_err(|e| io_err("read source", e))?;
                if read == 0 {
                    break;
                }
                hasher.update(&buf[..read]);
                file.write_all(&buf[..read])
                    .map_err(|e| io_err("write tmp file", e))?;
            }
            file.sync_all().map_err(|e| io_err("sync tmp file", e))
        })();
        match result {
            Ok(()) => Ok((tmp, hasher.finalize())),
            Err(e) => {
                let _ = fs::remove_file(&tmp);
                Err(e)
            }
        }
    }

    /// The atomic metadata commit: rename the finished tmp file into place.
    /// Concurrent puts of one digest race harmlessly — identical bytes either way.
    fn commit(&self, tmp: &Path, digest: &Digest) -> Result<(), BackendError> {
        let dest = self.path(digest);
        // ponytail: no directory fsync, so a power loss right after rename can
        // drop the blob (never corrupt it); add parent-dir sync_all if durability
        // across power loss is required.
        let landed = dest
            .parent()
            .map_or(Ok(()), fs::create_dir_all)
            .and_then(|()| fs::rename(tmp, &dest));
        landed.map_err(|e| {
            let _ = fs::remove_file(tmp);
            io_err("commit", e)
        })
    }
}

impl CasBackend for DiskCas {
    fn has(&self, digest: &Digest) -> Result<bool, BackendError> {
        Ok(self.size(digest)?.is_some())
    }

    fn get(&self, digest: &Digest) -> Result<Box<dyn Read + Send>, BackendError> {
        // ponytail: whole blob is read into RAM so it can be verified *before* any
        // byte is served; a streaming check can only fail after serving. Move to a
        // verified chunk tree if blobs outgrow memory.
        let bytes = match fs::read(self.path(digest)) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(BackendError::BlobNotFound(digest.clone()));
            }
            Err(e) => return Err(io_err("read blob", e)),
        };
        if !digest.verify(&bytes) {
            return Err(BackendError::Internal(format!("corrupt blob {digest}")));
        }
        Ok(Box::new(Cursor::new(bytes)))
    }

    fn put(&self, source: &mut dyn Read) -> Result<Digest, BackendError> {
        let (tmp, digest) = self.write_tmp(source, DigestHasher::new(DigestAlgo::DEFAULT))?;
        self.commit(&tmp, &digest)?;
        Ok(digest)
    }

    fn put_keyed(&self, expected: &Digest, source: &mut dyn Read) -> Result<(), BackendError> {
        let (tmp, actual) = self.write_tmp(source, DigestHasher::new(expected.algo()))?;
        if actual != *expected {
            let _ = fs::remove_file(&tmp);
            return Err(BackendError::InvalidArgument(format!(
                "put_keyed: bytes do not hash to {expected}"
            )));
        }
        self.commit(&tmp, expected)
    }

    fn size(&self, digest: &Digest) -> Result<Option<u64>, BackendError> {
        match fs::metadata(self.path(digest)) {
            Ok(m) => Ok(Some(m.len())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(io_err("stat blob", e)),
        }
    }

    fn as_reclaimable(&self) -> Option<&dyn ReclaimableCas> {
        Some(self)
    }
}

impl ReclaimableCas for DiskCas {
    fn iter_local(&self) -> Result<Vec<LocalBlob>, BackendError> {
        let mut out = Vec::new();
        for algo in ALGOS {
            let Ok(prefixes) = fs::read_dir(self.root.join(algo.name())) else {
                continue;
            };
            for prefix in prefixes.flatten() {
                let Ok(files) = fs::read_dir(prefix.path()) else {
                    continue;
                };
                for file in files.flatten() {
                    let name = format!(
                        "{}:{}{}",
                        algo.name(),
                        prefix.file_name().to_string_lossy(),
                        file.file_name().to_string_lossy()
                    );
                    let Ok(digest) = name.parse::<Digest>() else {
                        continue;
                    };
                    let size_bytes = file.metadata().map_err(|e| io_err("stat blob", e))?.len();
                    out.push(LocalBlob { digest, size_bytes });
                }
            }
        }
        Ok(out)
    }

    fn reclaim(&self, digest: &Digest) -> Result<bool, BackendError> {
        match fs::remove_file(self.path(digest)) {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(io_err("reclaim", e)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nessie_backend_core::{CasBackend, Digest, DigestAlgo, ReclaimableCas};
    use std::io::Read;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn fresh() -> PathBuf {
        static N: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "nessie-cas-disk-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn blob_path(root: &std::path::Path, d: &Digest) -> PathBuf {
        let hex = d.to_string().split_once(':').unwrap().1.to_owned();
        root.join(d.algo().name()).join(&hex[..2]).join(&hex[2..])
    }

    fn read_all(mut r: Box<dyn Read + Send>) -> Vec<u8> {
        let mut v = Vec::new();
        r.read_to_end(&mut v).unwrap();
        v
    }

    #[test]
    fn put_is_idempotent_and_roundtrips() {
        let root = fresh();
        let cas = DiskCas::open(&root).unwrap();
        let a = cas.put(&mut &b"hello"[..]).unwrap();
        let b = cas.put(&mut &b"hello"[..]).unwrap();
        assert_eq!(a, b);
        assert_eq!(read_all(cas.get(&a).unwrap()), b"hello");
        assert_eq!(cas.size(&a).unwrap(), Some(5));
        assert_eq!(cas.iter_local().unwrap().len(), 1);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn keyed_mismatch_stores_nothing() {
        let root = fresh();
        let cas = DiskCas::open(&root).unwrap();
        let wrong = Digest::compute_with(DigestAlgo::Sha256, b"other");
        assert!(cas.put_keyed(&wrong, &mut &b"bytes"[..]).is_err());
        assert!(!cas.has(&wrong).unwrap());
        assert!(cas.iter_local().unwrap().is_empty());
        assert_eq!(std::fs::read_dir(root.join("tmp")).unwrap().count(), 0);
        let right = Digest::compute_with(DigestAlgo::Sha256, b"bytes");
        cas.put_keyed(&right, &mut &b"bytes"[..]).unwrap();
        assert!(blob_path(&root, &right).exists());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn open_sweeps_uncommitted_tmp() {
        let root = fresh();
        std::fs::create_dir_all(root.join("tmp")).unwrap();
        std::fs::write(root.join("tmp/1-0"), b"half a write").unwrap();
        let cas = DiskCas::open(&root).unwrap();
        assert_eq!(std::fs::read_dir(root.join("tmp")).unwrap().count(), 0);
        assert!(cas.iter_local().unwrap().is_empty());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn corrupt_bytes_fail_get() {
        let root = fresh();
        let cas = DiskCas::open(&root).unwrap();
        let d = cas.put(&mut &b"pristine"[..]).unwrap();
        std::fs::write(blob_path(&root, &d), b"tampered").unwrap();
        assert!(cas.get(&d).is_err());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn reclaim_during_open_read_still_serves_bytes() {
        let root = fresh();
        let cas = DiskCas::open(&root).unwrap();
        let d = cas.put(&mut &b"in flight"[..]).unwrap();
        let reader = cas.get(&d).unwrap();
        assert!(cas.reclaim(&d).unwrap());
        assert!(!cas.reclaim(&d).unwrap());
        assert!(!cas.has(&d).unwrap());
        assert_eq!(read_all(reader), b"in flight");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn iter_local_skips_tmp_and_junk() {
        let root = fresh();
        let cas = DiskCas::open(&root).unwrap();
        let d = cas.put(&mut &b"real"[..]).unwrap();
        std::fs::write(root.join("tmp/9-9"), b"x").unwrap();
        let junk = root.join("blake3/zz");
        std::fs::create_dir_all(&junk).unwrap();
        std::fs::write(junk.join("not-hex"), b"x").unwrap();
        let local = cas.iter_local().unwrap();
        assert_eq!(local.len(), 1);
        assert_eq!(local[0].digest, d);
        assert_eq!(local[0].size_bytes, 4);
        assert!(cas.as_reclaimable().is_some());
        let _ = std::fs::remove_dir_all(root);
    }
}
