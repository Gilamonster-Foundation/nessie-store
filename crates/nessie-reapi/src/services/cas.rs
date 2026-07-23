//! The `ContentAddressableStorage` service — the dedup + inline-blob workhorse.
//!
//! Implements `FindMissingBlobs` (pre-upload dedup), `BatchUpdateBlobs` (inline
//! upload), and `BatchReadBlobs` (inline download) over the SHA-256-native
//! `CasBackend`. `GetTree` (a following slice) and the split/splice extensions are
//! stubbed. The sync `CasBackend` ops run on the blocking pool so the async control
//! plane is never stalled.

use crate::boundary::Sha256Boundary;
use crate::config::ReapiConfig;
use crate::status::status_from_backend;
use crate::{reapi, rpc};
use nessie_backend_core::CasBackend;
use std::io::Read;
use std::sync::Arc;
use tonic::{Request, Response, Status};

/// The CAS gRPC service over a SHA-256-native [`CasBackend`].
pub struct CasV2Svc {
    cas: Arc<dyn CasBackend>,
    boundary: Sha256Boundary,
    cfg: Arc<ReapiConfig>,
}

impl CasV2Svc {
    /// Build the CAS service over `cas`.
    #[must_use]
    pub fn new(cas: Arc<dyn CasBackend>, cfg: Arc<ReapiConfig>) -> Self {
        Self {
            cas,
            boundary: Sha256Boundary,
            cfg,
        }
    }
}

/// A per-item google.rpc.Status for a `BatchUpdateBlobs` response.
fn rpc_status(outcome: Result<(), Status>) -> rpc::Status {
    match outcome {
        Ok(()) => rpc::Status {
            code: tonic::Code::Ok as i32,
            message: String::new(),
            details: Vec::new(),
        },
        Err(s) => rpc::Status {
            code: s.code() as i32,
            message: s.message().to_string(),
            details: Vec::new(),
        },
    }
}

/// Store one batch item: reject a non-IDENTITY compressor and a size mismatch, then
/// `put_keyed` (which re-verifies the bytes hash to the client's digest).
fn update_one(
    cas: &dyn CasBackend,
    boundary: &Sha256Boundary,
    item: &reapi::batch_update_blobs_request::Request,
) -> Result<(), Status> {
    if item.compressor != reapi::compressor::Value::Identity as i32 {
        return Err(Status::invalid_argument(
            "only the IDENTITY compressor is supported",
        ));
    }
    let digest = item
        .digest
        .as_ref()
        .ok_or_else(|| Status::invalid_argument("batch item is missing its digest"))?;
    if digest.size_bytes < 0 || digest.size_bytes as u64 != item.data.len() as u64 {
        return Err(Status::invalid_argument(format!(
            "data length {} does not match digest size_bytes {}",
            item.data.len(),
            digest.size_bytes
        )));
    }
    let native = boundary.to_native(digest)?;
    let mut source = item.data.as_slice();
    cas.put_keyed(&native, &mut source)
        .map_err(|e| status_from_backend(&e))
}

/// Read one batch item: `get` (self-verifying) the blob and echo it, or a per-item
/// error status with empty data.
fn read_one(
    cas: &dyn CasBackend,
    boundary: &Sha256Boundary,
    reapi_digest: &reapi::Digest,
) -> reapi::batch_read_blobs_response::Response {
    let identity = reapi::compressor::Value::Identity as i32;
    let outcome = (|| -> Result<Vec<u8>, Status> {
        let native = boundary.to_native(reapi_digest)?;
        let mut reader = cas.get(&native).map_err(|e| status_from_backend(&e))?;
        let mut data = Vec::new();
        reader
            .read_to_end(&mut data)
            .map_err(|e| Status::internal(format!("reading blob: {e}")))?;
        Ok(data)
    })();
    match outcome {
        Ok(data) => reapi::batch_read_blobs_response::Response {
            digest: Some(reapi_digest.clone()),
            data,
            compressor: identity,
            status: Some(rpc_status(Ok(()))),
        },
        Err(status) => reapi::batch_read_blobs_response::Response {
            digest: Some(reapi_digest.clone()),
            data: Vec::new(),
            compressor: identity,
            status: Some(rpc_status(Err(status))),
        },
    }
}

#[tonic::async_trait]
impl reapi::content_addressable_storage_server::ContentAddressableStorage for CasV2Svc {
    async fn find_missing_blobs(
        &self,
        request: Request<reapi::FindMissingBlobsRequest>,
    ) -> Result<Response<reapi::FindMissingBlobsResponse>, Status> {
        let req = request.into_inner();
        let cas = self.cas.clone();
        let boundary = self.boundary;
        tokio::task::spawn_blocking(move || {
            let mut missing = Vec::new();
            for reapi_digest in req.blob_digests {
                let native = boundary.to_native(&reapi_digest)?;
                if !cas.has(&native).map_err(|e| status_from_backend(&e))? {
                    // Echo the client's original digest (hash + size) for each miss.
                    missing.push(reapi_digest);
                }
            }
            Ok(Response::new(reapi::FindMissingBlobsResponse {
                missing_blob_digests: missing,
            }))
        })
        .await
        .map_err(|e| Status::internal(format!("find_missing_blobs task panicked: {e}")))?
    }

    async fn batch_update_blobs(
        &self,
        request: Request<reapi::BatchUpdateBlobsRequest>,
    ) -> Result<Response<reapi::BatchUpdateBlobsResponse>, Status> {
        let req = request.into_inner();
        let total: usize = req.requests.iter().map(|r| r.data.len()).sum();
        if total as i64 > self.cfg.max_batch_total_size_bytes {
            return Err(Status::resource_exhausted(format!(
                "batch of {total} bytes exceeds max_batch_total_size_bytes {}",
                self.cfg.max_batch_total_size_bytes
            )));
        }
        let cas = self.cas.clone();
        let boundary = self.boundary;
        tokio::task::spawn_blocking(move || {
            let responses = req
                .requests
                .into_iter()
                .map(|item| reapi::batch_update_blobs_response::Response {
                    digest: item.digest.clone(),
                    status: Some(rpc_status(update_one(&*cas, &boundary, &item))),
                })
                .collect();
            Ok(Response::new(reapi::BatchUpdateBlobsResponse { responses }))
        })
        .await
        .map_err(|e| Status::internal(format!("batch_update_blobs task panicked: {e}")))?
    }

    async fn batch_read_blobs(
        &self,
        request: Request<reapi::BatchReadBlobsRequest>,
    ) -> Result<Response<reapi::BatchReadBlobsResponse>, Status> {
        let req = request.into_inner();
        let cas = self.cas.clone();
        let boundary = self.boundary;
        tokio::task::spawn_blocking(move || {
            let responses = req
                .digests
                .iter()
                .map(|d| read_one(&*cas, &boundary, d))
                .collect();
            Ok(Response::new(reapi::BatchReadBlobsResponse { responses }))
        })
        .await
        .map_err(|e| Status::internal(format!("batch_read_blobs task panicked: {e}")))?
    }

    /// Streamed; filled in the GetTree slice. The concrete stream type is named here
    /// so the trait is satisfied, but this stub never constructs one.
    type GetTreeStream =
        tokio_stream::wrappers::ReceiverStream<Result<reapi::GetTreeResponse, Status>>;

    async fn get_tree(
        &self,
        _request: Request<reapi::GetTreeRequest>,
    ) -> Result<Response<Self::GetTreeStream>, Status> {
        Err(Status::unimplemented("GetTree lands in a following slice"))
    }

    async fn split_blob(
        &self,
        _request: Request<reapi::SplitBlobRequest>,
    ) -> Result<Response<reapi::SplitBlobResponse>, Status> {
        Err(Status::unimplemented(
            "SplitBlob is not supported (cache subset)",
        ))
    }

    async fn splice_blob(
        &self,
        _request: Request<reapi::SpliceBlobRequest>,
    ) -> Result<Response<reapi::SpliceBlobResponse>, Status> {
        Err(Status::unimplemented(
            "SpliceBlob is not supported (cache subset)",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nessie_backend_core::{Digest, DigestAlgo};
    use nessie_backend_mem::MemCas;
    use reapi::content_addressable_storage_server::ContentAddressableStorage;

    fn svc() -> CasV2Svc {
        CasV2Svc::new(Arc::new(MemCas::new()), Arc::new(ReapiConfig::default()))
    }

    fn reapi_digest(bytes: &[u8]) -> reapi::Digest {
        Sha256Boundary.to_reapi(
            &Digest::compute_with(DigestAlgo::Sha256, bytes),
            bytes.len() as u64,
        )
    }

    #[tokio::test]
    async fn batch_update_then_find_missing_dedups() {
        let svc = svc();
        let present = b"hello reapi".to_vec();
        let absent = b"never uploaded".to_vec();

        // Upload `present`.
        let up = svc
            .batch_update_blobs(Request::new(reapi::BatchUpdateBlobsRequest {
                requests: vec![reapi::batch_update_blobs_request::Request {
                    digest: Some(reapi_digest(&present)),
                    data: present.clone(),
                    compressor: reapi::compressor::Value::Identity as i32,
                }],
                ..Default::default()
            }))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(
            up.responses[0].status.as_ref().unwrap().code,
            tonic::Code::Ok as i32
        );

        // FindMissing reports only the absent one.
        let missing = svc
            .find_missing_blobs(Request::new(reapi::FindMissingBlobsRequest {
                blob_digests: vec![reapi_digest(&present), reapi_digest(&absent)],
                ..Default::default()
            }))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(missing.missing_blob_digests, vec![reapi_digest(&absent)]);
    }

    #[tokio::test]
    async fn batch_update_then_read_is_byte_identical() {
        let svc = svc();
        let blob = b"round-trip me".to_vec();
        svc.batch_update_blobs(Request::new(reapi::BatchUpdateBlobsRequest {
            requests: vec![reapi::batch_update_blobs_request::Request {
                digest: Some(reapi_digest(&blob)),
                data: blob.clone(),
                compressor: reapi::compressor::Value::Identity as i32,
            }],
            ..Default::default()
        }))
        .await
        .unwrap();

        let missing_digest = reapi_digest(b"not stored");
        let read = svc
            .batch_read_blobs(Request::new(reapi::BatchReadBlobsRequest {
                digests: vec![reapi_digest(&blob), missing_digest.clone()],
                ..Default::default()
            }))
            .await
            .unwrap()
            .into_inner();

        assert_eq!(read.responses.len(), 2);
        // Present: byte-identical, OK.
        assert_eq!(read.responses[0].data, blob);
        assert_eq!(
            read.responses[0].status.as_ref().unwrap().code,
            tonic::Code::Ok as i32
        );
        // Absent: NOT_FOUND, empty data.
        assert!(read.responses[1].data.is_empty());
        assert_eq!(
            read.responses[1].status.as_ref().unwrap().code,
            tonic::Code::NotFound as i32
        );
    }

    #[tokio::test]
    async fn batch_update_rejects_a_wrong_digest() {
        let svc = svc();
        // Digest of one blob, data of another — put_keyed must reject it per-item.
        let mut wrong = reapi_digest(b"claimed");
        let data = b"actual bytes".to_vec();
        wrong.size_bytes = data.len() as i64; // pass the size gate; fail the hash
        let resp = svc
            .batch_update_blobs(Request::new(reapi::BatchUpdateBlobsRequest {
                requests: vec![reapi::batch_update_blobs_request::Request {
                    digest: Some(wrong),
                    data,
                    compressor: reapi::compressor::Value::Identity as i32,
                }],
                ..Default::default()
            }))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(
            resp.responses[0].status.as_ref().unwrap().code,
            tonic::Code::InvalidArgument as i32
        );
    }
}
