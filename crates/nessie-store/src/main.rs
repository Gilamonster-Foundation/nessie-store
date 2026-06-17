//! The `nessie-store` daemon CLI.
//!
//! `nessie-store init` writes a default config; `nessie-store serve` runs the
//! ONTAP REST daemon over the configured backend.

#![forbid(unsafe_code)]

use std::path::PathBuf;
use std::sync::Arc;

use clap::{Parser, Subcommand};
use tracing_subscriber::EnvFilter;

use nessie_backend_core::VolumeBackend;
use nessie_backend_mem::MemBackend;
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
        Cmd::Serve { config } => serve(&config).await,
    }
}

async fn serve(config_path: &std::path::Path) -> anyhow::Result<()> {
    let cfg = Config::load(config_path)?;
    let identity = Identity::load_or_create(&cfg.identity_path())?;

    let backend: Arc<dyn VolumeBackend> = match cfg.backend {
        BackendKind::Mem => Arc::new(MemBackend::new()),
    };

    let listen = cfg.listen;
    let state = AppState::new(backend, Arc::new(cfg), Arc::new(identity));

    let listener = tokio::net::TcpListener::bind(listen).await?;
    tracing::info!(%listen, "nessie-store listening");
    axum::serve(listener, app(state)).await?;
    Ok(())
}
