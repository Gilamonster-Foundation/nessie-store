//! Wiring the cache protocol faces onto **one** content-addressed backend.
//!
//! The daemon serves two protocols over the same blobs: the REAPI gRPC face
//! (`nessie-reapi`) and the S3 object face (`nessie-s3`). They are not integrated
//! with each other and do not need to be — a blob written over one is readable
//! over the other because both name it by the same digest. That only holds if the
//! composition root hands them the *same* store, which is what
//! [`build_faces`] exists to guarantee; building a store per face would
//! quietly break the property while every test still passed.

use crate::cas_node::CasNode;
use crate::config::{Config, ReapiServerConfig, S3ServerConfig};
use nessie_backend_core::{CasBackend, PeerId};
use nessie_reapi::DevSelfSigner;
use std::sync::Arc;

/// The cache faces' shared backend, plus the node (if any) that owns its blobs.
pub struct Faces {
    /// The `[cas]` node, when configured: spawn its maintenance, hold it alive.
    pub node: Option<Arc<CasNode>>,
    /// The one backend REAPI and S3 serve; `None` when neither face is enabled.
    pub backend: Option<(Arc<dyn CasBackend>, DevSelfSigner)>,
}

/// The daemon's wiring entry point: validate the cache faces, build the `[cas]`
/// node, and hand both faces **one** backend. `main` calls exactly this, so tests
/// through it fail if the real wiring ever builds a store per consumer.
///
/// With `[cas]` present, REAPI, S3 and retention maintenance share the node's
/// store (disk-backed if so configured). Without it, the faces share one private
/// in-memory store.
///
/// # Errors
///
/// An enabled S3 face still on the default secret; a bad `[cas]` block; a backend
/// without `put_keyed`.
pub fn build_faces(cfg: &Config) -> anyhow::Result<Faces> {
    let s3_on = cfg.s3.as_ref().is_some_and(|s| s.enabled);
    let reapi_on = cfg.reapi.as_ref().is_some_and(|r| r.enabled);
    if let Some(sc) = cfg.s3.as_ref().filter(|s| s.enabled) {
        check_s3_secret(sc)?;
    }
    let node = cfg
        .cas
        .as_ref()
        .map(|c| CasNode::from_config(c, PeerId::new(cfg.data_lif.clone())).map(Arc::new))
        .transpose()?;
    let backend = if reapi_on || s3_on {
        Some(build_cas_backend(node.clone())?)
    } else {
        None
    };
    Ok(Faces { node, backend })
}

/// Refuse the default S3 secret: a well-known credential on a network port is not
/// a warning-level problem.
fn check_s3_secret(sc: &S3ServerConfig) -> anyhow::Result<()> {
    anyhow::ensure!(
        sc.secret_key != S3ServerConfig::default().secret_key,
        "[s3] secret_key is the default; set NESSIE_S3_SECRET_KEY (or secret_key) before enabling the S3 face"
    );
    Ok(())
}

/// Build the self-attesting backend both cache faces share, and verify it honors
/// `put_keyed` before anything binds a port.
///
/// Blob bodies go to `node`'s store when given, else a private in-memory one. The
/// signer and the action cache's verifier are one matched dev keypair (k=1
/// write-through); a real ed25519 signer from agent-mesh replaces `DevSelfSigner`
/// in a swarm.
///
/// # Errors
///
/// If the backend does not honor `put_keyed` — the SHA-256-native write seam both
/// faces depend on — so a misconfiguration fails loudly at startup rather than
/// per-request.
pub fn build_cas_backend(
    node: Option<Arc<CasNode>>,
) -> anyhow::Result<(Arc<dyn CasBackend>, DevSelfSigner)> {
    use nessie_backend_mem::{MemActionCache, MemCas};
    use std::num::NonZeroUsize;

    let signer = DevSelfSigner::new("nessie-reapi-self");
    let verifier = signer.verifier();
    let k1 = NonZeroUsize::new(1).expect("1 is nonzero");
    let bodies: Arc<dyn CasBackend> = match node {
        Some(n) => n,
        None => Arc::new(MemCas::new()),
    };
    let backend: Arc<dyn CasBackend> = Arc::new(MemActionCache::shared(bodies, verifier, k1));
    probe_put_keyed(backend.as_ref())?;
    Ok((backend, signer))
}

/// Verify the wired backend honors `put_keyed` at startup.
fn probe_put_keyed(cas: &dyn CasBackend) -> anyhow::Result<()> {
    use nessie_backend_core::{Digest, DigestAlgo};
    let probe = b"nessie cache-face put_keyed startup probe";
    let digest = Digest::compute_with(DigestAlgo::Sha256, probe);
    cas.put_keyed(&digest, &mut probe.as_slice())
        .map_err(|e| anyhow::anyhow!("cache backend does not support put_keyed: {e}"))?;
    Ok(())
}

/// Spawn the tonic REAPI server over the shared backend.
///
/// # Errors
///
/// Currently infallible; returns `Result` so a future binding/validation step can
/// fail here without changing the call site.
pub fn spawn_reapi(
    rc: ReapiServerConfig,
    backend: Arc<dyn CasBackend>,
    signer: DevSelfSigner,
) -> anyhow::Result<()> {
    let reapi_cfg = nessie_reapi::ReapiConfig {
        instance_name: rc.instance_name.clone(),
        ac_update_enabled: rc.ac_update_enabled,
        ..Default::default()
    };
    let signer: Arc<dyn nessie_reapi::AttestationSigner> = Arc::new(signer);
    let router = nessie_reapi::build_router(backend, Some(signer), reapi_cfg);

    let addr = rc.listen;
    tracing::info!(
        %addr,
        instance = %rc.instance_name,
        ac_update = rc.ac_update_enabled,
        "REAPI cache face enabled (SHA-256-native, self-attesting)"
    );
    tokio::spawn(async move {
        if let Err(e) = router.serve(addr).await {
            tracing::error!(%e, "REAPI gRPC server exited");
        }
    });
    Ok(())
}

/// Build the axum app that serves the S3 face.
///
/// `S3Service` is a `tower::Service`, so it mounts as an axum fallback exactly like
/// the ONTAP surface — no second HTTP stack. Axum's `fallback_service` requires
/// `Error = Infallible`, hence the `HandleError` wrapper.
pub fn s3_app(sc: &S3ServerConfig, backend: Arc<dyn CasBackend>) -> axum::Router {
    let service = nessie_s3::build_service(backend, sc.access_key.clone(), sc.secret_key.clone());
    axum::Router::new().fallback_service(axum::error_handling::HandleError::<_, _, ()>::new(
        service,
        handle_s3_transport_error,
    ))
}

/// Last-resort handler for a *transport*-level S3 failure.
///
/// S3 application errors (`NoSuchKey`, `InvalidDigest`, …) are already rendered as
/// S3 XML by `S3Service` and never reach here; this only covers a broken connection.
async fn handle_s3_transport_error(err: nessie_s3::HttpError) -> axum::http::StatusCode {
    tracing::error!(?err, "S3 transport error");
    axum::http::StatusCode::INTERNAL_SERVER_ERROR
}

/// Spawn the S3 object-API server over the **same** backend the REAPI face serves.
///
/// # Errors
///
/// Currently infallible; the bind happens inside the spawned task so a port clash
/// is logged rather than taking down the control plane.
pub fn spawn_s3(sc: S3ServerConfig, backend: Arc<dyn CasBackend>) -> anyhow::Result<()> {
    let app = s3_app(&sc, backend);
    let addr = sc.listen;
    tracing::info!(
        %addr,
        access_key = %sc.access_key,
        "S3 cache face enabled (content-addressed keys only, SigV4)"
    );
    tokio::spawn(async move {
        match tokio::net::TcpListener::bind(addr).await {
            Ok(listener) => {
                if let Err(e) = axum::serve(listener, app).await {
                    tracing::error!(%e, "S3 server exited");
                }
            }
            Err(e) => tracing::error!(%e, %addr, "S3 server could not bind"),
        }
    });
    Ok(())
}
