//! Merkle tree objects — the directory nodes of the content-addressed DAG.
//!
//! A [`Tree`] is a structured CAS blob mapping names to child entries, each either a
//! file blob or a sub-[`Tree`]. It is git's tree object and REAPI's `Directory`: the
//! interior nodes that make the CAS a real Merkle DAG rather than a flat blob store.
//!
//! Two things depend on it. **Reachability GC**: a tree's [`Referenced::references`]
//! are its children, so `reachable_closure` walks `root → tree → sub-tree → blob`
//! transitively (without it, a tree reads as an opaque leaf and its children would
//! look unreachable). **The REAPI face**: `ActionResult.output_directories` name a
//! `Tree` by digest.
//!
//! Like [`ActionResult`](crate::ActionResult), a `Tree` has a deterministic,
//! canonical, content-addressed encoding: two identical trees hash to the same
//! [`Digest`], and decode is a strict canonical *validator* (entries strictly
//! ascending by name).

use crate::digest::Digest;
use crate::error::BackendError;
use crate::references::Referenced;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// A child of a [`Tree`]: the digest it points at, and whether that digest names a
/// file blob or a sub-tree.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TreeEntry {
    /// The child's content digest — a file blob, or a sub-tree's [`Tree::tree_digest`].
    pub digest: Digest,
    /// Whether `digest` names a file or a directory.
    pub kind: TreeKind,
}

/// What a [`TreeEntry`] points at.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum TreeKind {
    /// A file blob, with the REAPI-visible executable bit.
    File {
        /// REAPI `is_executable`.
        is_executable: bool,
    },
    /// A sub-directory: `digest` is that sub-tree's `tree_digest`.
    Dir,
}

/// A directory node: names → child entries, sorted canonically by name.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tree {
    /// Entries keyed by name (a single path component). `BTreeMap` fixes the
    /// canonical order so the encoding is order-independent by construction.
    pub entries: BTreeMap<String, TreeEntry>,
}

/// Domain tag prefixing the canonical encoding. A frozen wire contract.
const TREE_DOMAIN: &[u8] = b"nessie.Tree.v1\x00";

impl Tree {
    /// An empty tree.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The digest this tree is stored and referenced under: `Digest::compute` of its
    /// [canonical bytes](Tree::to_canonical_bytes).
    #[must_use]
    pub fn tree_digest(&self) -> Digest {
        Digest::compute(&self.to_canonical_bytes())
    }

    /// The deterministic, domain-separated, length-delimited canonical encoding.
    ///
    /// Layout: `TREE_DOMAIN`, `entries.len(): u64`, then each entry in `BTreeMap`
    /// key order as `name` (len-delimited) · `kind` (`0` = file + an executable
    /// byte, `1` = dir) · `digest` multihash (len-delimited). A frozen wire contract.
    #[must_use]
    pub fn to_canonical_bytes(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(TREE_DOMAIN);
        out.extend_from_slice(&(self.entries.len() as u64).to_le_bytes());
        for (name, entry) in &self.entries {
            write_delimited(&mut out, name.as_bytes());
            match entry.kind {
                TreeKind::File { is_executable } => {
                    out.push(0);
                    out.push(u8::from(is_executable));
                }
                TreeKind::Dir => out.push(1),
            }
            write_delimited(&mut out, &entry.digest.to_multihash_bytes());
        }
        out
    }

    /// Inverse of [`to_canonical_bytes`](Tree::to_canonical_bytes).
    ///
    /// # Errors
    ///
    /// [`BackendError::InvalidArgument`] if the bytes are not a well-formed canonical
    /// `Tree` (wrong domain tag, truncated, trailing bytes, a bad embedded digest, a
    /// bad kind tag, or names not strictly ascending).
    pub fn from_canonical_bytes(bytes: &[u8]) -> Result<Self, BackendError> {
        let mut r = Reader::new(bytes);
        r.expect_tag(TREE_DOMAIN)?;
        let n = r.read_u64()?;
        let mut entries = BTreeMap::new();
        let mut prev: Option<String> = None;
        for _ in 0..n {
            let name =
                String::from_utf8(r.read_delimited()?.to_vec()).map_err(|_| bad("entry name"))?;
            // Strict ascending order = canonical validator (see ActionResult).
            if prev.as_ref().is_some_and(|p| &name <= p) {
                return Err(bad("entries not in canonical ascending order"));
            }
            let kind = match r.read_u8()? {
                0 => TreeKind::File {
                    is_executable: r.read_bool()?,
                },
                1 => TreeKind::Dir,
                _ => return Err(bad("unknown tree entry kind")),
            };
            let digest = r.read_digest()?;
            entries.insert(name.clone(), TreeEntry { digest, kind });
            prev = Some(name);
        }
        r.expect_end()?;
        Ok(Self { entries })
    }
}

impl Referenced for Tree {
    /// The tree's children: every entry's digest (file blobs and sub-trees alike).
    /// This is what makes reachability transitive over directory structure.
    fn references(&self) -> Vec<Digest> {
        self.entries.values().map(|e| e.digest.clone()).collect()
    }
}

fn bad(what: &str) -> BackendError {
    BackendError::InvalidArgument(format!("malformed Tree: {what}"))
}

fn write_delimited(out: &mut Vec<u8>, bytes: &[u8]) {
    out.extend_from_slice(&(bytes.len() as u64).to_le_bytes());
    out.extend_from_slice(bytes);
}

/// A bounds-checked cursor over the canonical bytes (a truncated/hostile blob is a
/// clean error, never a panic).
struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], BackendError> {
        let end = self
            .pos
            .checked_add(n)
            .ok_or_else(|| bad("length overflow"))?;
        let slice = self
            .buf
            .get(self.pos..end)
            .ok_or_else(|| bad("truncated"))?;
        self.pos = end;
        Ok(slice)
    }

    fn expect_tag(&mut self, tag: &[u8]) -> Result<(), BackendError> {
        if self.take(tag.len())? == tag {
            Ok(())
        } else {
            Err(bad("wrong domain tag"))
        }
    }

    fn read_u8(&mut self) -> Result<u8, BackendError> {
        Ok(self.take(1)?[0])
    }

    fn read_bool(&mut self) -> Result<bool, BackendError> {
        match self.read_u8()? {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(bad("bool not 0/1")),
        }
    }

    fn read_u64(&mut self) -> Result<u64, BackendError> {
        let b: [u8; 8] = self.take(8)?.try_into().expect("took 8 bytes");
        Ok(u64::from_le_bytes(b))
    }

    fn read_delimited(&mut self) -> Result<&'a [u8], BackendError> {
        let len = self.read_u64()?;
        let len = usize::try_from(len).map_err(|_| bad("length too large"))?;
        self.take(len)
    }

    fn read_digest(&mut self) -> Result<Digest, BackendError> {
        Digest::from_multihash_bytes(self.read_delimited()?).map_err(|_| bad("bad embedded digest"))
    }

    fn expect_end(&self) -> Result<(), BackendError> {
        if self.pos == self.buf.len() {
            Ok(())
        } else {
            Err(bad("trailing bytes"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Tree {
        let mut entries = BTreeMap::new();
        entries.insert(
            "bin".to_string(),
            TreeEntry {
                digest: Digest::compute(b"a-subtree"),
                kind: TreeKind::Dir,
            },
        );
        entries.insert(
            "run.sh".to_string(),
            TreeEntry {
                digest: Digest::compute(b"script"),
                kind: TreeKind::File {
                    is_executable: true,
                },
            },
        );
        entries.insert(
            "README".to_string(),
            TreeEntry {
                digest: Digest::compute(b"readme"),
                kind: TreeKind::File {
                    is_executable: false,
                },
            },
        );
        Tree { entries }
    }

    #[test]
    fn canonical_roundtrips() {
        let t = sample();
        let back = Tree::from_canonical_bytes(&t.to_canonical_bytes()).expect("decode");
        assert_eq!(t, back);
    }

    #[test]
    fn digest_is_order_independent() {
        // Insert the same entries in a different order — same canonical digest.
        let a = sample();
        let mut b_entries = BTreeMap::new();
        b_entries.insert("README".to_string(), a.entries["README"].clone());
        b_entries.insert("run.sh".to_string(), a.entries["run.sh"].clone());
        b_entries.insert("bin".to_string(), a.entries["bin"].clone());
        let b = Tree { entries: b_entries };
        assert_eq!(a.tree_digest(), b.tree_digest());
    }

    #[test]
    fn references_are_all_child_digests() {
        let t = sample();
        let refs = t.references();
        assert_eq!(refs.len(), 3);
        assert!(refs.contains(&Digest::compute(b"a-subtree")));
        assert!(refs.contains(&Digest::compute(b"script")));
        assert!(refs.contains(&Digest::compute(b"readme")));
    }

    #[test]
    fn empty_tree_roundtrips_and_has_no_references() {
        let t = Tree::new();
        assert!(t.references().is_empty());
        assert_eq!(
            Tree::from_canonical_bytes(&t.to_canonical_bytes()).unwrap(),
            t
        );
    }

    #[test]
    fn a_change_changes_the_digest() {
        let mut t = sample();
        let before = t.tree_digest();
        t.entries.get_mut("run.sh").unwrap().kind = TreeKind::File {
            is_executable: false,
        };
        assert_ne!(before, t.tree_digest());
    }

    #[test]
    fn wrong_domain_and_truncation_are_rejected() {
        assert!(Tree::from_canonical_bytes(b"not-a-tree").is_err());
        let bytes = sample().to_canonical_bytes();
        assert!(Tree::from_canonical_bytes(&bytes[..bytes.len() - 1]).is_err());
        let mut trailing = bytes.clone();
        trailing.push(0xff);
        assert!(Tree::from_canonical_bytes(&trailing).is_err());
    }
}
