//! Daemon configuration (TOML).
//!
//! A minimal config selects the backend and carries the static cluster identity
//! the ONTAP control plane reports. `nessie-store init` writes a default file;
//! `nessie-store serve --config <path>` loads it.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Which storage substrate the daemon dispatches to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum BackendKind {
    /// The in-memory reference backend (zero-privilege; non-persistent).
    #[default]
    Mem,
    /// The ZFS substrate (real datasets/snapshots/clones; needs ZFS + privilege).
    Zfs,
}

/// Top-level daemon configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Address the HTTP server binds.
    pub listen: SocketAddr,
    /// Directory for persistent daemon state (identity, registry, …).
    pub data_dir: PathBuf,
    /// Selected storage backend.
    pub backend: BackendKind,
    /// Basic-auth username (ONTAP clients send this).
    pub admin_username: String,
    /// Basic-auth password. Override via the `NESSIE_ADMIN_PASSWORD` env var.
    pub admin_password: String,
    /// Reported cluster name.
    pub cluster_name: String,
    /// Reported SVM name.
    pub svm_name: String,
    /// Reported single-node serial number (clients abort on a zero/empty serial).
    pub node_serial_number: String,
    /// Reported ONTAP API version.
    pub ontap_version: String,
    /// Synthetic data-LIF IP that NFS clients mount / Trident probes.
    pub data_lif: String,
    /// ZFS pool datasets are created under (when `backend = "zfs"`).
    pub zfs_pool: String,
    /// NFS export client specs for ZFS volumes (CIDRs/hosts); empty disables exports.
    pub zfs_nfs_clients: Vec<String>,
    /// `chown` target applied to a ZFS dataset root on junction set (e.g. `1000:1000`).
    pub zfs_dataset_owner: Option<String>,
    /// `chmod` mode applied to a ZFS dataset root on junction set (e.g. `0777`).
    pub zfs_dataset_mode: Option<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            listen: "0.0.0.0:8443".parse().expect("valid default listen addr"),
            data_dir: default_data_dir(),
            backend: BackendKind::Mem,
            admin_username: "admin".to_string(),
            admin_password: "admin".to_string(),
            cluster_name: "nessie-store".to_string(),
            svm_name: "svm0".to_string(),
            node_serial_number: "SIM-1-0000000001".to_string(),
            ontap_version: "9.14.1".to_string(),
            data_lif: "127.0.0.1".to_string(),
            zfs_pool: "ontap-sim".to_string(),
            zfs_nfs_clients: Vec::new(),
            zfs_dataset_owner: None,
            zfs_dataset_mode: None,
        }
    }
}

fn default_data_dir() -> PathBuf {
    PathBuf::from(".nessie-store")
}

impl Config {
    /// Load a config from a TOML file.
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| anyhow::anyhow!("reading config {}: {e}", path.display()))?;
        let mut cfg: Config = toml::from_str(&text)
            .map_err(|e| anyhow::anyhow!("parsing config {}: {e}", path.display()))?;
        cfg.apply_env();
        Ok(cfg)
    }

    /// Overlay environment overrides (secrets never live in the TOML file).
    pub fn apply_env(&mut self) {
        if let Ok(pw) = std::env::var("NESSIE_ADMIN_PASSWORD") {
            self.admin_password = pw;
        }
    }

    /// Path to the persisted cluster/SVM identity.
    #[must_use]
    pub fn identity_path(&self) -> PathBuf {
        self.data_dir.join("identity.json")
    }

    /// Serialize the default config to TOML (used by `init`).
    #[must_use]
    pub fn to_toml(&self) -> String {
        toml::to_string_pretty(self).expect("config serializes to TOML")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_roundtrips_through_toml() {
        let cfg = Config::default();
        let text = cfg.to_toml();
        let back: Config = toml::from_str(&text).expect("parse");
        assert_eq!(back.listen, cfg.listen);
        assert_eq!(back.backend, BackendKind::Mem);
        assert_eq!(back.ontap_version, "9.14.1");
    }

    #[test]
    fn partial_config_fills_defaults() {
        // Only one field set; everything else must default (serde(default)).
        let back: Config = toml::from_str("svm_name = \"svmX\"\n").expect("parse");
        assert_eq!(back.svm_name, "svmX");
        assert_eq!(back.cluster_name, "nessie-store");
        assert_eq!(back.admin_username, "admin");
    }

    #[test]
    fn selects_zfs_backend_with_settings() {
        let back: Config = toml::from_str(
            "backend = \"zfs\"\nzfs_pool = \"tank\"\nzfs_nfs_clients = [\"10.0.0.0/8\"]\n",
        )
        .expect("parse");
        assert_eq!(back.backend, BackendKind::Zfs);
        assert_eq!(back.zfs_pool, "tank");
        assert_eq!(back.zfs_nfs_clients, ["10.0.0.0/8"]);
    }
}
