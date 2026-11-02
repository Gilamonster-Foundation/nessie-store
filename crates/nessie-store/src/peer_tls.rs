//! mTLS for the peer plane: the node's server and client configs, and the listener
//! that serves `/cas/v1` apart from the ONTAP REST port.
//!
//! The peer API has its **own listener** because rustls decides whether to demand a
//! client certificate per connection, before any request path is known; a path
//! rule on the ONTAP listener could not make `/cas/v1` require one while ONTAP
//! clients (which hold no mesh key) keep Basic auth.

use std::future::Future;
use std::net::TcpListener;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use agent_mesh_protocol::RevocationGuard;
use anyhow::Context as _;
use axum::Router;
use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum_server::accept::Accept;
use axum_server::tls_rustls::{RustlsAcceptor, RustlsConfig};
use nessie_mtls::{AuthorizedKeys, MeshVerifier, NodeIdentity, PeerChain};
use tokio::net::TcpStream;
use tokio_rustls::server::TlsStream;

use crate::config::Config;

/// The verifier every mTLS face shares: trusts `authorized_keys`, and honours
/// `[tls] revocation_log` when set (opened now, so a missing, invalid or
/// rolled-back log stops startup instead of being discovered later).
///
/// # Errors
///
/// A configured revocation log that cannot be opened.
pub fn mesh_verifier(cfg: &Config, id: &NodeIdentity) -> anyhow::Result<Arc<MeshVerifier>> {
    let authorized = AuthorizedKeys::load(&cfg.authorized_keys_path());
    Ok(Arc::new(match cfg.revocation_log() {
        Some(log) => {
            let guard = RevocationGuard::open(&log, id.user_fingerprint()).with_context(|| {
                format!("[tls] revocation_log {} is not usable", log.display())
            })?;
            MeshVerifier::with_revocation(authorized, guard)
        }
        None => MeshVerifier::new(authorized),
    }))
}

/// The node's server and client TLS configs and the verifier behind them: its
/// identity under `data_dir/identity/`, trusting exactly the `authorized_keys`
/// file (and revocation log, if configured).
///
/// # Errors
///
/// A missing or unreadable identity (run `nessie-store identity issue`), or an
/// unusable revocation log.
pub fn mesh_configs(
    cfg: &Config,
) -> anyhow::Result<(
    Arc<rustls::ServerConfig>,
    Arc<rustls::ClientConfig>,
    Arc<MeshVerifier>,
)> {
    let dir = cfg.mesh_identity_dir();
    let id = NodeIdentity::load(&dir)
        .with_context(|| format!("[tls] mtls = true needs an identity in {}", dir.display()))?;
    let verifier = mesh_verifier(cfg, &id)?;
    Ok((
        Arc::new(nessie_mtls::server_config(
            &id,
            Some(verifier.clone()),
            &[b"http/1.1"],
        )?),
        Arc::new(nessie_mtls::client_config(&id, verifier.clone())?),
        verifier,
    ))
}

/// Accepts TLS like axum-server's stock acceptor, then stashes the peer's
/// verified chain on the connection's service.
#[derive(Clone, Debug)]
struct ChainAcceptor(RustlsAcceptor);

/// A connection's service that tags every request with the connection's peer chain.
#[derive(Clone, Debug)]
pub struct WithChain<S> {
    inner: S,
    chain: Option<PeerChain>,
}

impl<S, B> tower::Service<axum::http::Request<B>> for WithChain<S>
where
    S: tower::Service<axum::http::Request<B>>,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = S::Future;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, mut req: axum::http::Request<B>) -> Self::Future {
        if let Some(c) = &self.chain {
            req.extensions_mut().insert(c.clone());
        }
        self.inner.call(req)
    }
}

type Accepted<S> = std::io::Result<(TlsStream<TcpStream>, WithChain<S>)>;

impl<S: Send + 'static> Accept<TcpStream, S> for ChainAcceptor {
    type Stream = TlsStream<TcpStream>;
    type Service = WithChain<S>;
    type Future = Pin<Box<dyn Future<Output = Accepted<S>> + Send>>;

    fn accept(&self, stream: TcpStream, service: S) -> Self::Future {
        let tls = self.0.accept(stream, service);
        Box::pin(async move {
            let (stream, inner) = tls.await?;
            let chain = stream
                .get_ref()
                .1
                .peer_certificates()
                .and_then(|c| c.first())
                .and_then(|c| PeerChain::from_certificate(c).ok());
            Ok((stream, WithChain { inner, chain }))
        })
    }
}

/// Refuse a request whose connection's peer chain is no longer admitted:
/// revocation reaches connections that were open before it.
async fn recheck(State(v): State<Arc<MeshVerifier>>, req: Request, next: Next) -> Response {
    // A connection without a chain cannot have passed the handshake; refuse anyway.
    let verdict = match req.extensions().get::<PeerChain>() {
        Some(c) => v.recheck(&c.0).map_err(|e| e.to_string()),
        None => Err("no peer chain on the connection".to_owned()),
    };
    match verdict {
        Ok(()) => next.run(req).await,
        Err(reason) => {
            tracing::warn!(%reason, "mTLS request refused");
            StatusCode::FORBIDDEN.into_response()
        }
    }
}

/// Serve `app` on `listener` over TLS with `server`, which demands a client
/// certificate; every request is re-judged by `verifier` against the revocation
/// state as it stands then. Used by the peer API and REAPI.
///
/// # Errors
///
/// A listener that cannot be adopted, or the server exiting.
pub async fn serve_checked(
    listener: TcpListener,
    app: Router,
    server: Arc<rustls::ServerConfig>,
    verifier: Arc<MeshVerifier>,
) -> std::io::Result<()> {
    listener.set_nonblocking(true)?;
    let app = app.layer(axum::middleware::from_fn_with_state(verifier, recheck));
    axum_server::from_tcp(listener)
        .acceptor(ChainAcceptor(RustlsAcceptor::new(RustlsConfig::from_config(
            server,
        ))))
        .serve(app.into_make_service())
        .await
}

/// [`serve_checked`] for the peer API. A peer the verifier refuses fails the
/// handshake: no request is read.
///
/// # Errors
///
/// A listener that cannot be adopted, or the server exiting.
pub async fn serve_peer(
    listener: TcpListener,
    app: Router,
    server: Arc<rustls::ServerConfig>,
    verifier: Arc<MeshVerifier>,
) -> std::io::Result<()> {
    serve_checked(listener, app, server, verifier).await
}
