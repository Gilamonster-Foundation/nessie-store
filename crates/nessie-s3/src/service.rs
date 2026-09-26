//! The three S3 operations a content-addressed cache face actually needs.
//!
//! `s3s` supplies the S3 protocol — routing, SigV4, XML, the 114-method [`S3`]
//! trait whose every method defaults to `NotImplemented`. This implements the
//! three that a digest-keyed client calls: `PutObject`, `GetObject`, `HeadObject`.
//! Everything else keeps the honest default decline.
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
use nessie_backend_core::{CasBackend, Digest};
use s3s::dto::{
    ETag, GetObjectInput, GetObjectOutput, HeadObjectInput, HeadObjectOutput, PutObjectInput,
    PutObjectOutput, StreamingBlob,
};
use s3s::{S3, S3Request, S3Response, S3Result, s3_error};
use std::io::Read;
use std::sync::Arc;
use tokio_util::io::{StreamReader, SyncIoBridge};

/// A 64 KiB read chunk — the response-body granularity, matching the REAPI
/// `ByteStream` face so both protocols pull from the CAS the same way.
const CHUNK: usize = 64 * 1024;

/// The S3 face over a [`CasBackend`].
///
/// Holds no state of its own: every S3 key resolves to a digest by parsing, so the
/// CAS is the whole store. That is why the face needs no configuration beyond the
/// credentials `s3s` checks.
pub struct NessieS3 {
    cas: Arc<dyn CasBackend>,
}

impl NessieS3 {
    /// Build the face over `cas`.
    #[must_use]
    pub fn new(cas: Arc<dyn CasBackend>) -> Self {
        Self { cas }
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
        let PutObjectInput { body, key, .. } = req.input;
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
