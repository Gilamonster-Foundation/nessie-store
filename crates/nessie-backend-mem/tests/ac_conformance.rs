//! The in-memory action cache must pass every AC conformance suite — the sanity
//! check that both the `ActionCacheBackend` contract and the harness are sound.

use nessie_backend_core::{ActionCacheBackend, SignatureVerifier};
use nessie_backend_mem::{MemActionCache, MemCas};
use std::num::NonZeroUsize;
use std::sync::Arc;

#[test]
fn mem_action_cache_passes_full_conformance() {
    nessie_ac_conformance::run_all(&|verifier: Arc<dyn SignatureVerifier>, k: NonZeroUsize| {
        Box::new(MemActionCache::new(MemCas::new(), verifier, k)) as Box<dyn ActionCacheBackend>
    });
}
