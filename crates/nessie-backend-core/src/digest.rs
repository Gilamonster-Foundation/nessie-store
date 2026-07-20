//! Content digests — the identity of a blob in the content-addressed substrate.
//!
//! A [`Digest`] is a hash of bytes *plus the algorithm that produced it*. It is
//! the key in the content-addressed store (CAS): location-independent, so any
//! peer holding the bytes can serve them, and self-verifying, so a reader proves
//! the bytes match the key with [`Digest::verify`]. This is the P2P-native data
//! model — see `docs/design/p2p-cas-swarm.md`.
//!
//! # Self-describing
//!
//! The algorithm travels *with* every digest, in both its text form
//! (`"blake3:<hex>"`) and its multihash byte form
//! (`<multicodec><len><digest>`). A peer never needs out-of-band agreement on
//! which hash function was used. This is also what lets the compute default
//! **forward-ratchet**: a new algorithm is added to [`DigestAlgo`] and becomes
//! the default without a flag day, because every existing digest still names its
//! own algorithm. (Workspace law: *multihash over specific algos* — identifiers
//! self-describe; *freeze minimally* — today's BLAKE3 pin rotates via a ratchet,
//! never a flag day.)
//!
//! The type is genuinely multi-hash, never one-hash-baked-in: BLAKE3 is only the
//! *default* the compute path selects, and SHA-256 is a first-class peer in
//! [`DigestAlgo`] (the REAPI face's wire contract pins it). Nothing downstream may
//! assume a single algorithm — a `Digest` always carries its own.

use serde::{Deserialize, Serialize};
use sha2::Digest as _;
use std::fmt;
use std::str::FromStr;

/// The hash algorithm behind a [`Digest`].
///
/// The default ([`DigestAlgo::DEFAULT`]) is BLAKE3-256. This enum is the pin that
/// forward-ratchets: growing it adds a new algorithm without invalidating any
/// existing digest, since each digest names its own algorithm.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum DigestAlgo {
    /// BLAKE3 with 256-bit (32-byte) output — the default content-address function.
    Blake3,
    /// SHA-256 (32-byte output). Not the native default; present because the REAPI
    /// face's wire contract pins it, so the type must speak it as a first-class peer.
    Sha256,
}

impl DigestAlgo {
    /// The algorithm [`Digest::compute`] uses. BLAKE3 today; this is the pin that
    /// forward-ratchets to a stronger default without a flag day.
    pub const DEFAULT: DigestAlgo = DigestAlgo::Blake3;

    /// The multicodec code identifying this algorithm in the multihash byte form.
    ///
    /// `0x1e` is the registered multicodec for BLAKE3.
    #[must_use]
    pub const fn multicodec(self) -> u8 {
        match self {
            DigestAlgo::Blake3 => 0x1e,
            DigestAlgo::Sha256 => 0x12,
        }
    }

    /// The length in bytes of this algorithm's digest output.
    #[must_use]
    pub const fn digest_len(self) -> usize {
        match self {
            DigestAlgo::Blake3 | DigestAlgo::Sha256 => 32,
        }
    }

    /// The lowercase text name used in the `"<algo>:<hex>"` string form.
    ///
    /// Names follow the multiformats registry (`blake3`, `sha2-256`).
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            DigestAlgo::Blake3 => "blake3",
            DigestAlgo::Sha256 => "sha2-256",
        }
    }

    fn from_multicodec(code: u8) -> Option<Self> {
        match code {
            0x1e => Some(DigestAlgo::Blake3),
            0x12 => Some(DigestAlgo::Sha256),
            _ => None,
        }
    }

    fn from_name(name: &str) -> Option<Self> {
        match name {
            "blake3" => Some(DigestAlgo::Blake3),
            "sha2-256" => Some(DigestAlgo::Sha256),
            _ => None,
        }
    }
}

/// The content-addressed identity of a blob: a hash plus the algorithm that
/// produced it.
///
/// Constructed by hashing bytes ([`Digest::compute`]), never supplied blindly —
/// the store computes the key from the content. Two honest nodes hashing the same
/// bytes always produce the same `Digest`, which is exactly why a digest can be a
/// location-independent name in the swarm.
///
/// Serializes as its text form (`"blake3:<hex>"`), keeping wire and registry
/// shapes plain text — the same discipline the UUID newtypes follow.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Digest {
    algo: DigestAlgo,
    bytes: Vec<u8>,
}

impl Digest {
    /// Compute the digest of `data` with the default algorithm
    /// ([`DigestAlgo::DEFAULT`]).
    #[must_use]
    pub fn compute(data: &[u8]) -> Self {
        Self::compute_with(DigestAlgo::DEFAULT, data)
    }

    /// Compute the digest of `data` with an explicit algorithm.
    #[must_use]
    pub fn compute_with(algo: DigestAlgo, data: &[u8]) -> Self {
        let bytes = match algo {
            DigestAlgo::Blake3 => blake3::hash(data).as_bytes().to_vec(),
            DigestAlgo::Sha256 => sha2::Sha256::digest(data).to_vec(),
        };
        Self { algo, bytes }
    }

    /// True iff `data` hashes (under this digest's own algorithm) to this digest.
    ///
    /// This is the self-verification a CAS read performs before trusting bytes
    /// fetched from an untrusted peer.
    #[must_use]
    pub fn verify(&self, data: &[u8]) -> bool {
        Self::compute_with(self.algo, data) == *self
    }

    /// The algorithm that produced this digest.
    #[must_use]
    pub fn algo(&self) -> DigestAlgo {
        self.algo
    }

    /// The raw hash bytes (without the algorithm tag).
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// The self-describing multihash byte form: `<multicodec><len><digest bytes>`.
    ///
    /// This is the wire/DHT form. All current algorithms have a single-byte
    /// multicodec and a digest length below 128, so each field is one byte; a
    /// full unsigned-varint encoding is deferred until an algorithm needs it.
    #[must_use]
    pub fn to_multihash_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(2 + self.bytes.len());
        out.push(self.algo.multicodec());
        out.push(self.algo.digest_len() as u8);
        out.extend_from_slice(&self.bytes);
        out
    }

    /// Parse the self-describing multihash byte form produced by
    /// [`Digest::to_multihash_bytes`].
    ///
    /// # Errors
    ///
    /// Returns [`DigestParseError`] if the input is truncated, names an unknown
    /// multicodec, or carries a length that disagrees with the algorithm.
    pub fn from_multihash_bytes(input: &[u8]) -> Result<Self, DigestParseError> {
        let &code = input.first().ok_or(DigestParseError::Truncated)?;
        let &len = input.get(1).ok_or(DigestParseError::Truncated)?;
        let algo =
            DigestAlgo::from_multicodec(code).ok_or(DigestParseError::UnknownMulticodec(code))?;
        let digest = input.get(2..).ok_or(DigestParseError::Truncated)?;
        if digest.len() != len as usize {
            return Err(DigestParseError::Truncated);
        }
        if digest.len() != algo.digest_len() {
            return Err(DigestParseError::WrongLength {
                expected: algo.digest_len(),
                actual: digest.len(),
            });
        }
        Ok(Self {
            algo,
            bytes: digest.to_vec(),
        })
    }
}

impl fmt::Display for Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.algo.name(), to_hex(&self.bytes))
    }
}

impl FromStr for Digest {
    type Err = DigestParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (name, hex) = s
            .split_once(':')
            .ok_or(DigestParseError::MissingSeparator)?;
        let algo = DigestAlgo::from_name(name)
            .ok_or_else(|| DigestParseError::UnknownAlgo(name.to_owned()))?;
        let bytes = from_hex(hex)?;
        if bytes.len() != algo.digest_len() {
            return Err(DigestParseError::WrongLength {
                expected: algo.digest_len(),
                actual: bytes.len(),
            });
        }
        Ok(Self { algo, bytes })
    }
}

impl Serialize for Digest {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for Digest {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

/// Why a [`Digest`] failed to parse from its text or multihash byte form.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DigestParseError {
    /// The text form lacked the `"<algo>:<hex>"` separator.
    #[error("digest string is missing the ':' separator")]
    MissingSeparator,
    /// The text form named an algorithm this build does not know.
    #[error("unknown digest algorithm: {0}")]
    UnknownAlgo(String),
    /// The multihash byte form named a multicodec this build does not know.
    #[error("unknown multihash multicodec: {0:#04x}")]
    UnknownMulticodec(u8),
    /// The hex payload was not valid, even-length lowercase/uppercase hex.
    #[error("digest hex payload is not valid hex")]
    BadHex,
    /// The multihash byte form ended before the declared digest length.
    #[error("multihash byte form is truncated")]
    Truncated,
    /// The decoded digest length disagreed with the algorithm's fixed output size.
    #[error("digest length {actual} does not match algorithm's {expected}")]
    WrongLength {
        /// The length the algorithm requires.
        expected: usize,
        /// The length actually decoded.
        actual: usize,
    },
}

fn to_hex(bytes: &[u8]) -> String {
    crate::hex::encode(bytes)
}

fn from_hex(s: &str) -> Result<Vec<u8>, DigestParseError> {
    crate::hex::decode(s).ok_or(DigestParseError::BadHex)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The canonical BLAKE3 hash of the empty input — pins both the algorithm
    /// choice and the hex encoding so a regression in either is caught here.
    const BLAKE3_EMPTY: &str =
        "blake3:af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262";

    #[test]
    fn compute_default_is_blake3_and_matches_known_vector() {
        let d = Digest::compute(b"");
        assert_eq!(d.algo(), DigestAlgo::Blake3);
        assert_eq!(d.to_string(), BLAKE3_EMPTY);
    }

    /// The canonical SHA-256 of the empty input — pins the second algorithm so
    /// the type stays genuinely multi-hash (never one-hash-baked-in).
    const SHA256_EMPTY: &str =
        "sha2-256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

    #[test]
    fn sha256_is_a_first_class_peer_algorithm() {
        let d = Digest::compute_with(DigestAlgo::Sha256, b"");
        assert_eq!(d.algo(), DigestAlgo::Sha256);
        assert_eq!(d.to_string(), SHA256_EMPTY);
        // Distinct algorithms over the same bytes are distinct digests, and each
        // self-describes through its multihash multicodec.
        assert_ne!(Digest::compute(b""), d);
        assert_eq!(d.to_multihash_bytes()[0], 0x12);
        let back = Digest::from_multihash_bytes(&d.to_multihash_bytes()).expect("parses");
        assert_eq!(d, back);
        assert_eq!(SHA256_EMPTY.parse::<Digest>().expect("parses"), d);
    }

    #[test]
    fn compute_is_deterministic() {
        assert_eq!(
            Digest::compute(b"hello world"),
            Digest::compute(b"hello world")
        );
    }

    #[test]
    fn different_inputs_produce_different_digests() {
        assert_ne!(Digest::compute(b"a"), Digest::compute(b"b"));
    }

    #[test]
    fn verify_accepts_matching_bytes_and_rejects_tampered() {
        let d = Digest::compute(b"the original bytes");
        assert!(d.verify(b"the original bytes"));
        assert!(!d.verify(b"the tampered bytes"));
    }

    #[test]
    fn display_roundtrips_through_fromstr() {
        let d = Digest::compute(b"roundtrip me");
        let parsed: Digest = d.to_string().parse().expect("parses");
        assert_eq!(d, parsed);
    }

    #[test]
    fn multihash_bytes_roundtrip_and_are_self_describing() {
        let d = Digest::compute(b"self-describing");
        let mh = d.to_multihash_bytes();
        // <multicodec 0x1e><len 0x20><32 bytes>
        assert_eq!(mh[0], 0x1e);
        assert_eq!(mh[1], 0x20);
        assert_eq!(mh.len(), 2 + 32);
        let back = Digest::from_multihash_bytes(&mh).expect("parses");
        assert_eq!(d, back);
        assert_eq!(back.algo(), DigestAlgo::Blake3);
    }

    #[test]
    fn serde_roundtrips_as_a_plain_string() {
        let d = Digest::compute(b"serde");
        let json = serde_json::to_string(&d).expect("serialize");
        assert_eq!(json, format!("\"{d}\""));
        let back: Digest = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(d, back);
    }

    #[test]
    fn fromstr_rejects_unknown_algorithm() {
        let err = "md5:00".parse::<Digest>().unwrap_err();
        assert_eq!(err, DigestParseError::UnknownAlgo("md5".to_owned()));
    }

    #[test]
    fn fromstr_rejects_missing_separator() {
        assert_eq!(
            "deadbeef".parse::<Digest>().unwrap_err(),
            DigestParseError::MissingSeparator
        );
    }

    #[test]
    fn fromstr_rejects_wrong_length() {
        // Valid hex, valid algo, but only 1 byte where blake3 needs 32.
        let err = "blake3:ab".parse::<Digest>().unwrap_err();
        assert_eq!(
            err,
            DigestParseError::WrongLength {
                expected: 32,
                actual: 1
            }
        );
    }

    #[test]
    fn fromstr_rejects_bad_hex() {
        assert_eq!(
            "blake3:zz".parse::<Digest>().unwrap_err(),
            DigestParseError::BadHex
        );
    }

    #[test]
    fn from_multihash_rejects_unknown_multicodec() {
        let err = Digest::from_multihash_bytes(&[0x99, 0x01, 0xff]).unwrap_err();
        assert_eq!(err, DigestParseError::UnknownMulticodec(0x99));
    }

    #[test]
    fn from_multihash_rejects_truncation() {
        // Declares 32 bytes but supplies 3.
        assert_eq!(
            Digest::from_multihash_bytes(&[0x1e, 0x20, 0x00, 0x00, 0x00]).unwrap_err(),
            DigestParseError::Truncated
        );
    }
}
