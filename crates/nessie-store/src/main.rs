//! The `nessie-store` daemon CLI.
//!
//! `nessie-store init` writes a default config; `nessie-store serve` runs the
//! ONTAP REST daemon over the configured backend.

#![forbid(unsafe_code)]

use std::path::PathBuf;
use std::sync::Arc;

use clap::{Parser, Subcommand};
use tracing_subscriber::EnvFilter;

use nessie_backend_core::{PeerId, VolumeBackend};
use nessie_backend_mem::MemBackend;
use nessie_backend_zfs::{SystemRunner, ZfsBackend, ZfsConfig};
use nessie_store::config::{BackendKind, Config};
use nessie_store::identity::Identity;
use nessie_store::{AppState, app};

#[derive(Parser)]
#[command(
    name = "nessie-store",
    version,
    about = "Speaks the ONTAP REST API over pluggable storage"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Write a default configuration file.
    Init {
        /// Path to write the config to.
        #[arg(long, default_value = "nessie-store.toml")]
        config: PathBuf,
    },
    /// Run the daemon.
    Serve {
        /// Path to the config file.
        #[arg(long, default_value = "nessie-store.toml")]
        config: PathBuf,
        /// Serve plain HTTP instead of HTTPS (local/testing only; ONTAP clients expect TLS).
        #[arg(long)]
        no_tls: bool,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    match Cli::parse().cmd {
        Cmd::Init { config } => {
            std::fs::write(&config, Config::default().to_toml())?;
            println!("wrote default config to {}", config.display());
            Ok(())
        }
        Cmd::Serve { config, no_tls } => serve(&config, no_tls).await,
    }
}

async fn serve(config_path: &std::path::Path, no_tls: bool) -> anyhow::Result<()> {
    let cfg = Config::load(config_path)?;
    let identity = Identity::load_or_create(&cfg.identity_path())?;

    let backend: Arc<dyn VolumeBackend> = match cfg.backend {
        BackendKind::Mem => Arc::new(MemBackend::new()),
        BackendKind::Zfs => Arc::new(ZfsBackend::new(
            SystemRunner,
            ZfsConfig {
                pool: cfg.zfs_pool.clone(),
                data_lif: cfg.data_lif.clone(),
                nfs_clients: cfg.zfs_nfs_clients.clone(),
                dataset_owner: cfg.zfs_dataset_owner.clone(),
                dataset_mode: cfg.zfs_dataset_mode.clone(),
                // With the embedded NFS server on, it serves the export-root tree
                // itself — so volume junctions land there and we drive no host
                // kernel exports.
                srv_root: cfg.nfs_export_root.clone(),
                manage_kernel_exports: !cfg.nfs_enabled,
                ..ZfsConfig::default()
            },
        )),
    };

    // Embedded userspace NFS data plane (no host kernel NFS server). Runs on the
    // same tokio runtime as the HTTP control plane; exits only on a fatal error.
    if cfg.nfs_enabled {
        let bind = cfg.nfs_listen.clone();
        let root = cfg.nfs_export_root.clone();
        let export = cfg.nfs_export_name.clone();
        tokio::spawn(async move {
            if let Err(e) = nessie_nfs::serve(root, &bind, &export).await {
                tracing::error!(%e, "embedded NFS server exited");
            }
        });
    }

    // Optional content-addressed store node: run retention maintenance (durable GC
    // or cache eviction) on a schedule. In-memory for now; a persistent backend and
    // a real network router slot in behind the same seams.
    if let Some(cas_cfg) = &cfg.cas {
        // The node's swarm routing identity. `data_lif` is a reasonable placeholder
        // until a dedicated peer-address config lands.
        let me = PeerId::new(cfg.data_lif.clone());
        let node = std::sync::Arc::new(nessie_store::cas_node::CasNode::from_config(cas_cfg, me)?);
        tracing::info!(mode = ?cas_cfg.mode, "cas node enabled; scheduling retention maintenance");
        nessie_store::cas_node::spawn_maintenance(node);
    }

    // Optional REAPI (Bazel remote cache) gRPC face, beside the ONTAP control plane —
    // a Bazel remote cache with no BuildBarn to stand up. SHA-256-native, self-attesting
    // (k=1 write-through). A persistent backend and real ed25519 signers slot in behind
    // the same seams later.
    if let Some(rc) = cfg.reapi.clone().filter(|r| r.enabled) {
        spawn_reapi(rc)?;
    }

    let listen = cfg.listen;
    let tls_dir = cfg.tls_dir();
    let router = app(AppState::new(backend, Arc::new(cfg), Arc::new(identity)));

    if no_tls {
        let listener = tokio::net::TcpListener::bind(listen).await?;
        tracing::info!(%listen, "nessie-store listening (HTTP, --no-tls)");
        axum::serve(listener, router).await?;
    } else {
        // ONTAP REST is HTTPS-only. Install a rustls crypto provider, ensure a
        // server cert, and serve TLS.
        let _ = rustls::crypto::ring::default_provider().install_default();
        let certs = nessie_store::tls::ensure_cert(&tls_dir)?;
        let tls_config =
            axum_server::tls_rustls::RustlsConfig::from_pem_file(&certs.cert, &certs.key).await?;
        tracing::info!(%listen, "nessie-store listening (HTTPS)");
        axum_server::bind_rustls(listen, tls_config)
            .serve(router.into_make_service())
            .await?;
    }
    Ok(())
}

/// Verify the wired REAPI backend honors `put_keyed` (the SHA-256-native write seam)
/// at startup, so a misconfigured backend fails loudly here instead of per-request.
fn probe_put_keyed(cas: &dyn nessie_backend_core::CasBackend) -> anyhow::Result<()> {
    use nessie_backend_core::{Digest, DigestAlgo};
    let probe = b"nessie-reapi put_keyed startup probe";
    let digest = Digest::compute_with(DigestAlgo::Sha256, probe);
    cas.put_keyed(&digest, &mut probe.as_slice())
        .map_err(|e| anyhow::anyhow!("REAPI backend does not support put_keyed: {e}"))?;
    Ok(())
}

/// Build the in-memory, self-attesting REAPI cache backend and spawn the tonic server.
fn spawn_reapi(rc: nessie_store::config::ReapiServerConfig) -> anyhow::Result<()> {
    use nessie_backend_core::CasBackend;
    use nessie_backend_mem::{MemActionCache, MemCas};
    use std::num::NonZeroUsize;

    // The signer and the AC backend's verifier are one matched dev keypair (k=1
    // write-through); a real ed25519 signer from agent-mesh replaces DevSelfSigner in
    // a swarm.
    let signer = nessie_reapi::DevSelfSigner::new("nessie-reapi-self");
    let verifier = signer.verifier();
    let k1 = NonZeroUsize::new(1).expect("1 is nonzero");
    let backend: Arc<dyn CasBackend> = Arc::new(MemActionCache::new(MemCas::new(), verifier, k1));
    probe_put_keyed(backend.as_ref())?;

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
