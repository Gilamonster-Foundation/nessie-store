//! TLS for the cache faces (`[tls] mtls = true`).
//!
//! * **REAPI** terminates TLS with the node certificate and *requires* a client
//!   certificate the [`MeshVerifier`] admits. tonic 0.12 cannot take a custom
//!   client verifier through `ServerTlsConfig`, so we accept TLS ourselves and
//!   hand tonic the finished streams via `serve_with_incoming`.
//! * **S3** serves HTTPS with the same certificate and no client certificate:
//!   SigV4 stays the authentication, and clients pin [`FaceTls::cert_sha256`].

use std::net::TcpListener as StdListener;
use std::sync::Arc;

use anyhow::Context as _;
use nessie_mtls::{AuthorizedKeys, MeshVerifier, NodeIdentity};

use crate::config::Config;

/// Names every node certificate carries, so loopback clients can verify it.
const LOCAL_NAMES: [&str; 3] = ["localhost", "127.0.0.1", "::1"];

/// The node certificate and the client verifier the cache faces share.
pub struct FaceTls {
    node: NodeIdentity,
    verifier: Arc<MeshVerifier>,
}

impl FaceTls {
    /// `node`'s certificate (with `names` added to the loopback names) and a
    /// verifier trusting exactly `authorized`.
    pub fn new(
        node: NodeIdentity,
        authorized: AuthorizedKeys,
        names: &[String],
    ) -> anyhow::Result<Self> {
        Self::with_verifier(node, Arc::new(MeshVerifier::new(authorized)), names)
    }

    /// Like [`new`](Self::new) with a ready-made verifier (e.g. one honouring a
    /// revocation log).
    pub fn with_verifier(
        node: NodeIdentity,
        verifier: Arc<MeshVerifier>,
        names: &[String],
    ) -> anyhow::Result<Self> {
        Ok(Self {
            node: served_node(node, names)?,
            verifier,
        })
    }

    /// `Some` iff `[tls] mtls = true`: loads the node identity and
    /// `authorized_keys` named by `cfg`. Fails closed if the identity is missing.
    pub fn from_config(cfg: &Config) -> anyhow::Result<Option<Arc<Self>>> {
        if !cfg.mtls_enabled() {
            return Ok(None);
        }
        let dir = cfg.mesh_identity_dir();
        let node = NodeIdentity::load(&dir).with_context(|| {
            format!(
                "[tls] mtls = true needs a node identity in {} (run `nessie-store identity issue`)",
                dir.display()
            )
        })?;
        let verifier = crate::peer_tls::mesh_verifier(cfg, &node)?;
        Ok(Some(Arc::new(Self::with_verifier(
            node,
            verifier,
            &cfg_names(cfg),
        )?)))
    }

    /// Lowercase hex SHA-256 of the served certificate: what S3 clients pin.
    pub fn cert_sha256(&self) -> String {
        self.node.cert_sha256()
    }

    /// The served certificate, PEM.
    pub fn cert_pem(&self) -> String {
        self.node.cert_pem()
    }

    fn reapi_config(&self) -> anyhow::Result<Arc<rustls::ServerConfig>> {
        Ok(Arc::new(nessie_mtls::server_config(
            &self.node,
            Some(self.verifier.clone()),
            &[b"h2"],
        )?))
    }

    fn s3_config(&self) -> anyhow::Result<Arc<rustls::ServerConfig>> {
        Ok(Arc::new(nessie_mtls::server_config(
            &self.node,
            None,
            &[b"h2", b"http/1.1"],
        )?))
    }
}

fn cfg_names(cfg: &Config) -> Vec<String> {
    cfg.tls
        .as_ref()
        .map(|t| t.names.clone())
        .unwrap_or_default()
}

/// `node` with the certificate names the faces serve: loopback plus `names`.
fn served_node(node: NodeIdentity, names: &[String]) -> anyhow::Result<NodeIdentity> {
    let all: Vec<String> = LOCAL_NAMES
        .iter()
        .map(|n| (*n).to_owned())
        .chain(names.iter().cloned())
        .collect();
    Ok(node.with_names(&all)?)
}

/// The SHA-256 fingerprint of the certificate the faces serve for `cfg`, read
/// from the identity on disk (what `identity show` prints).
pub fn served_cert_sha256(cfg: &Config) -> anyhow::Result<(String, String)> {
    let node = NodeIdentity::load(&cfg.mesh_identity_dir())
        .context("loading the node identity (run `nessie-store identity issue`)")?;
    let node = served_node(node, &cfg_names(cfg))?;
    Ok((node.fingerprint().hex(), node.cert_sha256()))
}

/// What `identity show` says about revocation: the loaded log's counter and
/// generation, or that none is configured.
///
/// # Errors
///
/// A configured log that is missing, invalid or rolled back (the same refusal
/// the daemon makes).
pub fn revocation_summary(cfg: &Config) -> anyhow::Result<String> {
    let Some(log) = cfg.revocation_log() else {
        return Ok("no revocation log configured".to_owned());
    };
    let node = NodeIdentity::load(&cfg.mesh_identity_dir())
        .context("loading the node identity (run `nessie-store identity issue`)")?;
    let guard = agent_mesh_protocol::RevocationGuard::open(&log, node.user_fingerprint())
        .with_context(|| format!("[tls] revocation_log {} is not usable", log.display()))?;
    let state = guard.state()?;
    Ok(format!(
        "revocation log counter {} generation {}",
        state.len, state.generation
    ))
}

/// Serve `router` on `listener` over mutual TLS until the listener fails.
///
/// Handshakes run off the accept loop, so a stalled or refused peer never delays
/// another; only peers the verifier admitted reach tonic, and every request is
/// re-judged against the revocation state as it stands then.
pub async fn serve_reapi_tls(
    router: tonic::transport::server::Router,
    listener: StdListener,
    tls: &FaceTls,
) -> anyhow::Result<()> {
    // tonic 0.12 keeps `Routes` private inside `Router`; this is its only way out.
    #[allow(deprecated)]
    let app = router.into_router();
    crate::peer_tls::serve_checked(
        listener,
        app,
        tls.reapi_config()?,
        tls.verifier.clone(),
    )
    .await?;
    Ok(())
}

/// Serve the S3 `app` over HTTPS (no client certificate) on `listener`.
pub async fn serve_s3_tls(
    app: axum::Router,
    listener: StdListener,
    tls: &FaceTls,
) -> anyhow::Result<()> {
    listener.set_nonblocking(true)?;
    let config = axum_server::tls_rustls::RustlsConfig::from_config(tls.s3_config()?);
    axum_server::from_tcp_rustls(listener, config)
        .serve(app.into_make_service())
        .await?;
    Ok(())
}
