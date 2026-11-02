//! Pins that survive a restart, persisted the way git persists refs (#137).
//!
//! The pin set is a [`Tree`] stored in the CAS itself (one entry per pin, named by the
//! pin's digest text), and `<[cas] path>/refs/roots` names that tree's digest on one
//! line. A change stores the new tree first, then replaces the ref by temp + rename, so
//! the ref never names a tree the store does not hold. The tree the ref names is pinned;
//! a superseded tree is an ordinary blob that `lru` reclaims. Pin changes and `lru`
//! eviction passes share one lock, so a pass never acts on a half-made change.

use std::collections::BTreeSet;
use std::fs::{self, File};
use std::io::{Cursor, Read, Write};
use std::path::Path;
use std::sync::MutexGuard;

use anyhow::Context;
use nessie_backend_core::{BackendError, CasBackend, Digest, Tree, TreeEntry, TreeKind};

use super::CasNode;

/// The ref file under `<[cas] path>/refs/`.
const REF: &str = "roots";

impl CasNode {
    /// Pin `root` (idempotent). With a disk store the pin is on disk before this returns.
    ///
    /// # Errors
    ///
    /// The root tree could not be stored ([`BackendError::CapacityExceeded`] in a full
    /// `write_only` node) or the ref could not be written: the pins are unchanged. Or
    /// the ref was published but its directory sync failed ("durability unconfirmed"):
    /// the pin is in force, and a retry confirms it.
    pub fn pin(&self, root: Digest) -> Result<(), BackendError> {
        let mut unsynced = self.lock_pins();
        self.apply_pins(&mut unsynced, |pins| pins.insert(root))
    }

    /// Unpin `root` (idempotent).
    ///
    /// # Errors
    ///
    /// As [`pin`](Self::pin).
    pub fn unpin(&self, root: &Digest) -> Result<(), BackendError> {
        let mut unsynced = self.lock_pins();
        self.apply_pins(&mut unsynced, |pins| pins.remove(root))
    }

    /// Write `expected`'s bytes (verified, as [`CasBackend::put_keyed`]) and pin them in
    /// one critical section, so no eviction pass runs between the write and the pin.
    ///
    /// # Errors
    ///
    /// As [`CasBackend::put_keyed`], then as [`pin`](Self::pin).
    pub fn put_and_pin(
        &self,
        expected: &Digest,
        source: &mut dyn Read,
    ) -> Result<(), BackendError> {
        let mut unsynced = self.lock_pins();
        self.put_keyed(expected, source)?;
        seam(self, "put-before-pin").map_err(|e| BackendError::Internal(e.to_string()))?;
        self.apply_pins(&mut unsynced, |pins| pins.insert(expected.clone()))
    }

    /// The pin lock: pin changes, pinning uploads and `lru` eviction passes run one at a
    /// time. The flag is true while a published ref's directory sync is unconfirmed.
    pub(super) fn lock_pins(&self) -> MutexGuard<'_, bool> {
        // ponytail: one global lock, so pins stall while an eviction pass runs; upgrade
        // to a roots generation check before each reclaim if that stall matters.
        self.pin_lock.lock().expect("pin lock poisoned")
    }

    /// Apply `change` to the pins; the caller holds [`lock_pins`](Self::lock_pins).
    fn apply_pins(
        &self,
        unsynced: &mut bool,
        change: impl FnOnce(&mut BTreeSet<Digest>) -> bool,
    ) -> Result<(), BackendError> {
        // ponytail: each pin rewrites the whole set (O(pins) bytes; per object under
        // `[s3] pin_on_put`). Upgrade: a fan-out Tree (`TreeKind::Dir` by digest
        // prefix) once pins reach ~10^5.
        let mut pins = self.roots.pins();
        if !change(&mut pins) {
            // Nothing new, but a retry after "durability unconfirmed" confirms it.
            return match &self.refs {
                Some(refs) if *unsynced => self.sync_refs(refs, unsynced),
                _ => Ok(()),
            };
        }
        let Some(refs) = &self.refs else {
            self.roots.replace(pins, None); // in-memory store: nothing to persist
            return Ok(());
        };
        // Local store only (no mirror); the in-flight guard protects it until the ref
        // names it.
        let tree = self
            .store
            .put(&mut Cursor::new(root_tree(&pins).to_canonical_bytes()))?;
        if let Err(e) = publish_ref(refs, &tree) {
            self.store.abandon(&tree);
            let path = refs.join(REF);
            return Err(BackendError::Internal(format!(
                "writing {}: {e}",
                path.display()
            )));
        }
        // The rename published the ref: memory follows it even if the sync below fails.
        self.roots.replace(pins, Some(tree.clone()));
        self.store.committed(&tree);
        *unsynced = true;
        self.sync_refs(refs, unsynced)
    }

    /// Sync `refs/` so the rename survives a power loss.
    fn sync_refs(&self, refs: &Path, unsynced: &mut bool) -> Result<(), BackendError> {
        seam(self, "dir-sync")
            .and_then(|()| File::open(refs)?.sync_all())
            .map_err(|e| {
                let path = refs.join(REF);
                BackendError::Internal(format!(
                    "{}: published, durability unconfirmed: {e}",
                    path.display()
                ))
            })?;
        *unsynced = false;
        Ok(())
    }
}

/// The pins and root tree the ref in `refs` names, read from `local` (no peer
/// fallback); none on first boot.
///
/// # Errors
///
/// An unreadable or unparseable ref, a tree this node does not hold or that fails
/// verification, or an entry whose name is not its digest. The node must not start:
/// starting with no pins would let `lru` evict what was pinned.
pub(super) fn load(
    refs: &Path,
    local: &dyn CasBackend,
) -> anyhow::Result<(BTreeSet<Digest>, Option<Digest>)> {
    let path = refs.join(REF);
    let text = match fs::read_to_string(&path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok((BTreeSet::new(), None)),
        read => read.with_context(|| format!("reading {}", path.display()))?,
    };
    read_tree(&text, local).with_context(|| {
        format!(
            "loading pins from {} (delete it to start with no pins)",
            path.display()
        )
    })
}

fn read_tree(
    text: &str,
    local: &dyn CasBackend,
) -> anyhow::Result<(BTreeSet<Digest>, Option<Digest>)> {
    let tree: Digest = text.strip_suffix('\n').unwrap_or(text).parse()?;
    let mut bytes = Vec::new();
    local.get(&tree)?.read_to_end(&mut bytes)?; // `get` verifies the digest
    let mut pins = BTreeSet::new();
    for (name, entry) in Tree::from_canonical_bytes(&bytes)?.entries {
        anyhow::ensure!(
            name == entry.digest.to_string(),
            "entry {name:?} names {}",
            entry.digest
        );
        pins.insert(entry.digest);
    }
    Ok((pins, Some(tree)))
}

fn root_tree(pins: &BTreeSet<Digest>) -> Tree {
    let entries = pins
        .iter()
        .map(|d| {
            let entry = TreeEntry {
                digest: d.clone(),
                kind: TreeKind::File {
                    is_executable: false,
                },
            };
            (d.to_string(), entry)
        })
        .collect();
    Tree { entries }
}

/// Replace the ref by temp + fsync + rename. After `Ok` the ref is published.
fn publish_ref(refs: &Path, tree: &Digest) -> std::io::Result<()> {
    fs::create_dir_all(refs)?;
    let tmp = refs.join("roots.tmp");
    let mut file = File::create(&tmp)?;
    writeln!(file, "{tree}")?;
    file.sync_all()?;
    fs::rename(&tmp, refs.join(REF))
}

/// A test-only pause or fault point (`dir-sync`, `evicting`, `put-before-pin`,
/// `scrub-corrupt`); a no-op outside unit tests.
pub(super) fn seam(node: &CasNode, at: &str) -> std::io::Result<()> {
    #[cfg(test)]
    {
        tests::SEAM.with(|s| s.borrow().as_ref().map_or(Ok(()), |f| f(node, at)))
    }
    #[cfg(not(test))]
    {
        let _ = (node, at);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::cell::{Cell, RefCell};
    use std::rc::Rc;

    use nessie_backend_core::{CasBackend as _, Digest, PeerId};

    use super::{CasNode, REF, load};
    use crate::config::{CasBackendKind, CasConfig, CasMode};

    type Hook = Box<dyn Fn(&CasNode, &str) -> std::io::Result<()>>;

    thread_local! {
        pub(super) static SEAM: RefCell<Option<Hook>> = const { RefCell::new(None) };
    }

    fn set_seam(hook: impl Fn(&CasNode, &str) -> std::io::Result<()> + 'static) {
        SEAM.with(|s| *s.borrow_mut() = Some(Box::new(hook)));
    }

    fn clear_seam() {
        SEAM.with(|s| *s.borrow_mut() = None);
    }

    /// A disk `lru` node with no budget and no grace: a pass evicts anything unpinned.
    fn lru(dir: &std::path::Path) -> CasConfig {
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

    /// Codex P1 #1 on #165: once `rename` succeeds the ref is published. A failed
    /// directory fsync must not leave memory (and eviction) on the old pins.
    #[test]
    fn a_dir_sync_failure_after_rename_keeps_memory_on_the_published_ref() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = lru(dir.path());
        let node = open(&cfg);
        let d = node.put(&mut &b"pinned"[..]).unwrap();
        set_seam(|_, at| match at {
            "dir-sync" => Err(std::io::Error::other("injected dir fsync failure")),
            _ => Ok(()),
        });
        let err = node.pin(d.clone()).expect_err("the dir fsync failed");
        let retry = node
            .pin(d.clone())
            .expect_err("a retry re-checks durability");
        clear_seam();
        assert!(
            retry.to_string().contains("durability unconfirmed"),
            "{retry}"
        );

        let refs = dir.path().join("refs");
        let (on_disk, tree) = load(&refs, node.local()).unwrap();
        assert_eq!(
            node.roots().pins(),
            on_disk,
            "memory follows the published ref"
        );
        node.maintain_once().unwrap();
        assert!(node.has(&d).unwrap(), "the pin protects its blob");
        assert!(
            node.has(&tree.unwrap()).unwrap(),
            "the published tree survives"
        );
        assert!(refs.join(REF).exists());
        assert!(err.to_string().contains("durability unconfirmed"), "{err}");
        node.pin(d.clone()).expect("a retry confirms durability");
        drop(node);
        assert!(open(&cfg).roots().pins().contains(&d), "and restarts");
    }

    /// Codex P1 #2 on #165: an eviction pass holds the pin lock, so no pin, unpin or
    /// pinning upload can interleave with its root and guard snapshots.
    #[test]
    fn an_eviction_pass_holds_the_pin_lock() {
        let dir = tempfile::tempdir().unwrap();
        let node = open(&lru(dir.path()));
        let d = node.put(&mut &b"pinned"[..]).unwrap();
        node.pin(d.clone()).unwrap();
        let held = Rc::new(Cell::new(false));
        let seen = held.clone();
        set_seam(move |node, at| {
            if at == "evicting" {
                seen.set(node.pin_lock.try_lock().is_err());
            }
            Ok(())
        });
        node.maintain_once().unwrap();
        clear_seam();
        assert!(held.get(), "a pin could run inside the eviction pass");
        assert!(node.has(&d).unwrap());
    }

    /// Codex P1 #3 on #165: between the verified write and its pin no eviction pass may
    /// run, or an acknowledged pinned upload is already gone.
    #[test]
    fn a_pinning_upload_cannot_be_evicted_before_its_pin() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = lru(dir.path());
        let node = open(&cfg);
        let bytes = b"kept by request";
        let d = Digest::compute(bytes);
        let held = Rc::new(Cell::new(false));
        let seen = held.clone();
        set_seam(move |node, at| {
            if at == "put-before-pin" {
                match node.pin_lock.try_lock() {
                    // Free: an eviction pass could run right here, so run one.
                    Ok(free) => {
                        drop(free);
                        node.maintain_once().map_err(std::io::Error::other)?;
                    }
                    Err(_) => seen.set(true),
                }
            }
            Ok(())
        });
        node.put_and_pin(&d, &mut &bytes[..]).unwrap();
        clear_seam();
        assert!(
            node.has(&d).unwrap(),
            "the acknowledged upload is still readable"
        );
        assert!(
            held.get(),
            "an eviction pass could run between the write and the pin"
        );
        node.maintain_once().unwrap();
        assert!(node.has(&d).unwrap(), "and survives the next pass");
        drop(node);
        let node = open(&cfg);
        node.maintain_once().unwrap();
        assert!(node.has(&d).unwrap() && node.roots().pins().contains(&d));
    }

    /// Codex P1 on #165 round 2: scrub's corrupt verdict and its removal must be atomic
    /// against a pinning re-put, or it deletes the bytes that re-put just repaired.
    #[test]
    fn scrub_keeps_a_blob_a_pinning_reput_repaired_after_its_verdict() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = CasConfig {
            byte_budget: Some(1 << 20), // eviction is not the subject here
            ..lru(dir.path())
        };
        let node = open(&cfg);
        let bytes = b"repaired by a pinning re-put";
        let d = Digest::compute(bytes);
        node.put_keyed(&d, &mut &bytes[..]).unwrap();
        let hex = d.to_string().rsplit_once(':').unwrap().1.to_owned();
        let file = dir.path().join("blake3").join(&hex[..2]).join(&hex[2..]);
        std::fs::write(&file, b"bit rot").unwrap();

        set_seam(move |node, at| {
            if at == "scrub-corrupt" {
                node.put_and_pin(&d.clone(), &mut &bytes[..])
                    .map_err(std::io::Error::other)?;
            }
            Ok(())
        });
        let report = node.maintain_once().unwrap();
        clear_seam();

        let d = Digest::compute(bytes);
        assert_eq!(report.scrub().corrupt, 1, "scrub saw the bit rot");
        assert!(node.has(&d).unwrap(), "the repaired, pinned blob was kept");
        let mut back = Vec::new();
        std::io::Read::read_to_end(&mut node.get(&d).unwrap(), &mut back).unwrap();
        assert_eq!(back, bytes);
        drop(node);
        let node = open(&cfg);
        assert!(node.has(&d).unwrap() && node.roots().pins().contains(&d));
    }
}
