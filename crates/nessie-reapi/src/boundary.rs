//! The SHA-256 digest boundary — the one place the algorithm pin lives.
//!
//! A REAPI instance is **SHA-256-native**: client-facing blobs are keyed under
//! [`DigestAlgo::Sha256`], so the boundary is a pure, stateless *shape* transform
//! rather than a hash translation — no index, no recompute, no second source of
//! truth. `reapi::Digest{hash, size_bytes}` ↔ `(native Digest{algo:Sha256}, size)`.
//! `size_bytes` rides alongside the native digest, never inside it (a content digest
//! carries no length).

use crate::reapi;
use nessie_backend_core::{Digest, DigestAlgo};
use tonic::Status;

/// The stateless SHA-256 shape transform between REAPI and native digests.
#[derive(Debug, Clone, Copy, Default)]
pub struct Sha256Boundary;

impl Sha256Boundary {
    /// Convert a REAPI `Digest` to a native [`Digest`]. Validates the hash is 64
    /// lowercase-hex characters, then parses `sha2-256:{hash}` (reusing the merged
    /// `FromStr`, which length-checks the 32 bytes).
    ///
    /// # Errors
    ///
    /// [`tonic::Status::invalid_argument`] if the hash is not 64 lowercase-hex chars.
    pub fn to_native(&self, r: &reapi::Digest) -> Result<Digest, Status> {
        if r.hash.len() != 64 || !r.hash.bytes().all(is_lower_hex) {
            return Err(Status::invalid_argument(format!(
                "REAPI digest hash must be 64 lowercase-hex characters, got {:?}",
                r.hash
            )));
        }
        format!("sha2-256:{}", r.hash)
            .parse::<Digest>()
            .map_err(|e| Status::invalid_argument(format!("invalid sha2-256 digest: {e}")))
    }

    /// Convert a native SHA-256 [`Digest`] plus its byte `size` to a REAPI `Digest`.
    /// The REAPI hash *is* the digest's bytes in lowercase hex — no recompute.
    #[must_use]
    pub fn to_reapi(&self, digest: &Digest, size: u64) -> reapi::Digest {
        debug_assert_eq!(
            digest.algo(),
            DigestAlgo::Sha256,
            "a SHA-256-native instance must only emit SHA-256 digests"
        );
        reapi::Digest {
            hash: hex_lower(digest.as_bytes()),
            size_bytes: size as i64,
        }
    }

    /// The digest function this instance advertises and keys under.
    #[must_use]
    pub fn digest_function(&self) -> reapi::digest_function::Value {
        reapi::digest_function::Value::Sha256
    }
}

fn is_lower_hex(b: u8) -> bool {
    b.is_ascii_digit() || (b'a'..=b'f').contains(&b)
}

fn hex_lower(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        s.push(HEX[(b >> 4) as usize] as char);
        s.push(HEX[(b & 0x0f) as usize] as char);
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_empty_vector_round_trips() {
        // SHA-256("") — the canonical known vector.
        let hash = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
        let b = Sha256Boundary;
        let native = b
            .to_native(&reapi::Digest {
                hash: hash.to_string(),
                size_bytes: 0,
            })
            .expect("valid sha256 digest");
        assert_eq!(native.algo(), DigestAlgo::Sha256);
        // Emitting it back yields the same hash, with the size we supply.
        let back = b.to_reapi(&native, 0);
        assert_eq!(back.hash, hash);
        assert_eq!(back.size_bytes, 0);
        // And it matches computing SHA-256 of the empty input natively.
        assert_eq!(native, Digest::compute_with(DigestAlgo::Sha256, b""));
    }

    #[test]
    fn to_native_rejects_bad_hashes() {
        let b = Sha256Boundary;
        for bad in ["", "abcd", &"z".repeat(64), &"A".repeat(64)] {
            assert!(
                b.to_native(&reapi::Digest {
                    hash: bad.to_string(),
                    size_bytes: 0,
                })
                .is_err(),
                "must reject {bad:?}"
            );
        }
    }

    #[test]
    fn digest_function_is_sha256() {
        assert_eq!(
            Sha256Boundary.digest_function(),
            reapi::digest_function::Value::Sha256
        );
    }
}
