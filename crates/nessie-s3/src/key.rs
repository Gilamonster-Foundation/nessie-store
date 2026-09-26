//! The S3 object key → native [`Digest`] map.
//!
//! This is the payoff of content addressing, and the reason the S3 face needs no
//! index: for a cache client that keys blobs by their own SHA-256 —
//! bazel-remote's `{prefix}/cas/{xx}/{hex}` — **the object key *is* the digest**.
//! There is no bucket→object table, no name→digest mapping, nothing to keep
//! consistent across a restart. [`ObjectKey::parse`] is a pure function and the
//! CAS is the only state.
//!
//! Two key shapes are deliberately *refused* rather than stored:
//!
//! * `cas.v2/…` — written by bazel-remote's default `--s3.storage_mode zstd`.
//!   Those objects are zstd frames behind a bazel-remote-specific header, so the
//!   stored bytes do **not** hash to the key naming them. Accepting one would mean
//!   filing bytes under a digest they do not have, which is the single thing a
//!   content-addressed store may never do. `--s3.storage_mode uncompressed` is the
//!   aligned setting, and [`ObjectKey::Compressed`] exists so the face can say so
//!   instead of failing with a bare verification error.
//! * `ac/…` and `raw/…` — keyed by the *action* digest (or by nothing), not by the
//!   content's own hash. That is a name→bytes mapping, which is the action-cache
//!   tier's job, not the CAS's. Slice 1 reports them absent; see the crate docs.
//!
//! The client's configured key prefix (`--s3.prefix`) is **ignored** rather than
//! configured away: only the last three segments are read. That is safe precisely
//! because the store is content-addressed — two prefixes naming one digest name
//! one blob, so aliasing them is identity, not collision. It also means the face
//! has no setting that can be wrong.

use nessie_backend_core::Digest;

/// What an S3 object key names in the content-addressed store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ObjectKey {
    /// A content-addressed blob. The digest was read from the key itself, so the
    /// write path can hand it straight to `put_keyed` for re-verification.
    Cas(Digest),
    /// A `cas.v2/…` key: content-addressed in *name* only, because the bytes are
    /// framed. Carried as its own variant so the face can name the client flag
    /// that fixes it.
    Compressed,
    /// An `ac/…` or `raw/…` key — keyed by something other than its own content.
    NotContentAddressed,
}

impl ObjectKey {
    /// Classify an S3 object key, or `None` if it does not have a recognized
    /// `{kind}/{xx}/{hash}` tail (the caller reports those absent).
    ///
    /// The shard segment must be the hash's own first two characters, as both of
    /// bazel-remote's key builders emit it; a mismatch is a malformed key, not a
    /// blob we are missing.
    #[must_use]
    pub fn parse(key: &str) -> Option<Self> {
        let mut segments = key.rsplit('/');
        let hash = segments.next()?;
        let shard = segments.next()?;
        let kind = segments.next()?;
        if shard.len() != 2 || !hash.starts_with(shard) {
            return None;
        }
        match kind {
            "cas" => sha256_digest(hash).map(ObjectKey::Cas),
            "cas.v2" => Some(ObjectKey::Compressed),
            "ac" | "raw" => Some(ObjectKey::NotContentAddressed),
            _ => None,
        }
    }
}

/// Parse 64 lowercase-hex characters as a native SHA-256 [`Digest`].
///
/// Lowercase is required, not merely conventional: the hex is the *name* of the
/// object, and accepting both cases would let one blob have two names — exactly
/// the aliasing a content-addressed store exists to prevent.
fn sha256_digest(hash: &str) -> Option<Digest> {
    if hash.len() != 64 || !hash.bytes().all(is_lower_hex) {
        return None;
    }
    format!("sha2-256:{hash}").parse().ok()
}

fn is_lower_hex(b: u8) -> bool {
    b.is_ascii_digit() || b.is_ascii_lowercase() && b <= b'f'
}

#[cfg(test)]
mod tests {
    use super::*;
    use nessie_backend_core::DigestAlgo;

    /// SHA-256(""), the canonical vector — and the shard bazel-remote would use.
    const EMPTY: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

    fn cas_key(prefix: &str) -> String {
        format!("{prefix}cas/{}/{EMPTY}", &EMPTY[..2])
    }

    #[test]
    fn cas_key_yields_the_digest_named_in_it() {
        let Some(ObjectKey::Cas(d)) = ObjectKey::parse(&cas_key("")) else {
            panic!("expected a CAS key");
        };
        assert_eq!(d.algo(), DigestAlgo::Sha256);
        // The whole point: no recompute, no index — the key named this blob.
        assert_eq!(d, Digest::compute_with(DigestAlgo::Sha256, b""));
    }

    #[test]
    fn any_client_prefix_maps_to_the_same_blob() {
        // Prefix-agnostic by construction; harmless because the digest is identity.
        let bare = ObjectKey::parse(&cas_key(""));
        for prefix in ["bazel/", "a/b/c/", "cas/"] {
            assert_eq!(
                ObjectKey::parse(&cas_key(prefix)),
                bare,
                "prefix {prefix:?}"
            );
        }
    }

    #[test]
    fn compressed_and_action_cache_keys_are_distinguished() {
        let shard = &EMPTY[..2];
        assert_eq!(
            ObjectKey::parse(&format!("cas.v2/{shard}/{EMPTY}")),
            Some(ObjectKey::Compressed)
        );
        for kind in ["ac", "raw"] {
            assert_eq!(
                ObjectKey::parse(&format!("{kind}/{shard}/{EMPTY}")),
                Some(ObjectKey::NotContentAddressed),
                "kind {kind:?}"
            );
        }
    }

    #[test]
    fn malformed_keys_are_rejected() {
        let shard = &EMPTY[..2];
        let upper = EMPTY.to_uppercase();
        for bad in [
            String::new(),
            EMPTY.to_string(),                       // no kind/shard
            format!("cas/{EMPTY}"),                  // no shard
            format!("cas/zz/{EMPTY}"),               // shard != hash[..2]
            format!("cas/{}/{upper}", &upper[..2]),  // uppercase hex aliases
            format!("cas/{shard}/{}", &EMPTY[..63]), // short hash
            format!("cas/{shard}/{EMPTY}0"),         // long hash
            format!("blobs/{shard}/{EMPTY}"),        // unknown kind
        ] {
            assert_eq!(ObjectKey::parse(&bad), None, "must reject {bad:?}");
        }
    }
}
