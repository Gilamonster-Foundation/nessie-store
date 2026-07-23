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
use nessie_backend_core::{CasBackend, Digest};
use prost::Message;
use std::io::Read;
use std::sync::Arc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status};

/// Directories per `GetTreeResponse` page when the client does not pin `page_size`.
const DEFAULT_GET_TREE_PAGE_SIZE: usize = 1000;

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

/// Fully read a stored blob's bytes (`BlobNotFound` → `NotFound`).
fn read_blob_bytes(cas: &dyn CasBackend, digest: &Digest) -> Result<Vec<u8>, Status> {
    let mut reader = cas.get(digest).map_err(|e| status_from_backend(&e))?;
    let mut buf = Vec::new();
    reader
        .read_to_end(&mut buf)
        .map_err(|e| Status::internal(format!("reading Directory blob: {e}")))?;
    Ok(buf)
}

/// Breadth-first walk of the `Directory` DAG rooted at `root`, returning the stored
/// `Directory` protos in BFS order (shared subtrees visited once). We **decode and
/// re-emit the stored proto** — never re-derive a tree — so the returned messages are
/// exactly what the client uploaded, and only child *directory* nodes are followed
/// (files are leaves). A missing/corrupt blob aborts the walk with a `Status`.
fn collect_tree(
    cas: &dyn CasBackend,
    boundary: &Sha256Boundary,
    root: &Digest,
) -> Result<Vec<reapi::Directory>, Status> {
    use std::collections::{HashSet, VecDeque};
    let mut visited: HashSet<Digest> = HashSet::new();
    let mut queue: VecDeque<Digest> = VecDeque::new();
    let mut out: Vec<reapi::Directory> = Vec::new();
    queue.push_back(root.clone());
    while let Some(dg) = queue.pop_front() {
        if !visited.insert(dg.clone()) {
            continue;
        }
        let bytes = read_blob_bytes(cas, &dg)?;
        let dir = reapi::Directory::decode(bytes.as_slice())
            .map_err(|e| Status::invalid_argument(format!("blob is not a REAPI Directory: {e}")))?;
        for node in &dir.directories {
            if let Some(child) = &node.digest {
                queue.push_back(boundary.to_native(child)?);
            }
        }
        out.push(dir);
    }
    Ok(out)
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

    type GetTreeStream = ReceiverStream<Result<reapi::GetTreeResponse, Status>>;

    /// Stream the `Directory` DAG rooted at `root_digest`, breadth-first, in pages of
    /// `page_size` (`next_page_token` = the count already delivered, so a fresh call
    /// with that token resumes exactly there). The whole walk runs on the blocking pool
    /// and feeds a bounded channel; a missing root or corrupt Directory aborts the
    /// stream with a `Status`.
    async fn get_tree(
        &self,
        request: Request<reapi::GetTreeRequest>,
    ) -> Result<Response<Self::GetTreeStream>, Status> {
        let req = request.into_inner();
        let root = req
            .root_digest
            .ok_or_else(|| Status::invalid_argument("missing root_digest"))?;
        let root = self.boundary.to_native(&root)?;
        let page_size = if req.page_size <= 0 {
            DEFAULT_GET_TREE_PAGE_SIZE
        } else {
            req.page_size as usize
        };
        let skip: usize = if req.page_token.is_empty() {
            0
        } else {
            req.page_token
                .parse()
                .map_err(|_| Status::invalid_argument("invalid page_token"))?
        };
        let cas = self.cas.clone();
        let boundary = self.boundary;
        let (tx, rx) = tokio::sync::mpsc::channel(4);
        tokio::task::spawn_blocking(move || {
            let all = match collect_tree(&*cas, &boundary, &root) {
                Ok(v) => v,
                Err(e) => {
                    let _ = tx.blocking_send(Err(e));
                    return;
                }
            };
            let total = all.len();
            let start = skip.min(total);
            let mut remaining: Vec<reapi::Directory> = all.into_iter().skip(start).collect();
            let mut delivered = start;
            loop {
                let take = page_size.min(remaining.len());
                let chunk: Vec<reapi::Directory> = remaining.drain(..take).collect();
                delivered += take;
                let is_final = remaining.is_empty();
                let resp = reapi::GetTreeResponse {
                    directories: chunk,
                    next_page_token: if is_final {
                        String::new()
                    } else {
                        delivered.to_string()
                    },
                };
                if tx.blocking_send(Ok(resp)).is_err() || is_final {
                    return;
                }
            }
        });
        Ok(Response::new(ReceiverStream::new(rx)))
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

    fn sha(bytes: &[u8]) -> Digest {
        Digest::compute_with(DigestAlgo::Sha256, bytes)
    }

    /// A CAS holding a `root → sub → file` tree; returns `(cas, root reapi::Digest)`.
    fn store_small_tree() -> (Arc<dyn CasBackend>, reapi::Digest) {
        let cas: Arc<dyn CasBackend> = Arc::new(MemCas::new());
        let b = Sha256Boundary;

        let file_bytes = b"hello".to_vec();
        let file = sha(&file_bytes);
        cas.put_keyed(&file, &mut file_bytes.as_slice()).unwrap();

        let sub = reapi::Directory {
            files: vec![reapi::FileNode {
                name: "f".to_string(),
                digest: Some(b.to_reapi(&file, file_bytes.len() as u64)),
                is_executable: false,
                node_properties: None,
            }],
            ..Default::default()
        };
        let sub_bytes = sub.encode_to_vec();
        let sub_digest = sha(&sub_bytes);
        cas.put_keyed(&sub_digest, &mut sub_bytes.as_slice())
            .unwrap();

        let root = reapi::Directory {
            directories: vec![reapi::DirectoryNode {
                name: "sub".to_string(),
                digest: Some(b.to_reapi(&sub_digest, sub_bytes.len() as u64)),
            }],
            ..Default::default()
        };
        let root_bytes = root.encode_to_vec();
        let root_digest = sha(&root_bytes);
        cas.put_keyed(&root_digest, &mut root_bytes.as_slice())
            .unwrap();

        (cas, b.to_reapi(&root_digest, root_bytes.len() as u64))
    }

    async fn drain_tree(svc: &CasV2Svc, req: reapi::GetTreeRequest) -> Vec<reapi::GetTreeResponse> {
        use tokio_stream::StreamExt as _;
        let mut stream = svc.get_tree(Request::new(req)).await.unwrap().into_inner();
        let mut pages = Vec::new();
        while let Some(item) = stream.next().await {
            pages.push(item.unwrap());
        }
        pages
    }

    #[tokio::test]
    async fn get_tree_walks_the_dag_breadth_first() {
        let (cas, root) = store_small_tree();
        let svc = CasV2Svc::new(cas, Arc::new(ReapiConfig::default()));
        let pages = drain_tree(
            &svc,
            reapi::GetTreeRequest {
                root_digest: Some(root),
                ..Default::default()
            },
        )
        .await;
        let dirs: Vec<_> = pages.into_iter().flat_map(|p| p.directories).collect();
        assert_eq!(dirs.len(), 2, "root + sub, each visited once");
        // BFS order: root first (has one child dir named "sub"), then sub (has one file).
        assert_eq!(dirs[0].directories.len(), 1);
        assert_eq!(dirs[0].directories[0].name, "sub");
        assert_eq!(dirs[1].files.len(), 1);
        assert_eq!(dirs[1].files[0].name, "f");
    }

    #[tokio::test]
    async fn get_tree_paginates_with_a_resume_token() {
        let (cas, root) = store_small_tree();
        let svc = CasV2Svc::new(cas, Arc::new(ReapiConfig::default()));
        let pages = drain_tree(
            &svc,
            reapi::GetTreeRequest {
                root_digest: Some(root),
                page_size: 1,
                ..Default::default()
            },
        )
        .await;
        // Two directories, one per page: first page carries a resume token, last is empty.
        assert_eq!(pages.len(), 2);
        assert_eq!(pages[0].directories.len(), 1);
        assert_eq!(pages[0].next_page_token, "1");
        assert_eq!(pages[1].directories.len(), 1);
        assert_eq!(pages[1].next_page_token, "");
    }

    #[tokio::test]
    async fn get_tree_on_a_missing_root_is_not_found() {
        let svc = svc();
        let mut stream = svc
            .get_tree(Request::new(reapi::GetTreeRequest {
                root_digest: Some(reapi_digest(b"never stored")),
                ..Default::default()
            }))
            .await
            .unwrap()
            .into_inner();
        use tokio_stream::StreamExt as _;
        let first = stream.next().await.unwrap();
        assert_eq!(first.unwrap_err().code(), tonic::Code::NotFound);
    }
}
