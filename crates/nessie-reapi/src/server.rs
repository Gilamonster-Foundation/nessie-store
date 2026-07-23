//! Assemble the cache-subset services into a ready-to-serve tonic router.
//!
//! [`build_router`] is the one seam the daemon needs: hand it a backend (and, for
//! writes, a signer) and it wires Capabilities + CAS + ByteStream — plus ActionCache
//! when the backend is AC-capable — onto a [`Router`] the caller drives with
//! `.serve(addr)`. All service-wrapper knowledge stays here; the daemon chooses the
//! backend.

use crate::config::ReapiConfig;
use crate::services::{ActionCacheSvc, ByteStreamSvc, CapabilitiesSvc, CasV2Svc};
use crate::signer::AttestationSigner;
use crate::{bytestream, reapi};
use nessie_backend_core::CasBackend;
use std::sync::Arc;
use tonic::transport::Server;
use tonic::transport::server::Router;

/// Wire the cache-subset services onto a tonic [`Router`].
///
/// Capabilities, CAS, and ByteStream are always served. ActionCache is added only when
/// `cas.as_action_cache()` is `Some` (an honest decline otherwise), and `signer` is
/// required to serve `UpdateActionResult` — without one, updates return `UNIMPLEMENTED`.
#[must_use]
pub fn build_router(
    cas: Arc<dyn CasBackend>,
    signer: Option<Arc<dyn AttestationSigner>>,
    cfg: ReapiConfig,
) -> Router {
    let cfg = Arc::new(cfg);
    let ac_enabled = cas.as_action_cache().is_some();

    let caps = reapi::capabilities_server::CapabilitiesServer::new(CapabilitiesSvc::new(
        cfg.clone(),
        ac_enabled,
    ));
    let cas_svc = reapi::content_addressable_storage_server::ContentAddressableStorageServer::new(
        CasV2Svc::new(cas.clone(), cfg.clone()),
    );
    let bs = bytestream::byte_stream_server::ByteStreamServer::new(ByteStreamSvc::new(cas.clone()));

    let router = Server::builder()
        .add_service(caps)
        .add_service(cas_svc)
        .add_service(bs);

    if ac_enabled {
        router.add_service(reapi::action_cache_server::ActionCacheServer::new(
            ActionCacheSvc::new(cas, signer, cfg),
        ))
    } else {
        router
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::DevSelfSigner;
    use nessie_backend_mem::{MemActionCache, MemCas};
    use std::num::NonZeroUsize;

    #[test]
    fn builds_router_for_ac_and_non_ac_backends() {
        // A plain CAS (no ActionCache): Capabilities + CAS + ByteStream only.
        let plain: Arc<dyn CasBackend> = Arc::new(MemCas::new());
        let _ = build_router(plain, None, ReapiConfig::default());

        // An AC-capable backend: the ActionCache branch wires the fourth service.
        let signer = DevSelfSigner::new("t");
        let backend: Arc<dyn CasBackend> = Arc::new(MemActionCache::new(
            MemCas::new(),
            signer.verifier(),
            NonZeroUsize::new(1).unwrap(),
        ));
        let _ = build_router(backend, Some(Arc::new(signer)), ReapiConfig::default());
    }
}
