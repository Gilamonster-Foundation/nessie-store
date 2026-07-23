//! The `ActionCache` service — the ungameable-completion keystone, over the wire.
//!
//! `GetActionResult` asks "is there a **confirmed** AC entry for `digest(Action)`?" —
//! and the Action digest pins the immutable spec, so the answer cannot be faked by
//! weakening the spec. `UpdateActionResult` stores the result body and **self-attests**
//! it (a `k = 1` write-through). Both dispatch to the backend's `ActionCacheBackend`
//! tier (`UNIMPLEMENTED` if the instance has no action cache).

use crate::boundary::Sha256Boundary;
use crate::config::ReapiConfig;
use crate::map::{ar_from_reapi, ar_to_reapi};
use crate::reapi;
use crate::signer::AttestationSigner;
use crate::size::CasSizeSource;
use crate::status::status_from_backend;
use nessie_backend_core::{BackendError, CasBackend};
use std::sync::Arc;
use tonic::{Request, Response, Status};

/// The `ActionCache` gRPC service.
pub struct ActionCacheSvc {
    cas: Arc<dyn CasBackend>,
    boundary: Sha256Boundary,
    signer: Option<Arc<dyn AttestationSigner>>,
    cfg: Arc<ReapiConfig>,
}

impl ActionCacheSvc {
    /// Build the service. `signer` is required for `UpdateActionResult`; without one,
    /// updates return `UNIMPLEMENTED`.
    #[must_use]
    pub fn new(
        cas: Arc<dyn CasBackend>,
        signer: Option<Arc<dyn AttestationSigner>>,
        cfg: Arc<ReapiConfig>,
    ) -> Self {
        Self {
            cas,
            boundary: Sha256Boundary,
            signer,
            cfg,
        }
    }
}

#[tonic::async_trait]
impl reapi::action_cache_server::ActionCache for ActionCacheSvc {
    async fn get_action_result(
        &self,
        request: Request<reapi::GetActionResultRequest>,
    ) -> Result<Response<reapi::ActionResult>, Status> {
        let req = request.into_inner();
        let action_digest = req
            .action_digest
            .ok_or_else(|| Status::invalid_argument("missing action_digest"))?;
        let action = self.boundary.to_native(&action_digest)?;
        let cas = self.cas.clone();
        let boundary = self.boundary;
        tokio::task::spawn_blocking(move || {
            let ac = cas
                .as_action_cache()
                .ok_or_else(|| Status::unimplemented("this instance has no action cache"))?;
            match ac.get_action_result(&action) {
                Ok(Some(ar)) => {
                    let sizes = CasSizeSource(cas.clone());
                    Ok(Response::new(ar_to_reapi(&ar, &boundary, &sizes)?))
                }
                Ok(None) => Err(Status::not_found("no cached result for this action")),
                // A conflict (Byzantine ≥ k / non-determinism) or a confirmed-but-absent
                // body both read as a cache miss to the client.
                Err(BackendError::ActionResultConflict { .. } | BackendError::BlobNotFound(_)) => {
                    Err(Status::not_found("no confirmed result for this action"))
                }
                Err(e) => Err(status_from_backend(&e)),
            }
        })
        .await
        .map_err(|e| Status::internal(format!("get_action_result task panicked: {e}")))?
    }

    async fn update_action_result(
        &self,
        request: Request<reapi::UpdateActionResultRequest>,
    ) -> Result<Response<reapi::ActionResult>, Status> {
        if !self.cfg.ac_update_enabled {
            return Err(Status::permission_denied(
                "action cache updates are disabled",
            ));
        }
        let signer = self
            .signer
            .clone()
            .ok_or_else(|| Status::unimplemented("no signer configured for updates"))?;
        let req = request.into_inner();
        let action_digest = req
            .action_digest
            .ok_or_else(|| Status::invalid_argument("missing action_digest"))?;
        let action = self.boundary.to_native(&action_digest)?;
        let proto_ar = req
            .action_result
            .ok_or_else(|| Status::invalid_argument("missing action_result"))?;
        let native_ar = ar_from_reapi(&proto_ar, &self.boundary)?;
        let cas = self.cas.clone();
        tokio::task::spawn_blocking(move || {
            let ac = cas
                .as_action_cache()
                .ok_or_else(|| Status::unimplemented("this instance has no action cache"))?;
            // Store the ActionResult body under its own (result) digest.
            let body = native_ar.to_canonical_bytes();
            cas.put(&mut body.as_slice())
                .map_err(|e| status_from_backend(&e))?;
            let result_digest = native_ar.result_digest();
            // Self-attest that `action` produced this result (a k=1 write-through).
            let signed = signer.sign_statement(&action, &result_digest);
            ac.attest_action_result(&action, signed)
                .map_err(|e| status_from_backend(&e))?;
            Ok(Response::new(proto_ar))
        })
        .await
        .map_err(|e| Status::internal(format!("update_action_result task panicked: {e}")))?
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::signer::DevSelfSigner;
    use nessie_backend_core::{Digest, DigestAlgo};
    use nessie_backend_mem::{MemActionCache, MemCas};
    use reapi::action_cache_server::ActionCache;
    use std::num::NonZeroUsize;

    fn sha(bytes: &[u8]) -> Digest {
        Digest::compute_with(DigestAlgo::Sha256, bytes)
    }

    fn harness() -> (ActionCacheSvc, Arc<dyn CasBackend>) {
        let signer = DevSelfSigner::new("dev");
        let verifier = signer.verifier();
        let backend = MemActionCache::new(MemCas::new(), verifier, NonZeroUsize::new(1).unwrap());
        let cas: Arc<dyn CasBackend> = Arc::new(backend);
        let svc = ActionCacheSvc::new(
            cas.clone(),
            Some(Arc::new(signer)),
            Arc::new(ReapiConfig::default()),
        );
        (svc, cas)
    }

    #[tokio::test]
    async fn update_then_get_is_the_keystone_round_trip() {
        let (svc, cas) = harness();
        // Upload an output blob (SHA-256), keyed like a real client would.
        let out_bytes = b"the build output".to_vec();
        let out = sha(&out_bytes);
        cas.put_keyed(&out, &mut out_bytes.as_slice()).unwrap();

        let action_digest = Sha256Boundary.to_reapi(&sha(b"the action"), 0);
        let proto_ar = reapi::ActionResult {
            output_files: vec![reapi::OutputFile {
                path: "out.txt".to_string(),
                digest: Some(Sha256Boundary.to_reapi(&out, out_bytes.len() as u64)),
                is_executable: false,
                contents: Vec::new(),
                node_properties: None,
            }],
            exit_code: 0,
            ..Default::default()
        };

        // A miss before the write.
        assert_eq!(
            svc.get_action_result(Request::new(reapi::GetActionResultRequest {
                action_digest: Some(action_digest.clone()),
                ..Default::default()
            }))
            .await
            .unwrap_err()
            .code(),
            tonic::Code::NotFound
        );

        // Write-through (self-attest, k=1).
        svc.update_action_result(Request::new(reapi::UpdateActionResultRequest {
            action_digest: Some(action_digest.clone()),
            action_result: Some(proto_ar.clone()),
            ..Default::default()
        }))
        .await
        .expect("update");

        // Now GetActionResult confirms it — the ungameable-completion keystone.
        let got = svc
            .get_action_result(Request::new(reapi::GetActionResultRequest {
                action_digest: Some(action_digest),
                ..Default::default()
            }))
            .await
            .expect("get")
            .into_inner();
        assert_eq!(got.output_files.len(), 1);
        assert_eq!(got.output_files[0].path, "out.txt");
        assert_eq!(
            got.output_files[0].digest.as_ref().unwrap().size_bytes,
            out_bytes.len() as i64
        );
        assert_eq!(got.exit_code, 0);
    }

    #[tokio::test]
    async fn get_on_an_unknown_action_is_not_found() {
        let (svc, _cas) = harness();
        let err = svc
            .get_action_result(Request::new(reapi::GetActionResultRequest {
                action_digest: Some(Sha256Boundary.to_reapi(&sha(b"never attested"), 0)),
                ..Default::default()
            }))
            .await
            .unwrap_err();
        assert_eq!(err.code(), tonic::Code::NotFound);
    }
}
