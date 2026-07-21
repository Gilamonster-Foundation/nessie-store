//! Daemon configuration (TOML).
//!
//! A minimal config selects the backend and carries the static cluster identity
//! the ONTAP control plane reports. `nessie-store init` writes a default file;
//! `nessie-store serve --config <path>` loads it.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// The retention mode for an optional content-addressed store node.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum CasMode {
    /// Durable store of record: reachability GC only, never evicts a reachable blob.
    #[default]
    Durable,
    /// Bounded cache: replica-gated LRU eviction; cold blobs float to the swarm.
    Cache,
}

/// Optional `[cas]` node configuration. When present, the daemon runs a
/// content-addressed store and periodically performs retention maintenance
/// (durable GC or cache eviction). Fields are plain values so this crate stays
/// free of the retention-engine dependency; `cas_node` turns it into a policy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CasConfig {
    /// Durable store-of-record or bounded cache.
    pub mode: CasMode,
    /// Grace window (seconds) before an unreachable/cold blob may be reclaimed —
    /// the cross-restart backstop for the put→reference race.
    pub gc_grace_secs: u64,
    /// How often (seconds) to run a retention maintenance pass.
    pub maintenance_interval_secs: u64,
    /// Cache mode only: the local byte ceiling that triggers eviction.
    pub byte_budget: Option<u64>,
    /// Cache mode only: `R`, the number of other holders required before a
    /// reachable blob may be evicted (defaults to 2).
    pub replication_factor: Option<usize>,
}

impl Default for CasConfig {
    fn default() -> Self {
        Self {
            mode: CasMode::Durable,
            gc_grace_secs: 300,
            maintenance_interval_secs: 300,
            byte_budget: None,
            replication_factor: None,
        }
    }
}

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

    /// Start the embedded userspace NFSv3 server (the data plane; no host kernel
    /// NFS needed). When false, no NFS server is started.
    pub nfs_enabled: bool,
    /// Address the embedded NFS server binds (e.g. `0.0.0.0:2049`).
    pub nfs_listen: String,
    /// Directory tree the NFS server exports. Defaults to the ZFS pseudo-root
    /// where volume junctions mount, so every junctioned volume is served.
    pub nfs_export_root: PathBuf,
    /// Export path clients mount (`<host>:/<name>`); empty serves the bare root.
    pub nfs_export_name: String,

    /// Optional content-addressed store node. Absent (`None`) = the daemon runs no
    /// CAS node. Present = it runs one and schedules retention maintenance.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cas: Option<CasConfig>,
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
            nfs_enabled: false,
            nfs_listen: "0.0.0.0:2049".to_string(),
            nfs_export_root: PathBuf::from("/srv"),
            nfs_export_name: String::new(),
            cas: None,
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

    /// Directory holding the TLS cert/key (`data_dir/tls`).
    #[must_use]
    pub fn tls_dir(&self) -> PathBuf {
        self.data_dir.join("tls")
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

    #[test]
    fn cas_node_is_absent_by_default_and_parses_when_present() {
        // Absent by default, and omitted from serialized TOML.
        let def = Config::default();
        assert!(def.cas.is_none());
        assert!(!def.to_toml().contains("[cas]"));

        // A cache-mode node parses with its params.
        let cfg: Config = toml::from_str(
            "[cas]\nmode = \"cache\"\nbyte_budget = 1048576\nreplication_factor = 3\ngc_grace_secs = 60\nmaintenance_interval_secs = 30\n",
        )
        .expect("parse");
        let cas = cfg.cas.expect("cas present");
        assert_eq!(cas.mode, CasMode::Cache);
        assert_eq!(cas.byte_budget, Some(1_048_576));
        assert_eq!(cas.replication_factor, Some(3));
        assert_eq!(cas.gc_grace_secs, 60);

        // A durable node fills the sensible defaults.
        let durable: Config = toml::from_str("[cas]\nmode = \"durable\"\n").expect("parse");
        let cas = durable.cas.expect("cas present");
        assert_eq!(cas.mode, CasMode::Durable);
        assert_eq!(cas.gc_grace_secs, 300);
        assert_eq!(cas.byte_budget, None);
    }

    #[test]
    fn nfs_defaults_and_override() {
        // Off by default, with a sane :2049 listen + /srv export root.
        let def = Config::default();
        assert!(!def.nfs_enabled);
        assert_eq!(def.nfs_listen, "0.0.0.0:2049");
        assert_eq!(def.nfs_export_root, PathBuf::from("/srv"));

        // The embedded NFS plane is opt-in via the `[nfs]`-style flat keys.
        let cfg: Config = toml::from_str(
            "nfs_enabled = true\nnfs_listen = \"0.0.0.0:12049\"\nnfs_export_root = \"/srv/ontap\"\n",
        )
        .expect("parse");
        assert!(cfg.nfs_enabled);
        assert_eq!(cfg.nfs_listen, "0.0.0.0:12049");
        assert_eq!(cfg.nfs_export_root, PathBuf::from("/srv/ontap"));
    }
}
