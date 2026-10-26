//! The `nessie-store` daemon CLI.
//!
//! `nessie-store init` writes a default config; `nessie-store serve` runs the
//! ONTAP REST daemon over the configured backend.

#![forbid(unsafe_code)]

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context as _;
use clap::{Parser, Subcommand};
use tracing_subscriber::EnvFilter;

use nessie_backend_core::VolumeBackend;
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
    /// Manage this node's agent-mesh identity (mTLS).
    Identity {
        #[command(subcommand)]
        cmd: IdentityCmd,
    },
}

#[derive(Subcommand)]
enum IdentityCmd {
    /// Issue this node's agent key from the operator's user key and write
    /// `agent.seed`, `agent.chain.json` and `binding.json` under `<data_dir>/identity/`.
    /// Never prints the seed.
    Issue {
        /// Path to the config file (locates `data_dir`).
        #[arg(long, default_value = "nessie-store.toml")]
        config: PathBuf,
        /// The operator's agent-mesh user key (PKCS#8 PEM).
        #[arg(long)]
        user_key: PathBuf,
        /// The user's GitHubBinding JSON (`amesh bind github` output).
        #[arg(long)]
        binding: PathBuf,
        /// Role label recorded in the signed chain.
        #[arg(long, default_value = "nessie")]
        role: String,
        /// Host label recorded in the signed chain.
        #[arg(long, default_value = "nessie")]
        host: String,
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
        Cmd::Identity {
            cmd:
                IdentityCmd::Issue {
                    config,
                    user_key,
                    binding,
                    role,
                    host,
                },
        } => {
            let dir = Config::load(&config)?.mesh_identity_dir();
            let fp = nessie_store::identity_cmd::issue(&user_key, &binding, &role, &host, &dir)?;
            println!("issued agent {} into {}", fp.hex(), dir.display());
            Ok(())
        }
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

    // Optional `[cas]` node plus the REAPI / S3 cache faces, wired by one entry point
    // so they share ONE content-addressed store (disk-backed if `[cas]` says so):
    // a blob written over REAPI gRPC is the blob an S3 client reads, and the store
    // retention maintenance sweeps. Building a store per consumer would break that.
    let faces = nessie_store::faces::build_faces(&cfg)?;
    if let (Some(node), Some(cas_cfg)) = (&faces.node, &cfg.cas) {
        tracing::info!(mode = ?cas_cfg.mode, backend = ?cas_cfg.backend, "cas node enabled; scheduling retention maintenance");
        nessie_store::cas_node::spawn_maintenance(node.clone());
    }
    if let Some((backend, signer)) = faces.backend {
        if let Some(rc) = cfg.reapi.clone().filter(|r| r.enabled) {
            nessie_store::faces::spawn_reapi(rc, backend.clone(), signer)?;
        }
        if let Some(sc) = cfg.s3.clone().filter(|s| s.enabled) {
            nessie_store::faces::spawn_s3(sc, backend)?;
        }
    }

    let listen = cfg.listen;
    let tls_dir = cfg.tls_dir();
    let peer_api = cfg.cas.as_ref().is_some_and(|c| c.peer_api);
    let mtls = cfg.mtls().then(|| {
        (
            cfg.peer_listen(),
            nessie_store::peer_tls::mesh_configs(&cfg),
        )
    });
    let mut router = app(AppState::new(backend, Arc::new(cfg), Arc::new(identity)));
    if peer_api {
        let node = faces
            .node
            .clone()
            .context("[cas] peer_api = true requires a [cas] node")?;
        let peer_app = nessie_store::cas_routes::peer_routes(node);
        if let Some((peer_listen, mesh)) = mtls {
            // Its own listener: rustls demands client certs per connection, not per path.
            let (server, _) = mesh?;
            let listener = std::net::TcpListener::bind(peer_listen)
                .with_context(|| format!("binding [tls] peer_listen {peer_listen}"))?;
            tracing::info!(%peer_listen, "cas peer API listening (mTLS)");
            tokio::spawn(async move {
                if let Err(e) = nessie_store::peer_tls::serve_peer(listener, peer_app, server).await
                {
                    tracing::error!(%e, "cas peer API exited");
                }
            });
        } else {
            tracing::warn!(
                "cas peer API mounted UNAUTHENTICATED (lab only; set [tls] mtls = true)"
            );
            router = router.merge(peer_app);
        }
    }

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
