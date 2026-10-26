//! TLS for the cache faces (`[tls] mtls = true`).
//!
//! * **REAPI** terminates TLS with the node certificate and *requires* a client
//!   certificate the [`MeshVerifier`] admits. tonic 0.12 cannot take a custom
//!   client verifier through `ServerTlsConfig`, so we accept TLS ourselves and
//!   hand tonic the finished streams via `serve_with_incoming`.
//! * **S3** serves HTTPS with the same certificate and no client certificate:
//!   SigV4 stays the authentication, and clients pin [`FaceTls::cert_sha256`].

use std::net::TcpListener as StdListener;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use anyhow::Context as _;
use nessie_mtls::{AuthorizedKeys, MeshVerifier, NodeIdentity};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::TlsAcceptor;
use tokio_rustls::server::TlsStream;
use tonic::transport::server::{Connected, Router};

use crate::config::Config;

/// Names every node certificate carries, so loopback clients can verify it.
const LOCAL_NAMES: [&str; 3] = ["localhost", "127.0.0.1", "::1"];

/// A peer that has not finished its handshake by then is dropped.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

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
        Ok(Self {
            node: served_node(node, names)?,
            verifier: Arc::new(MeshVerifier::new(authorized)),
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
        let authorized = AuthorizedKeys::load(&cfg.authorized_keys_path());
        Ok(Some(Arc::new(Self::new(
            node,
            authorized,
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

/// A finished server-side TLS stream, as tonic wants it.
struct TlsConn(TlsStream<TcpStream>);

impl Connected for TlsConn {
    type ConnectInfo = ();
    fn connect_info(&self) -> Self::ConnectInfo {}
}

impl AsyncRead for TlsConn {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.0).poll_read(cx, buf)
    }
}

impl AsyncWrite for TlsConn {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.0).poll_write(cx, buf)
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.0).poll_flush(cx)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.0).poll_shutdown(cx)
    }
}

fn tokio_listener(l: StdListener) -> std::io::Result<TcpListener> {
    l.set_nonblocking(true)?;
    TcpListener::from_std(l)
}

/// Serve `router` on `listener` over mutual TLS until the listener fails.
///
/// Handshakes run off the accept loop, so a stalled or refused peer never delays
/// another; only peers the verifier admitted reach tonic.
pub async fn serve_reapi_tls(
    router: Router,
    listener: StdListener,
    tls: &FaceTls,
) -> anyhow::Result<()> {
    let acceptor = TlsAcceptor::from(tls.reapi_config()?);
    let listener = tokio_listener(listener)?;
    let (tx, rx) = tokio::sync::mpsc::channel::<std::io::Result<TlsConn>>(64);
    tokio::spawn(async move {
        loop {
            let (tcp, peer) = match listener.accept().await {
                Ok(x) => x,
                Err(e) => {
                    tracing::error!(%e, "REAPI accept failed");
                    // ponytail: a persistent accept error ends the face; supervise if it ever flaps.
                    return;
                }
            };
            let (acceptor, tx) = (acceptor.clone(), tx.clone());
            tokio::spawn(async move {
                match tokio::time::timeout(HANDSHAKE_TIMEOUT, acceptor.accept(tcp)).await {
                    Ok(Ok(s)) => {
                        let _ = tx.send(Ok(TlsConn(s))).await;
                    }
                    Ok(Err(e)) => tracing::debug!(%peer, %e, "REAPI TLS handshake refused"),
                    Err(_) => tracing::debug!(%peer, "REAPI TLS handshake timed out"),
                }
            });
        }
    });
    router
        .serve_with_incoming(tokio_stream::wrappers::ReceiverStream::new(rx))
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
