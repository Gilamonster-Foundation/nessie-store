//! The S3 operations a content-addressed cache face actually needs.
//!
//! `s3s` supplies the S3 protocol — routing, SigV4, XML, the 114-method [`S3`]
//! trait whose every method defaults to `NotImplemented`. This implements the
//! three that a digest-keyed client calls — `PutObject`, `GetObject`, `HeadObject` —
//! plus `DeleteObject`, which only unpins (#137). `ListObjectsV2` lists the sha2-256
//! blobs. Everything else keeps the honest default decline, bulk `DeleteObjects` included.
//!
//! The same impedance mismatch the REAPI `ByteStream` service has applies here:
//! [`CasBackend`] is sync `std::io::Read` / `put_keyed`, `s3s` is async streams.
//! Writes bridge with `StreamReader` + `SyncIoBridge` (a real streaming path — the
//! blob is never held whole in memory); reads use a `spawn_blocking` pump feeding a
//! bounded `mpsc`, which is where the backpressure comes from.

use crate::error::s3_error_from_backend;
use crate::key::ObjectKey;
use bytes::{Bytes, BytesMut};
use futures::TryStreamExt;
use nessie_backend_core::{BackendError, CasBackend, Digest, DigestAlgo};
use s3s::dto::{
    DeleteObjectInput, DeleteObjectOutput, ETag, GetObjectInput, GetObjectOutput, HeadObjectInput,
    HeadObjectOutput, ListObjectsV2Input, ListObjectsV2Output, Object, PutObjectInput,
    PutObjectOutput, StreamingBlob, Timestamp,
};
use s3s::{S3, S3Request, S3Response, S3Result, s3_error};
use std::io::Read;
use std::sync::Arc;
use tokio_util::io::{StreamReader, SyncIoBridge};

/// A 64 KiB read chunk — the response-body granularity, matching the REAPI
/// `ByteStream` face so both protocols pull from the CAS the same way.
const CHUNK: usize = 64 * 1024;

/// The user-metadata key (`x-amz-meta-nessie-pin`) a PutObject sends to ask for a pin.
const PIN_META: &str = "nessie-pin";

/// Where the face records "keep this": the store's pins, which `lru` never evicts
/// (#137). The daemon's CAS node implements it.
pub trait Pins: Send + Sync {
    /// Pin `digest` (idempotent), durably before returning.
    ///
    /// # Errors
    ///
    /// The pin could not be recorded.
    fn pin(&self, digest: &Digest) -> Result<(), BackendError>;

    /// Unpin `digest` (idempotent).
    ///
    /// # Errors
    ///
    /// The change could not be recorded.
    fn unpin(&self, digest: &Digest) -> Result<(), BackendError>;
}

/// The S3 face over a [`CasBackend`].
///
/// Holds no state of its own: every S3 key resolves to a digest by parsing, so the
/// CAS is the whole store. That is why the face needs no configuration beyond the
/// credentials `s3s` checks.
pub struct NessieS3 {
    cas: Arc<dyn CasBackend>,
    /// `None`: nothing to pin into (a private in-memory store, which evicts nothing).
    pins: Option<Arc<dyn Pins>>,
    /// Pin every PutObject, not only those that send [`PIN_META`].
    pin_on_put: bool,
}

impl NessieS3 {
    /// Build the face over `cas`.
    #[must_use]
    pub fn new(cas: Arc<dyn CasBackend>) -> Self {
        Self {
            cas,
            pins: None,
            pin_on_put: false,
        }
    }

    /// Record pins in `pins`: every PutObject when `pin_on_put`, else only those
    /// sending `x-amz-meta-nessie-pin: true`. DeleteObject unpins.
    #[must_use]
    pub fn with_pins(mut self, pins: Arc<dyn Pins>, pin_on_put: bool) -> Self {
        self.pins = Some(pins);
        self.pin_on_put = pin_on_put;
        self
    }

    /// Pin (`on`) or unpin `digest`, on the blocking pool (it writes to disk).
    async fn set_pin(&self, digest: &Digest, on: bool) -> S3Result<()> {
        let Some(pins) = self.pins.clone() else {
            return Ok(());
        };
        let d = digest.clone();
        tokio::task::spawn_blocking(move || if on { pins.pin(&d) } else { pins.unpin(&d) })
            .await
            .map_err(|e| s3_error!(InternalError, "pin task panicked: {e}"))?
            .map_err(|e| s3_error_from_backend(&e))
    }

    /// Resolve a key to the digest it names, or the S3 error explaining why it
    /// names no blob we can hold.
    ///
    /// The asymmetry is deliberate: a `cas.v2` key is reported **absent** on the
    /// read paths (so a misconfigured proxy degrades to a cold cache rather than
    /// failing builds) and refused *loudly* on the write path, where the client
    /// logs the error and an operator can read it. See [`ObjectKey`].
    fn digest_for_read(&self, key: &str) -> S3Result<Digest> {
        match ObjectKey::parse(key) {
            Some(ObjectKey::Cas(d)) => Ok(d),
            _ => Err(s3_error!(NoSuchKey, "no content-addressed blob at {key:?}")),
        }
    }
}

#[async_trait::async_trait]
impl S3 for NessieS3 {
    /// Store bytes under the digest the key names, re-verifying they hash to it.
    ///
    /// This is the whole security property of the face in one call: the client
    /// supplies the digest, `put_keyed` recomputes it, and a mismatch is rejected.
    /// Nothing is ever filed under a digest it does not have.
    async fn put_object(
        &self,
        req: S3Request<PutObjectInput>,
    ) -> S3Result<S3Response<PutObjectOutput>> {
        let PutObjectInput {
            body,
            key,
            metadata,
            ..
        } = req.input;
        let pin = self.pin_on_put
            || metadata
                .as_ref()
                .and_then(|m| m.get(PIN_META))
                .is_some_and(|v| v.eq_ignore_ascii_case("true"));
        let digest = match ObjectKey::parse(&key) {
            Some(ObjectKey::Cas(d)) => d,
            Some(ObjectKey::Compressed) => {
                return Err(s3_error!(
                    InvalidRequest,
                    "{key:?} is a framed blob whose bytes do not hash to its key; \
                     this store only accepts content-addressed writes — run the \
                     client with uncompressed storage (bazel-remote: \
                     --s3.storage_mode uncompressed)"
                ));
            }
            Some(ObjectKey::NotContentAddressed) => {
                return Err(s3_error!(
                    NotImplemented,
                    "{key:?} is keyed by something other than its own content; \
                     this face serves the content-addressed prefix only"
                ));
            }
            None => return Err(s3_error!(InvalidRequest, "unrecognized object key {key:?}")),
        };
        let Some(body) = body else {
            return Err(s3_error!(IncompleteBody));
        };

        // Stream the request body into the sync CAS write without buffering it:
        // async Stream -> AsyncRead -> blocking Read, consumed on the blocking pool.
        // SyncIoBridge must be built here, in async context, to capture the handle.
        let reader = StreamReader::new(body.map_err(std::io::Error::other));
        let mut bridge = SyncIoBridge::new(reader);
        let cas = self.cas.clone();
        let verified = digest.clone();
        tokio::task::spawn_blocking(move || cas.put_keyed(&verified, &mut bridge))
            .await
            .map_err(|e| s3_error!(InternalError, "put task panicked: {e}"))?
            .map_err(|e| s3_error_from_backend(&e))?;
        // After the verified write; a pin that was not recorded fails the request.
        if pin {
            self.set_pin(&digest, true).await?;
        }

        Ok(S3Response::new(PutObjectOutput {
            e_tag: Some(ETag::Strong(etag_of(&digest))),
            ..Default::default()
        }))
    }

    /// Serve the blob the key names. `CasBackend::get` verifies the bytes hash to
    /// the digest before handing them over, so the read path is tamper-evident
    /// whether or not the client checks.
    async fn get_object(
        &self,
        req: S3Request<GetObjectInput>,
    ) -> S3Result<S3Response<GetObjectOutput>> {
        let digest = self.digest_for_read(&req.input.key)?;
        let cas = self.cas.clone();

        // Size first: a content digest carries no length, and S3 requires
        // Content-Length. This also turns "absent" into NoSuchKey before we commit
        // to a streaming response we could no longer put an error into.
        let size = size_of(&cas, &digest).await?;

        let (tx, rx) = tokio::sync::mpsc::channel(8);
        let streamed = digest.clone();
        tokio::task::spawn_blocking(move || {
            let mut reader = match cas.get(&streamed) {
                Ok(r) => r,
                Err(e) => {
                    let _ = tx.blocking_send(Err(std::io::Error::other(e.to_string())));
                    return;
                }
            };
            let mut buf = BytesMut::zeroed(CHUNK);
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        if tx
                            .blocking_send(Ok(Bytes::copy_from_slice(&buf[..n])))
                            .is_err()
                        {
                            break; // receiver dropped
                        }
                    }
                    Err(e) => {
                        let _ = tx.blocking_send(Err(e));
                        break;
                    }
                }
            }
        });

        Ok(S3Response::new(GetObjectOutput {
            body: Some(StreamingBlob::wrap(
                tokio_stream::wrappers::ReceiverStream::new(rx),
            )),
            content_length: Some(size as i64),
            e_tag: Some(ETag::Strong(etag_of(&digest))),
            ..Default::default()
        }))
    }

    /// Unpin the digest the key names: 204 whether or not it was pinned. Never deletes
    /// bytes; an unpinned blob stays until `lru` reclaims it (`write_only` keeps it).
    async fn delete_object(
        &self,
        req: S3Request<DeleteObjectInput>,
    ) -> S3Result<S3Response<DeleteObjectOutput>> {
        if let Some(ObjectKey::Cas(digest)) = ObjectKey::parse(&req.input.key) {
            self.set_pin(&digest, false).await?;
        }
        Ok(S3Response::new(DeleteObjectOutput::default()))
    }

    /// The existence probe. The client's `Contains` path, and the cheapest
    /// question the store answers.
    async fn head_object(
        &self,
        req: S3Request<HeadObjectInput>,
    ) -> S3Result<S3Response<HeadObjectOutput>> {
        let digest = self.digest_for_read(&req.input.key)?;
        let size = size_of(&self.cas, &digest).await?;
        Ok(S3Response::new(HeadObjectOutput {
            content_length: Some(size as i64),
            e_tag: Some(ETag::Strong(etag_of(&digest))),
            ..Default::default()
        }))
    }

    /// List the sha2-256 blobs held locally, as `cas/{xx}/{hex}` keys.
    ///
    /// Sharded rather than flat because that is what bazel-remote's S3 backend
    /// writes and reads, and a flat key has no shard to prefix-filter on; the flat
    /// form is still accepted on every other call (see [`ObjectKey`]). Blake3 blobs
    /// are not listed — they have no S3 name. `delimiter` is declined (501), not
    /// ignored: ignoring it would hand `list_with_delimiter` clients slash-bearing
    /// keys where they expect `CommonPrefixes`.
    async fn list_objects_v2(
        &self,
        req: S3Request<ListObjectsV2Input>,
    ) -> S3Result<S3Response<ListObjectsV2Output>> {
        let i = req.input;
        if i.delimiter.as_deref().is_some_and(|d| !d.is_empty()) {
            return Err(s3_error!(
                NotImplemented,
                "ListObjectsV2 delimiter is not supported"
            ));
        }
        let cas = self.cas.clone();
        let local = tokio::task::spawn_blocking(move || {
            cas.as_reclaimable()
                .ok_or_else(|| s3_error!(NotImplemented, "this store cannot enumerate its blobs"))?
                .iter_local()
                .map_err(|e| s3_error_from_backend(&e))
        })
        .await
        .map_err(|e| s3_error!(InternalError, "list task panicked: {e}"))??;

        let blobs = local
            .into_iter()
            .filter(|b| b.digest.algo() == DigestAlgo::Sha256)
            .map(|b| (canonical_key(&b.digest), b.size_bytes, b.digest))
            .collect();
        // The continuation token is the last key returned; it wins over start-after,
        // which S3 only honours on the first request.
        let after = i.continuation_token.as_deref().or(i.start_after.as_deref());
        let max = i.max_keys.map_or(MAX_KEYS, |m| m.clamp(0, MAX_KEYS));
        let (page, next) = list_page(
            blobs,
            i.prefix.as_deref().unwrap_or(""),
            after,
            max as usize,
        );
        let contents: Vec<Object> = page
            .into_iter()
            .map(|(key, size, digest)| Object {
                key: Some(key),
                size: Some(size as i64),
                e_tag: Some(ETag::Strong(etag_of(&digest))),
                // ponytail: a CAS keeps no mtime, so report the epoch, not a lie that
                // looks real. Upgrade = record write time in the store.
                last_modified: Some(Timestamp::from(std::time::UNIX_EPOCH)),
                ..Default::default()
            })
            .collect();
        Ok(S3Response::new(ListObjectsV2Output {
            name: Some(i.bucket),
            prefix: i.prefix,
            max_keys: Some(max),
            key_count: Some(contents.len() as i32),
            is_truncated: Some(next.is_some()),
            continuation_token: i.continuation_token,
            next_continuation_token: next,
            contents: Some(contents),
            ..Default::default()
        }))
    }
}

/// S3's page ceiling, and the default when `max-keys` is absent.
const MAX_KEYS: i32 = 1000;

/// The face's canonical key for a sha2-256 digest: `cas/{xx}/{hex}`.
fn canonical_key(digest: &Digest) -> String {
    let hex = etag_of(digest);
    format!("cas/{}/{hex}", &hex[..2])
}

/// One page of a lexicographic listing: keys with `prefix`, strictly after `after`,
/// at most `max`; plus the token (the last key) when more remain.
// ponytail: collects and sorts every blob per page, O(n log n) per request; upgrade
// = an ordered index in the store so a page seeks instead of scanning.
fn list_page<T>(
    mut blobs: Vec<(String, u64, T)>,
    prefix: &str,
    after: Option<&str>,
    max: usize,
) -> (Vec<(String, u64, T)>, Option<String>) {
    blobs.retain(|(k, ..)| k.starts_with(prefix) && after.is_none_or(|a| k.as_str() > a));
    blobs.sort_by(|a, b| a.0.cmp(&b.0));
    let truncated = blobs.len() > max && max > 0;
    blobs.truncate(max);
    let next = truncated
        .then(|| blobs.last().map(|b| b.0.clone()))
        .flatten();
    (blobs, next)
}

/// The blob's stored length, or `NoSuchKey` if we do not hold it.
async fn size_of(cas: &Arc<dyn CasBackend>, digest: &Digest) -> S3Result<u64> {
    let cas = cas.clone();
    let d = digest.clone();
    let size = tokio::task::spawn_blocking(move || cas.size(&d))
        .await
        .map_err(|e| s3_error!(InternalError, "size task panicked: {e}"))?
        .map_err(|e| s3_error_from_backend(&e))?;
    size.ok_or_else(|| s3_error!(NoSuchKey, "no blob for {digest}"))
}

/// The object's ETag: the hex SHA-256 that already names it.
///
/// Deliberately **not** S3's documented MD5-of-the-body. We hold a strong digest
/// of every object for free and will not make a second pass over every blob to
/// compute a weaker one for a header this face's clients do not check. A future
/// client that validates ETags as MD5 is the trigger to revisit this, not before.
fn etag_of(digest: &Digest) -> String {
    digest
        .to_string()
        .rsplit_once(':')
        .map_or_else(|| digest.to_string(), |(_, hex)| hex.to_owned())
}

#[cfg(test)]
mod tests {
    use super::list_page;

    fn blobs(keys: &[&str]) -> Vec<(String, u64, ())> {
        keys.iter().map(|k| ((*k).to_owned(), 1, ())).collect()
    }
    fn keys(p: &[(String, u64, ())]) -> Vec<&str> {
        p.iter().map(|b| b.0.as_str()).collect()
    }

    #[test]
    fn pages_chain_through_the_token_without_gaps_or_repeats() {
        let all = ["cas/a", "cas/c", "cas/b", "cas/e", "cas/d"];
        let (p1, t1) = list_page(blobs(&all), "", None, 2);
        assert_eq!(keys(&p1), ["cas/a", "cas/b"]);
        let (p2, t2) = list_page(blobs(&all), "", t1.as_deref(), 2);
        assert_eq!(keys(&p2), ["cas/c", "cas/d"]);
        let (p3, t3) = list_page(blobs(&all), "", t2.as_deref(), 2);
        assert_eq!(keys(&p3), ["cas/e"]);
        assert_eq!(t3, None, "the exact-fit last page is not truncated");
    }

    #[test]
    fn prefix_start_after_and_zero_max() {
        let all = ["cas/a/1", "cas/a/2", "cas/b/1"];
        let (p, t) = list_page(blobs(&all), "cas/a/", Some("cas/a/1"), 10);
        assert_eq!((keys(&p), t), (vec!["cas/a/2"], None));
        let (p, t) = list_page(blobs(&all), "", None, 0);
        assert_eq!((p.len(), t), (0, None));
    }
}
