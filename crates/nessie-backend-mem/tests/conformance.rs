//! The in-memory backend must pass every conformance suite — it is the sanity
//! check that both the trait stack and the conformance harness are sound.

use nessie_backend_mem::MemBackend;

#[test]
fn mem_passes_full_conformance() {
    nessie_backend_conformance::run_all(&MemBackend::new());
}
