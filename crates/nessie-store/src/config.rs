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
    /// Bounded cache: evicts coldest-first over `byte_budget`, blobs with another
    /// known holder before sole copies. Pinned roots and in-flight writes stay.
    #[default]
    Lru,
    /// Retain everything: never reclaims; a write past `byte_budget` (the capacity)
    /// is refused, never made room for.
    WriteOnly,
}

/// Where a `[cas]` node keeps its blobs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum CasBackendKind {
    /// In memory: lost on restart (tests, throwaway caches).
    #[default]
    Mem,
    /// On disk under `[cas] path`: survives restart.
    Disk,
}

/// One static peer in `[cas] peers`: another nessie-store node's base URL.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CasPeer {
    /// Base URL of the peer's `/cas/v1` surface, e.g. `http://host:8443`.
    pub url: String,
    /// A hint that this peer is a permanent home: it is probed by the durability
    /// oracle and is a replication target. The hint is **verified** — the peer counts
    /// as a durable holder only if it reports `x-nessie-mode: write_only` itself.
    #[serde(default)]
    pub durable: bool,
}

/// Optional `[cas]` node configuration. When present, the daemon runs a
/// content-addressed store and periodically performs retention maintenance
/// (LRU eviction, or none in write-only mode). Fields are plain values so this crate stays
/// free of the retention-engine dependency; `cas_node` turns it into a policy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CasConfig {
    /// `lru` (default) or `write_only`. The old `durable` mode is gone.
    pub mode: CasMode,
    /// Where blobs live. `disk` requires [`CasConfig::path`].
    pub backend: CasBackendKind,
    /// Root directory of the disk store (`backend = "disk"` only).
    pub path: Option<PathBuf>,
    /// Grace window (seconds) before a fresh blob may be evicted (`lru`) — the
    /// cross-restart backstop for the put→reference race.
    pub gc_grace_secs: u64,
    /// How often (seconds) to run a retention maintenance pass.
    pub maintenance_interval_secs: u64,
    /// Required. `lru`: the local byte ceiling that triggers eviction.
    /// `write_only`: the hard capacity past which writes are refused.
    pub byte_budget: Option<u64>,
    /// Static peer list for fetch-through, the durability oracle and replication.
    /// Empty = a standalone node.
    pub peers: Vec<CasPeer>,
    /// Serve `GET|HEAD|PUT /cas/v1/blobs/{digest}` so peers can reach this node.
    /// **Lab only: the routes are unauthenticated; mTLS is the fast-follow.**
    pub peer_api: bool,
    /// This node's own peer URL, as other nodes list it. A `peers` entry equal to
    /// it is ignored, so one shared peer list can be deployed to every node.
    pub advertise_url: Option<String>,
    /// `write_only` only: **R**, the total verified copies of a write, this node
    /// included. A write is acknowledged only once R - 1 `write_only` peers hold it.
    /// Default 2; 1 is a single-node archive with no redundancy. Setting it under
    /// `lru` is a startup error.
    pub replication_factor: Option<usize>,
    /// Bytes a maintenance pass may re-read to verify blobs (scrub), in both modes;
    /// default 1 GiB. A pass always verifies at least one blob and resumes where the
    /// last stopped, so a large store is covered over several passes. `0` turns the
    /// scrub off (no verification, no quarantine retry, no repair); a very large value
    /// is the explicit way to verify everything every pass.
    pub scrub_bytes_per_pass: u64,
}

/// `[cas] scrub_bytes_per_pass` default: 1 GiB.
pub const DEFAULT_SCRUB_BYTES_PER_PASS: u64 = 1 << 30;

impl CasConfig {
    /// The peers this node should talk to: trailing slashes normalised, this node's
    /// own [`advertise_url`](CasConfig::advertise_url) removed.
    ///
    /// # Errors
    ///
    /// A URL listed twice — the duplicate would be counted as a second holder.
    pub fn effective_peers(&self) -> anyhow::Result<Vec<CasPeer>> {
        let norm = |u: &str| u.trim_end_matches('/').to_owned();
        let me = self.advertise_url.as_deref().map(norm);
        let mut seen = std::collections::HashSet::new();
        let mut out = Vec::new();
        for p in &self.peers {
            let url = norm(&p.url);
            anyhow::ensure!(seen.insert(url.clone()), "[cas] peers lists {url} twice");
            if me.as_deref() != Some(url.as_str()) {
                out.push(CasPeer {
                    url,
                    durable: p.durable,
                });
            }
        }
        Ok(out)
    }
}

impl Default for CasConfig {
    fn default() -> Self {
        Self {
            mode: CasMode::Lru,
            backend: CasBackendKind::Mem,
            path: None,
            gc_grace_secs: 300,
            maintenance_interval_secs: 300,
            byte_budget: None,
            peers: Vec::new(),
            peer_api: false,
            advertise_url: None,
            replication_factor: None,
            scrub_bytes_per_pass: DEFAULT_SCRUB_BYTES_PER_PASS,
        }
    }
}

/// Optional REAPI (Bazel remote cache) gRPC face. Absent = the daemon serves no
/// REAPI. Present + `enabled` spins up a SHA-256-native cache beside the ONTAP
/// control plane — *a Bazel remote cache with no BuildBarn to stand up*.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ReapiServerConfig {
    /// Serve the REAPI face. `false` = present-but-off (the config can be staged).
    pub enabled: bool,
    /// Address the gRPC server binds (distinct from the HTTP `listen`).
    pub listen: SocketAddr,
    /// The instance name this face serves (`""` = the default instance).
    pub instance_name: String,
    /// Allow `UpdateActionResult` (also gated on an AC-capable backend + a signer).
    pub ac_update_enabled: bool,
}

impl Default for ReapiServerConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            listen: "0.0.0.0:8980"
                .parse()
                .expect("valid default reapi listen addr"),
            instance_name: String::new(),
            ac_update_enabled: true,
        }
    }
}

/// Optional S3 object-API face over the same content-addressed store the REAPI
/// face serves. Absent = the daemon serves no S3.
///
/// Serves digest-keyed object keys (`{prefix}/cas/{xx}/{sha256-hex}`) — the layout
/// cache clients like bazel-remote's S3 backend use. Because both faces are wired to
/// **one** backend, a blob written over REAPI gRPC is readable over S3 and vice
/// versa; that sharing is the point, not a coincidence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct S3ServerConfig {
    /// Serve the S3 face. `false` = present-but-off (the config can be staged).
    pub enabled: bool,
    /// Address the S3 HTTP server binds (distinct from the ONTAP `listen` and the
    /// REAPI `listen`). 9000 is the de-facto S3-compatible port.
    pub listen: SocketAddr,
    /// SigV4 access key id clients authenticate with.
    pub access_key: String,
    /// SigV4 secret key. Override via `NESSIE_S3_SECRET_KEY`; like
    /// `admin_password`, a real deployment keeps this out of the TOML.
    pub secret_key: String,
}

impl Default for S3ServerConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            listen: "0.0.0.0:9000"
                .parse()
                .expect("valid default s3 listen addr"),
            access_key: "nessie".to_string(),
            secret_key: "nessie".to_string(),
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

    /// Optional REAPI (Bazel remote cache) gRPC face. Absent = no REAPI server.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reapi: Option<ReapiServerConfig>,

    /// Optional S3 object-API face over the same CAS. Absent = no S3 server.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub s3: Option<S3ServerConfig>,
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
            reapi: None,
            s3: None,
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
        if let Some(cas) = &cfg.cas {
            cas.effective_peers()?;
        }
        Ok(cfg)
    }

    /// Overlay environment overrides (secrets never live in the TOML file).
    pub fn apply_env(&mut self) {
        if let Ok(pw) = std::env::var("NESSIE_ADMIN_PASSWORD") {
            self.admin_password = pw;
        }
        if let Some(s3) = self.s3.as_mut() {
            if let Ok(ak) = std::env::var("NESSIE_S3_ACCESS_KEY") {
                s3.access_key = ak;
            }
            if let Ok(sk) = std::env::var("NESSIE_S3_SECRET_KEY") {
                s3.secret_key = sk;
            }
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

        // A write_only node parses with its capacity.
        let cfg: Config = toml::from_str(
            "[cas]\nmode = \"write_only\"\nbyte_budget = 1048576\ngc_grace_secs = 60\nmaintenance_interval_secs = 30\n",
        )
        .expect("parse");
        let cas = cfg.cas.expect("cas present");
        assert_eq!(cas.mode, CasMode::WriteOnly);
        assert_eq!(cas.byte_budget, Some(1_048_576));
        assert_eq!(cas.gc_grace_secs, 60);

        // lru is the default and fills the sensible defaults.
        let lru: Config = toml::from_str("[cas]\n").expect("parse");
        let cas = lru.cas.expect("cas present");
        assert_eq!(cas.mode, CasMode::Lru);
        assert_eq!(cas.gc_grace_secs, 300);
        assert_eq!(cas.byte_budget, None);
    }

    #[test]
    fn cas_peers_parse_and_peer_api_defaults_off() {
        let bare: Config = toml::from_str("[cas]\n").expect("parse");
        let cas = bare.cas.expect("cas present");
        assert!(cas.peers.is_empty());
        assert!(!cas.peer_api, "peer routes are off unless asked for");

        let cfg: Config = toml::from_str(
            "[cas]\npeer_api = true\npeers = [{ url = \"http://a:1\", durable = true }, { url = \"http://b:2\" }]\n",
        )
        .expect("parse");
        let cas = cfg.cas.expect("cas present");
        assert!(cas.peer_api);
        assert_eq!(
            cas.peers,
            vec![
                CasPeer {
                    url: "http://a:1".into(),
                    durable: true
                },
                CasPeer {
                    url: "http://b:2".into(),
                    durable: false
                },
            ]
        );
    }

    #[test]
    fn duplicate_peers_are_rejected_and_self_is_dropped() {
        let peer = |u: &str| CasPeer {
            url: u.into(),
            durable: true,
        };
        let dup = CasConfig {
            peers: vec![peer("http://a:1"), peer("http://a:1/")],
            ..CasConfig::default()
        };
        let err = dup.effective_peers().unwrap_err().to_string();
        assert!(
            err.contains("http://a:1 twice"),
            "names the duplicate: {err}"
        );

        let shared = CasConfig {
            peers: vec![peer("http://me:1"), peer("http://b:2")],
            advertise_url: Some("http://me:1/".into()),
            ..CasConfig::default()
        };
        assert_eq!(shared.effective_peers().unwrap(), vec![peer("http://b:2")]);
    }

    #[test]
    fn durable_mode_is_a_parse_error_naming_the_two_modes() {
        let err = toml::from_str::<Config>("[cas]\nmode = \"durable\"\n")
            .expect_err("durable is retired")
            .to_string();
        assert!(
            err.contains("`lru`") && err.contains("`write_only`"),
            "{err}"
        );
    }

    #[test]
    fn cas_backend_parses_mem_default_and_disk_with_path() {
        let mem: Config = toml::from_str("[cas]\n").expect("parse");
        let cas = mem.cas.expect("cas present");
        assert_eq!(cas.backend, CasBackendKind::Mem);
        assert_eq!(cas.path, None);

        let disk: Config =
            toml::from_str("[cas]\nbackend = \"disk\"\npath = \"/var/lib/nessie/cas\"\n")
                .expect("parse");
        let cas = disk.cas.expect("cas present");
        assert_eq!(cas.backend, CasBackendKind::Disk);
        assert_eq!(cas.path, Some(PathBuf::from("/var/lib/nessie/cas")));
    }

    #[test]
    fn s3_face_is_absent_by_default_and_parses_when_present() {
        let def = Config::default();
        assert!(def.s3.is_none());
        assert!(!def.to_toml().contains("[s3]"));

        let cfg: Config = toml::from_str(
            "[s3]\nenabled = true\nlisten = \"0.0.0.0:9100\"\naccess_key = \"ci\"\nsecret_key = \"shh\"\n",
        )
        .expect("parse");
        let sc = cfg.s3.expect("s3 present");
        assert!(sc.enabled);
        assert_eq!(sc.listen.port(), 9100);
        assert_eq!(sc.access_key, "ci");
        assert_eq!(sc.secret_key, "shh");
    }

    #[test]
    fn s3_face_can_be_staged_present_but_off() {
        // A staged block parses with defaults and stays OFF, so a config can carry
        // the section before the operator flips it on.
        let staged: Config = toml::from_str("[s3]\naccess_key = \"ci\"\n").expect("parse");
        let sc = staged.s3.expect("s3 present");
        assert!(!sc.enabled, "present-but-off is the default");
        assert_eq!(sc.listen.port(), 9000);
        assert_eq!(sc.access_key, "ci");
    }

    #[test]
    fn reapi_face_is_absent_by_default_and_parses_when_present() {
        // Absent by default and omitted from serialized TOML.
        let def = Config::default();
        assert!(def.reapi.is_none());
        assert!(!def.to_toml().contains("[reapi]"));

        // A present block parses; unspecified keys take the ReapiServerConfig defaults.
        let cfg: Config = toml::from_str(
            "[reapi]\nenabled = true\nlisten = \"0.0.0.0:9092\"\ninstance_name = \"ci\"\nac_update_enabled = false\n",
        )
        .expect("parse");
        let rc = cfg.reapi.expect("reapi present");
        assert!(rc.enabled);
        assert_eq!(rc.listen, "0.0.0.0:9092".parse().unwrap());
        assert_eq!(rc.instance_name, "ci");
        assert!(!rc.ac_update_enabled);

        // Present-but-minimal: enabled defaults to false, listen to :8980.
        let staged: Config =
            toml::from_str("[reapi]\ninstance_name = \"staged\"\n").expect("parse");
        let rc = staged.reapi.expect("reapi present");
        assert!(!rc.enabled);
        assert_eq!(rc.listen, "0.0.0.0:8980".parse().unwrap());
        assert!(rc.ac_update_enabled);
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
