//! The `ByteStream` service — large-blob upload/download over the SHA-256-native CAS.
//!
//! The impedance crux: `CasBackend` is **sync** `std::io::Read`/`put_keyed`, tonic is
//! **async** streams. `Read` bridges with a `spawn_blocking` pump feeding a bounded
//! `mpsc` (backpressure) that becomes the response stream; `Write` collects the
//! request stream and `put_keyed`s it on the blocking pool (which re-verifies the
//! bytes hash to the client's SHA-256 digest before committing). Buffered in v0 — a
//! zero-copy `StreamReader`/`SyncIoBridge` path is a documented perf follow-up.

use crate::boundary::Sha256Boundary;
use crate::bytestream as bs;
use crate::reapi;
use crate::resource::ResourceName;
use crate::status::status_from_backend;
use nessie_backend_core::CasBackend;
use std::io::Read;
use std::sync::Arc;
use tokio_stream::StreamExt;
use tonic::{Request, Response, Status, Streaming};

/// A 64 KiB read chunk (the ByteStream response granularity).
const CHUNK: usize = 64 * 1024;

/// The `ByteStream` gRPC service over a SHA-256-native [`CasBackend`].
pub struct ByteStreamSvc {
    cas: Arc<dyn CasBackend>,
    boundary: Sha256Boundary,
}

impl ByteStreamSvc {
    /// Build the service over `cas`.
    #[must_use]
    pub fn new(cas: Arc<dyn CasBackend>) -> Self {
        Self {
            cas,
            boundary: Sha256Boundary,
        }
    }
}

/// A native digest for a `{sha256, size}` blob reference.
fn native_for(
    boundary: &Sha256Boundary,
    sha256: String,
    size: u64,
) -> Result<nessie_backend_core::Digest, Status> {
    boundary.to_native(&reapi::Digest {
        hash: sha256,
        size_bytes: size as i64,
    })
}

/// Collect a `Write` request stream and store it. Generic over the stream so it is
/// unit-testable without a live gRPC transport (`tonic::Streaming` is a `Stream`).
async fn write_stream<S>(
    cas: Arc<dyn CasBackend>,
    boundary: Sha256Boundary,
    mut stream: S,
) -> Result<bs::WriteResponse, Status>
where
    S: tokio_stream::Stream<Item = Result<bs::WriteRequest, Status>> + Unpin,
{
    let mut resource: Option<ResourceName> = None;
    let mut data = Vec::new();
    while let Some(msg) = stream.next().await {
        let msg = msg?;
        if resource.is_none() && !msg.resource_name.is_empty() {
            resource = Some(ResourceName::parse(&msg.resource_name)?);
        }
        data.extend_from_slice(&msg.data);
        if msg.finish_write {
            break;
        }
    }
    let (sha256, size) = match resource {
        Some(ResourceName::Write { sha256, size, .. }) => (sha256, size),
        Some(_) => {
            return Err(Status::invalid_argument(
                "write resource must be an uploads/{uuid}/blobs/ path",
            ));
        }
        None => {
            return Err(Status::invalid_argument(
                "write stream carried no resource_name",
            ));
        }
    };
    if data.len() as u64 != size {
        return Err(Status::invalid_argument(format!(
            "wrote {} bytes but the resource declared {size}",
            data.len()
        )));
    }
    let native = native_for(&boundary, sha256, size)?;
    tokio::task::spawn_blocking(move || {
        // put_keyed re-verifies the accumulated bytes hash to `native` before commit.
        cas.put_keyed(&native, &mut data.as_slice())
            .map_err(|e| status_from_backend(&e))
    })
    .await
    .map_err(|e| Status::internal(format!("write task panicked: {e}")))??;
    Ok(bs::WriteResponse {
        committed_size: size as i64,
    })
}

#[tonic::async_trait]
impl bs::byte_stream_server::ByteStream for ByteStreamSvc {
    type ReadStream = tokio_stream::wrappers::ReceiverStream<Result<bs::ReadResponse, Status>>;

    async fn read(
        &self,
        request: Request<bs::ReadRequest>,
    ) -> Result<Response<Self::ReadStream>, Status> {
        let req = request.into_inner();
        let (sha256, size) = match ResourceName::parse(&req.resource_name)? {
            ResourceName::Read { sha256, size, .. } => (sha256, size),
            ResourceName::Write { .. } => {
                return Err(Status::invalid_argument(
                    "read resource must be a blobs/ path",
                ));
            }
        };
        let native = native_for(&self.boundary, sha256, size)?;
        let cas = self.cas.clone();
        let offset = req.read_offset.max(0) as u64;
        let limit = if req.read_limit > 0 {
            Some(req.read_limit as u64)
        } else {
            None
        };

        let (tx, rx) = tokio::sync::mpsc::channel(8);
        tokio::task::spawn_blocking(move || {
            let mut reader = match cas.get(&native) {
                Ok(r) => r,
                Err(e) => {
                    let _ = tx.blocking_send(Err(status_from_backend(&e)));
                    return;
                }
            };
            // Skip `read_offset` bytes.
            if offset > 0 {
                let _ = std::io::copy(&mut reader.by_ref().take(offset), &mut std::io::sink());
            }
            let mut remaining = limit;
            let mut buf = vec![0u8; CHUNK];
            loop {
                let cap = remaining.map_or(buf.len(), |r| (r as usize).min(buf.len()));
                if cap == 0 {
                    break;
                }
                match reader.read(&mut buf[..cap]) {
                    Ok(0) => break,
                    Ok(n) => {
                        let chunk = bs::ReadResponse {
                            data: buf[..n].to_vec(),
                        };
                        if tx.blocking_send(Ok(chunk)).is_err() {
                            break; // receiver dropped
                        }
                        if let Some(r) = remaining.as_mut() {
                            *r -= n as u64;
                        }
                    }
                    Err(e) => {
                        let _ =
                            tx.blocking_send(Err(Status::internal(format!("reading blob: {e}"))));
                        break;
                    }
                }
            }
        });
        Ok(Response::new(tokio_stream::wrappers::ReceiverStream::new(
            rx,
        )))
    }

    async fn write(
        &self,
        request: Request<Streaming<bs::WriteRequest>>,
    ) -> Result<Response<bs::WriteResponse>, Status> {
        let resp = write_stream(self.cas.clone(), self.boundary, request.into_inner()).await?;
        Ok(Response::new(resp))
    }

    async fn query_write_status(
        &self,
        request: Request<bs::QueryWriteStatusRequest>,
    ) -> Result<Response<bs::QueryWriteStatusResponse>, Status> {
        // v0 is non-resumable: a content-addressed put is cheap to retry, so a write
        // is either complete (the blob is present) or must be restarted.
        let (sha256, size) = match ResourceName::parse(&request.into_inner().resource_name)? {
            ResourceName::Read { sha256, size, .. } | ResourceName::Write { sha256, size, .. } => {
                (sha256, size)
            }
        };
        let native = native_for(&self.boundary, sha256, size)?;
        let cas = self.cas.clone();
        let present = tokio::task::spawn_blocking(move || cas.has(&native))
            .await
            .map_err(|e| Status::internal(format!("query task panicked: {e}")))?
            .map_err(|e| status_from_backend(&e))?;
        Ok(Response::new(bs::QueryWriteStatusResponse {
            committed_size: if present { size as i64 } else { 0 },
            complete: present,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nessie_backend_core::{Digest, DigestAlgo};
    use nessie_backend_mem::MemCas;

    fn svc() -> (ByteStreamSvc, Arc<MemCas>) {
        let cas = Arc::new(MemCas::new());
        (ByteStreamSvc::new(cas.clone()), cas)
    }

    fn write_resource(bytes: &[u8]) -> String {
        let d = Digest::compute_with(DigestAlgo::Sha256, bytes);
        let hash = Sha256Boundary.to_reapi(&d, bytes.len() as u64).hash;
        format!("inst/uploads/u1/blobs/{hash}/{}", bytes.len())
    }

    fn read_resource(bytes: &[u8]) -> String {
        let d = Digest::compute_with(DigestAlgo::Sha256, bytes);
        let hash = Sha256Boundary.to_reapi(&d, bytes.len() as u64).hash;
        format!("blobs/{hash}/{}", bytes.len())
    }

    #[tokio::test]
    async fn write_then_read_round_trips_in_chunks() {
        use bs::byte_stream_server::ByteStream;
        let (svc, _cas) = svc();
        // A blob larger than one 64 KiB chunk.
        let blob: Vec<u8> = (0..200_000).map(|i| (i % 251) as u8).collect();

        // Write it as two request messages.
        let mid = blob.len() / 2;
        let reqs = vec![
            Ok(bs::WriteRequest {
                resource_name: write_resource(&blob),
                write_offset: 0,
                finish_write: false,
                data: blob[..mid].to_vec(),
            }),
            Ok(bs::WriteRequest {
                resource_name: String::new(),
                write_offset: mid as i64,
                finish_write: true,
                data: blob[mid..].to_vec(),
            }),
        ];
        let resp = write_stream(svc.cas.clone(), svc.boundary, tokio_stream::iter(reqs))
            .await
            .expect("write");
        assert_eq!(resp.committed_size, blob.len() as i64);

        // Read it back, concatenating the streamed chunks.
        let mut stream = svc
            .read(Request::new(bs::ReadRequest {
                resource_name: read_resource(&blob),
                read_offset: 0,
                read_limit: 0,
            }))
            .await
            .expect("read")
            .into_inner();
        let mut got = Vec::new();
        let mut chunks = 0;
        while let Some(item) = stream.next().await {
            got.extend_from_slice(&item.expect("chunk").data);
            chunks += 1;
        }
        assert_eq!(got, blob);
        assert!(
            chunks >= 2,
            "a >64KiB blob streams in multiple chunks, got {chunks}"
        );
    }

    #[tokio::test]
    async fn write_rejects_a_size_mismatch() {
        let (svc, _cas) = svc();
        let blob = b"twelve bytes".to_vec();
        // Declare the wrong size in the resource name.
        let d = Digest::compute_with(DigestAlgo::Sha256, &blob);
        let hash = Sha256Boundary.to_reapi(&d, blob.len() as u64).hash;
        let resource = format!("inst/uploads/u/blobs/{hash}/999");
        let reqs = vec![Ok(bs::WriteRequest {
            resource_name: resource,
            write_offset: 0,
            finish_write: true,
            data: blob,
        })];
        assert!(
            write_stream(svc.cas.clone(), svc.boundary, tokio_stream::iter(reqs))
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn read_supports_offset_and_limit() {
        use bs::byte_stream_server::ByteStream;
        let (svc, _cas) = svc();
        let blob = b"0123456789".to_vec();
        write_stream(
            svc.cas.clone(),
            svc.boundary,
            tokio_stream::iter(vec![Ok(bs::WriteRequest {
                resource_name: write_resource(&blob),
                write_offset: 0,
                finish_write: true,
                data: blob.clone(),
            })]),
        )
        .await
        .unwrap();

        let mut stream = svc
            .read(Request::new(bs::ReadRequest {
                resource_name: read_resource(&blob),
                read_offset: 3,
                read_limit: 4,
            }))
            .await
            .unwrap()
            .into_inner();
        let mut got = Vec::new();
        while let Some(item) = stream.next().await {
            got.extend_from_slice(&item.unwrap().data);
        }
        assert_eq!(got, b"3456");
    }

    #[tokio::test]
    async fn query_write_status_reports_presence() {
        use bs::byte_stream_server::ByteStream;
        let (svc, _cas) = svc();
        let blob = b"present blob".to_vec();
        let before = svc
            .query_write_status(Request::new(bs::QueryWriteStatusRequest {
                resource_name: write_resource(&blob),
            }))
            .await
            .unwrap()
            .into_inner();
        assert!(!before.complete);

        write_stream(
            svc.cas.clone(),
            svc.boundary,
            tokio_stream::iter(vec![Ok(bs::WriteRequest {
                resource_name: write_resource(&blob),
                write_offset: 0,
                finish_write: true,
                data: blob.clone(),
            })]),
        )
        .await
        .unwrap();

        let after = svc
            .query_write_status(Request::new(bs::QueryWriteStatusRequest {
                resource_name: write_resource(&blob),
            }))
            .await
            .unwrap()
            .into_inner();
        assert!(after.complete);
        assert_eq!(after.committed_size, blob.len() as i64);
    }
}
