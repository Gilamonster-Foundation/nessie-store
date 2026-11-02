//! Pins that survive a restart, persisted the way git persists refs (#137).
//!
//! The pin set is a [`Tree`] stored in the CAS itself (one entry per pin, named by the
//! pin's digest text), and `<[cas] path>/refs/roots` names that tree's digest on one
//! line. A change stores the new tree first, then replaces the ref by temp + rename, so
//! the ref never names a tree the store does not hold. The tree the ref names is pinned;
//! a superseded tree is an ordinary blob that `lru` reclaims.

use std::collections::BTreeSet;
use std::fs::{self, File};
use std::io::{Cursor, Read, Write};
use std::path::Path;

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
    /// `write_only` node) or the ref could not be written. The pins are then unchanged.
    pub fn pin(&self, root: Digest) -> Result<(), BackendError> {
        self.update_pins(|pins| pins.insert(root))
    }

    /// Unpin `root` (idempotent).
    ///
    /// # Errors
    ///
    /// As [`pin`](Self::pin).
    pub fn unpin(&self, root: &Digest) -> Result<(), BackendError> {
        self.update_pins(|pins| pins.remove(root))
    }

    fn update_pins(
        &self,
        change: impl FnOnce(&mut BTreeSet<Digest>) -> bool,
    ) -> Result<(), BackendError> {
        // ponytail: one writer at a time, and each pin rewrites the whole set (O(pins)
        // bytes; per object under `[s3] pin_on_put`). Upgrade: a fan-out Tree
        // (`TreeKind::Dir` by digest prefix) once pins reach ~10^5.
        let _one_writer = self.pin_lock.lock().expect("pin lock poisoned");
        let mut pins = self.roots.pins();
        if !change(&mut pins) {
            return Ok(());
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
        if let Err(e) = write_ref(refs, &tree) {
            self.store.abandon(&tree);
            let path = refs.join(REF);
            return Err(BackendError::Internal(format!(
                "writing {}: {e}",
                path.display()
            )));
        }
        self.roots.replace(pins, Some(tree.clone()));
        self.store.committed(&tree);
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

fn write_ref(refs: &Path, tree: &Digest) -> std::io::Result<()> {
    fs::create_dir_all(refs)?;
    let tmp = refs.join("roots.tmp");
    let mut file = File::create(&tmp)?;
    writeln!(file, "{tree}")?;
    file.sync_all()?;
    fs::rename(&tmp, refs.join(REF))?;
    File::open(refs)?.sync_all() // the rename itself survives a power loss
}
