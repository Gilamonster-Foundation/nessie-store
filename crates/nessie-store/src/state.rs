//! Shared application state handed to every route.

use std::sync::Arc;

use nessie_backend_core::VolumeBackend;
use nessie_ontap_protocol::SvmRef;

use crate::config::Config;
use crate::identity::Identity;
use crate::snapmirror::SnapMirrorStore;

/// State shared across handlers. Cheap to clone (everything is behind `Arc`).
#[derive(Clone)]
pub struct AppState {
    /// The storage backend the control plane dispatches to.
    pub backend: Arc<dyn VolumeBackend>,
    /// Daemon configuration.
    pub config: Arc<Config>,
    /// Stable cluster identity.
    pub identity: Arc<Identity>,
    /// SnapMirror relationships + cluster peers (daemon state).
    pub snapmirror: Arc<SnapMirrorStore>,
}

impl AppState {
    /// Build the application state from its parts. The SnapMirror store is
    /// daemon-internal and constructed here (not injected).
    #[must_use]
    pub fn new(
        backend: Arc<dyn VolumeBackend>,
        config: Arc<Config>,
        identity: Arc<Identity>,
    ) -> Self {
        Self {
            backend,
            config,
            identity,
            snapmirror: Arc::new(SnapMirrorStore::default()),
        }
    }

    /// The SVM reference embedded in volume records.
    #[must_use]
    pub fn svm_ref(&self) -> SvmRef {
        SvmRef {
            name: self.config.svm_name.clone(),
            uuid: self.identity.svm_uuid.clone(),
        }
    }
}
