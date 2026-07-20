//! What a deterministic action produced, and its canonical content-addressed
//! encoding.
//!
//! An [`ActionResult`] is stored as a CAS blob under *its own* digest
//! ([`ActionResult::result_digest`]); an attestation names that digest, never the
//! struct inline. Two honest executors of the same deterministic action produce
//! **byte-identical** canonical bytes and therefore the same digest — the
//! determinism premise on which k-of-n agreement rests (`formal/README.md`,
//! "Action determinism"). Maps to the deterministic subset of Bazel REAPI v2
//! `build.bazel.remote.execution.v2.ActionResult`.

use crate::digest::Digest;
use crate::error::BackendError;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// One output artifact: a CAS blob digest plus the REAPI-visible executable bit.
/// The workspace-relative path is the [`ActionResult::outputs`] map key, not a
/// field here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutputFile {
    /// CAS digest of the file contents (a blob in the same `CasBackend`).
    pub digest: Digest,
    /// REAPI `OutputFile.is_executable`.
    pub is_executable: bool,
}

/// What a deterministic action produced.
///
/// `outputs` is a `BTreeMap` so the serialized/canonical form is order-independent
/// *by construction* — no hand-maintained sort invariant to break.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActionResult {
    /// Output artifacts, keyed by workspace-relative path (sorted by `BTreeMap`).
    pub outputs: BTreeMap<String, OutputFile>,
    /// Process exit code (REAPI `exit_code`); 0 = success.
    pub exit_code: i32,
    /// CAS digest of captured stdout, if any (REAPI `stdout_digest`; body in CAS).
    pub stdout_digest: Option<Digest>,
    /// CAS digest of captured stderr, if any (REAPI `stderr_digest`; body in CAS).
    pub stderr_digest: Option<Digest>,
}

/// Domain tag prefixing the canonical encoding — separates these bytes from any
/// other blob and pins the encoding version. A frozen wire contract.
const AR_DOMAIN: &[u8] = b"nessie.ActionResult.v1\x00";

impl ActionResult {
    /// The digest this result is stored and attested under: `Digest::compute` of
    /// the [canonical bytes](ActionResult::to_canonical_bytes). Byte-identical
    /// results share it; any difference surfaces as a *different* digest, so
    /// non-determinism becomes a disagreement the k-of-n gate can see.
    #[must_use]
    pub fn result_digest(&self) -> Digest {
        Digest::compute(&self.to_canonical_bytes())
    }

    /// The deterministic, domain-separated, length-delimited encoding — the exact
    /// bytes stored in CAS and hashed for [`result_digest`](ActionResult::result_digest).
    ///
    /// Layout (all lengths/ints little-endian, fixed width): `AR_DOMAIN`,
    /// `exit_code: i32`, `outputs.len(): u64`, then each entry in `BTreeMap` key
    /// order as `path` (len-delimited) · `digest` multihash (len-delimited) ·
    /// `is_executable: u8`, then `stdout_digest` and `stderr_digest` as optional
    /// (`0` = none; `1` + len-delimited multihash). Frozen wire contract.
    #[must_use]
    pub fn to_canonical_bytes(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(AR_DOMAIN);
        out.extend_from_slice(&self.exit_code.to_le_bytes());
        out.extend_from_slice(&(self.outputs.len() as u64).to_le_bytes());
        for (path, file) in &self.outputs {
            write_delimited(&mut out, path.as_bytes());
            write_delimited(&mut out, &file.digest.to_multihash_bytes());
            out.push(u8::from(file.is_executable));
        }
        write_opt_digest(&mut out, self.stdout_digest.as_ref());
        write_opt_digest(&mut out, self.stderr_digest.as_ref());
        out
    }

    /// Inverse of [`to_canonical_bytes`](ActionResult::to_canonical_bytes), used by
    /// `get_action_result` to materialize a confirmed result fetched from CAS.
    ///
    /// # Errors
    ///
    /// [`BackendError::InvalidArgument`] if the bytes are not a well-formed
    /// *canonical* `ActionResult` — wrong domain tag, truncated, trailing bytes, a
    /// bad embedded digest, or output paths that are not strictly ascending
    /// (out-of-order or duplicate). Decode is a canonical validator: only the one
    /// byte form `to_canonical_bytes` would emit is accepted.
    pub fn from_canonical_bytes(bytes: &[u8]) -> Result<Self, BackendError> {
        let mut r = Reader::new(bytes);
        r.expect_tag(AR_DOMAIN)?;
        let exit_code = r.read_i32()?;
        let n = r.read_u64()?;
        let mut outputs = BTreeMap::new();
        let mut prev_path: Option<String> = None;
        for _ in 0..n {
            let path =
                String::from_utf8(r.read_delimited()?.to_vec()).map_err(|_| bad("output path"))?;
            // Enforce STRICT ascending path order — the canonical form is the
            // `BTreeMap` iteration order. This makes decode a canonical *validator*,
            // not a permissive parser: an out-of-order (or duplicate) encoding is
            // rejected rather than silently re-canonicalized, so decode∘encode is
            // idempotent and the byte form is truly 1:1 with the value.
            if prev_path.as_ref().is_some_and(|p| &path <= p) {
                return Err(bad("outputs not in canonical ascending order"));
            }
            let digest = r.read_digest()?;
            let is_executable = r.read_bool()?;
            outputs.insert(
                path.clone(),
                OutputFile {
                    digest,
                    is_executable,
                },
            );
            prev_path = Some(path);
        }
        let stdout_digest = r.read_opt_digest()?;
        let stderr_digest = r.read_opt_digest()?;
        r.expect_end()?;
        Ok(Self {
            outputs,
            exit_code,
            stdout_digest,
            stderr_digest,
        })
    }
}

fn bad(what: &str) -> BackendError {
    BackendError::InvalidArgument(format!("malformed ActionResult: {what}"))
}

fn write_delimited(out: &mut Vec<u8>, bytes: &[u8]) {
    out.extend_from_slice(&(bytes.len() as u64).to_le_bytes());
    out.extend_from_slice(bytes);
}

fn write_opt_digest(out: &mut Vec<u8>, digest: Option<&Digest>) {
    match digest {
        None => out.push(0),
        Some(d) => {
            out.push(1);
            write_delimited(out, &d.to_multihash_bytes());
        }
    }
}

/// A cursor over the canonical byte buffer. Every read is bounds-checked and maps
/// a shortfall to [`BackendError::InvalidArgument`], so a truncated or hostile
/// blob is a clean error, never a panic.
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

    fn read_i32(&mut self) -> Result<i32, BackendError> {
        let b: [u8; 4] = self.take(4)?.try_into().expect("took 4 bytes");
        Ok(i32::from_le_bytes(b))
    }

    fn read_u64(&mut self) -> Result<u64, BackendError> {
        let b: [u8; 8] = self.take(8)?.try_into().expect("took 8 bytes");
        Ok(u64::from_le_bytes(b))
    }

    fn read_bool(&mut self) -> Result<bool, BackendError> {
        match self.take(1)?[0] {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(bad("bool not 0/1")),
        }
    }

    fn read_delimited(&mut self) -> Result<&'a [u8], BackendError> {
        let len = self.read_u64()?;
        let len = usize::try_from(len).map_err(|_| bad("length too large"))?;
        self.take(len)
    }

    fn read_digest(&mut self) -> Result<Digest, BackendError> {
        let mh = self.read_delimited()?;
        Digest::from_multihash_bytes(mh).map_err(|_| bad("bad embedded digest"))
    }

    fn read_opt_digest(&mut self) -> Result<Option<Digest>, BackendError> {
        match self.take(1)?[0] {
            0 => Ok(None),
            1 => Ok(Some(self.read_digest()?)),
            _ => Err(bad("option tag not 0/1")),
        }
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

    fn sample() -> ActionResult {
        let mut outputs = BTreeMap::new();
        outputs.insert(
            "bin/app".to_string(),
            OutputFile {
                digest: Digest::compute(b"app-bytes"),
                is_executable: true,
            },
        );
        outputs.insert(
            "README".to_string(),
            OutputFile {
                digest: Digest::compute(b"readme"),
                is_executable: false,
            },
        );
        ActionResult {
            outputs,
            exit_code: 0,
            stdout_digest: Some(Digest::compute(b"stdout")),
            stderr_digest: None,
        }
    }

    #[test]
    fn canonical_bytes_roundtrip() {
        let r = sample();
        let bytes = r.to_canonical_bytes();
        let back = ActionResult::from_canonical_bytes(&bytes).expect("decode");
        assert_eq!(r, back);
    }

    #[test]
    fn result_digest_is_deterministic_and_order_independent() {
        // Two ActionResults built by inserting the same entries in a different
        // order must be equal and share a digest (BTreeMap canonicalizes).
        let a = sample();
        let mut b_outputs = BTreeMap::new();
        // Insert in the opposite order.
        b_outputs.insert(
            "bin/app".to_string(),
            OutputFile {
                digest: Digest::compute(b"app-bytes"),
                is_executable: true,
            },
        );
        b_outputs.insert(
            "README".to_string(),
            OutputFile {
                digest: Digest::compute(b"readme"),
                is_executable: false,
            },
        );
        let b = ActionResult {
            outputs: b_outputs,
            exit_code: 0,
            stdout_digest: Some(Digest::compute(b"stdout")),
            stderr_digest: None,
        };
        assert_eq!(a.result_digest(), b.result_digest());
    }

    #[test]
    fn a_field_change_changes_the_digest() {
        let mut r = sample();
        let before = r.result_digest();
        r.exit_code = 1;
        assert_ne!(before, r.result_digest());
    }

    #[test]
    fn empty_result_roundtrips() {
        let r = ActionResult {
            outputs: BTreeMap::new(),
            exit_code: 0,
            stdout_digest: None,
            stderr_digest: None,
        };
        let back = ActionResult::from_canonical_bytes(&r.to_canonical_bytes()).expect("decode");
        assert_eq!(r, back);
    }

    #[test]
    fn wrong_domain_tag_is_rejected() {
        let err = ActionResult::from_canonical_bytes(b"not-a-result").unwrap_err();
        assert!(matches!(err, BackendError::InvalidArgument(_)));
    }

    /// Regression (adversarial review 2026-07-20): decode must REJECT a
    /// non-canonical (descending / out-of-order) output ordering, so decode is a
    /// canonical validator and `decode∘encode` is idempotent — not a permissive
    /// parser that silently re-sorts a distinct blob into the same value.
    fn assemble(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(AR_DOMAIN);
        b.extend_from_slice(&0i32.to_le_bytes());
        b.extend_from_slice(&(entries.len() as u64).to_le_bytes());
        for (path, blob) in entries {
            write_delimited(&mut b, path.as_bytes());
            write_delimited(&mut b, &Digest::compute(blob).to_multihash_bytes());
            b.push(0);
        }
        write_opt_digest(&mut b, None);
        write_opt_digest(&mut b, None);
        b
    }

    #[test]
    fn out_of_order_outputs_are_rejected_but_ascending_decodes() {
        // Same two entries, descending key order — must be rejected.
        let descending = assemble(&[("b", b"bb"), ("a", b"aa")]);
        assert!(ActionResult::from_canonical_bytes(&descending).is_err());
        // Ascending (canonical) order decodes cleanly.
        let ascending = assemble(&[("a", b"aa"), ("b", b"bb")]);
        assert!(ActionResult::from_canonical_bytes(&ascending).is_ok());
        // A duplicate key (non-strictly-increasing) is also rejected.
        let duplicate = assemble(&[("a", b"aa"), ("a", b"aa")]);
        assert!(ActionResult::from_canonical_bytes(&duplicate).is_err());
    }

    #[test]
    fn trailing_bytes_are_rejected() {
        let mut bytes = sample().to_canonical_bytes();
        bytes.push(0xff);
        assert!(ActionResult::from_canonical_bytes(&bytes).is_err());
    }

    #[test]
    fn truncation_is_rejected_not_panicked() {
        let bytes = sample().to_canonical_bytes();
        for cut in 0..bytes.len() {
            // Every prefix is either a clean decode error or (only at full length) ok.
            let _ = ActionResult::from_canonical_bytes(&bytes[..cut]);
        }
        assert!(ActionResult::from_canonical_bytes(&bytes[..bytes.len() - 1]).is_err());
    }
}
