//! The in-memory CAS backend must pass every CAS conformance suite — the sanity
//! check that both the `CasBackend` contract and the conformance harness are sound.

use nessie_backend_mem::MemCas;

#[test]
fn mem_cas_passes_full_conformance() {
    nessie_cas_conformance::run_all(&MemCas::new());
}
