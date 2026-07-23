//! The `Capabilities` service — the "no BuildBarn to stand up" honesty handshake.
//!
//! Advertises a **SHA-256-only, IDENTITY-only, cache-only** instance: the digest
//! function the whole face keys under, no compression, symlinks disallowed (the
//! native `Tree`/`ActionResult` carry none), and — the load-bearing declaration —
//! `execution_capabilities.exec_enabled = false`: this is a remote *cache*, not a
//! remote executor.

use crate::build::bazel::semver::SemVer;
use crate::config::ReapiConfig;
use crate::reapi;
use std::sync::Arc;
use tonic::{Request, Response, Status};

/// The `Capabilities` gRPC service.
pub struct CapabilitiesSvc {
    cfg: Arc<ReapiConfig>,
    /// Whether the backing store honors the ActionCache tier (gates AC-update advertising).
    ac_enabled: bool,
}

impl CapabilitiesSvc {
    /// Build the service for `cfg`; `ac_enabled` reflects whether the backend exposes
    /// the action-cache tier.
    #[must_use]
    pub fn new(cfg: Arc<ReapiConfig>, ac_enabled: bool) -> Self {
        Self { cfg, ac_enabled }
    }

    fn server_capabilities(&self) -> reapi::ServerCapabilities {
        let sha256 = reapi::digest_function::Value::Sha256 as i32;
        let identity = reapi::compressor::Value::Identity as i32;
        reapi::ServerCapabilities {
            cache_capabilities: Some(reapi::CacheCapabilities {
                digest_functions: vec![sha256],
                action_cache_update_capabilities: Some(reapi::ActionCacheUpdateCapabilities {
                    update_enabled: self.ac_enabled && self.cfg.ac_update_enabled,
                }),
                max_batch_total_size_bytes: self.cfg.max_batch_total_size_bytes,
                symlink_absolute_path_strategy:
                    reapi::symlink_absolute_path_strategy::Value::Disallowed as i32,
                supported_compressors: vec![identity],
                supported_batch_update_compressors: vec![identity],
                ..Default::default()
            }),
            // Cache only: no remote execution.
            execution_capabilities: Some(reapi::ExecutionCapabilities {
                digest_function: sha256,
                exec_enabled: false,
                ..Default::default()
            }),
            low_api_version: Some(SemVer {
                major: 2,
                minor: 0,
                ..Default::default()
            }),
            high_api_version: Some(SemVer {
                major: 2,
                minor: 3,
                ..Default::default()
            }),
            ..Default::default()
        }
    }
}

#[tonic::async_trait]
impl reapi::capabilities_server::Capabilities for CapabilitiesSvc {
    async fn get_capabilities(
        &self,
        _request: Request<reapi::GetCapabilitiesRequest>,
    ) -> Result<Response<reapi::ServerCapabilities>, Status> {
        Ok(Response::new(self.server_capabilities()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use reapi::capabilities_server::Capabilities;

    fn svc(ac_enabled: bool, ac_update: bool) -> CapabilitiesSvc {
        CapabilitiesSvc::new(
            Arc::new(ReapiConfig {
                ac_update_enabled: ac_update,
                ..Default::default()
            }),
            ac_enabled,
        )
    }

    #[tokio::test]
    async fn advertises_sha256_cache_only() {
        let resp = svc(true, true)
            .get_capabilities(Request::new(reapi::GetCapabilitiesRequest {
                instance_name: String::new(),
            }))
            .await
            .expect("get_capabilities")
            .into_inner();

        let cache = resp.cache_capabilities.expect("cache caps");
        assert_eq!(
            cache.digest_functions,
            vec![reapi::digest_function::Value::Sha256 as i32]
        );
        assert_eq!(
            cache.supported_compressors,
            vec![reapi::compressor::Value::Identity as i32]
        );
        assert_eq!(
            cache.symlink_absolute_path_strategy,
            reapi::symlink_absolute_path_strategy::Value::Disallowed as i32
        );
        assert_eq!(cache.max_batch_total_size_bytes, 4 * 1024 * 1024);
        assert!(
            cache
                .action_cache_update_capabilities
                .unwrap()
                .update_enabled
        );

        // The load-bearing "cache only, no remote execution" declaration.
        assert!(!resp.execution_capabilities.unwrap().exec_enabled);
        assert_eq!(resp.high_api_version.unwrap().major, 2);
    }

    #[tokio::test]
    async fn ac_update_is_gated_on_an_ac_capable_backend() {
        // update_enabled requires BOTH the config flag and an AC-capable backend.
        let no_ac = svc(false, true)
            .get_capabilities(Request::new(reapi::GetCapabilitiesRequest::default()))
            .await
            .unwrap()
            .into_inner();
        assert!(
            !no_ac
                .cache_capabilities
                .unwrap()
                .action_cache_update_capabilities
                .unwrap()
                .update_enabled
        );
    }
}
