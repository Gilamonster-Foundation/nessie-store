//! Static configuration for a REAPI face instance.

/// Tunables the face advertises and enforces. Kept small and plain; the daemon's
/// `[reapi]` block deserializes into it (a later slice).
#[derive(Debug, Clone)]
pub struct ReapiConfig {
    /// The instance name this face serves (`""` = the default instance).
    pub instance_name: String,
    /// The `max_batch_total_size_bytes` advertised — MUST match the tonic message
    /// size limit. Default 4 MiB.
    pub max_batch_total_size_bytes: i64,
    /// Whether `UpdateActionResult` is allowed (also gated on an AC-capable backend
    /// and a signer at wiring time).
    pub ac_update_enabled: bool,
}

impl Default for ReapiConfig {
    fn default() -> Self {
        Self {
            instance_name: String::new(),
            max_batch_total_size_bytes: 4 * 1024 * 1024,
            ac_update_enabled: true,
        }
    }
}
