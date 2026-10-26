//! mTLS for the peer plane: the node's server and client configs, and the listener
//! that serves `/cas/v1` apart from the ONTAP REST port.
//!
//! The peer API has its **own listener** because rustls decides whether to demand a
//! client certificate per connection, before any request path is known; a path
//! rule on the ONTAP listener could not make `/cas/v1` require one while ONTAP
//! clients (which hold no mesh key) keep Basic auth.

use std::net::TcpListener;
use std::sync::Arc;

use anyhow::Context as _;
use axum::Router;
use nessie_mtls::{AuthorizedKeys, MeshVerifier, NodeIdentity};

use crate::config::Config;

/// The node's server and client TLS configs: its identity under
/// `data_dir/identity/`, trusting exactly the `authorized_keys` file.
///
/// # Errors
///
/// A missing or unreadable identity (run `nessie-store identity issue`).
pub fn mesh_configs(
    cfg: &Config,
) -> anyhow::Result<(Arc<rustls::ServerConfig>, Arc<rustls::ClientConfig>)> {
    let dir = cfg.mesh_identity_dir();
    let id = NodeIdentity::load(&dir)
        .with_context(|| format!("[tls] mtls = true needs an identity in {}", dir.display()))?;
    let verifier = Arc::new(MeshVerifier::new(AuthorizedKeys::load(
        &cfg.authorized_keys_path(),
    )));
    Ok((
        nessie_mtls::server_config(&id, verifier.clone())?,
        nessie_mtls::client_config(&id, verifier)?,
    ))
}

/// Serve `app` on `listener` over TLS with `server`, which demands a client
/// certificate. A peer the verifier refuses fails the handshake: no request is read.
///
/// # Errors
///
/// A listener that cannot be adopted, or the server exiting.
pub async fn serve_peer(
    listener: TcpListener,
    app: Router,
    server: Arc<rustls::ServerConfig>,
) -> std::io::Result<()> {
    listener.set_nonblocking(true)?;
    axum_server::from_tcp_rustls(
        listener,
        axum_server::tls_rustls::RustlsConfig::from_config(server),
    )
    .serve(app.into_make_service())
    .await
}
