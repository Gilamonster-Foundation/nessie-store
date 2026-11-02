//! The in-memory CAS backend must pass every CAS conformance suite — the sanity
//! check that both the `CasBackend` contract and the conformance harness are sound.

use nessie_backend_core::ReclaimableCas;
use nessie_backend_mem::MemCas;

#[test]
fn mem_cas_passes_full_conformance() {
    nessie_cas_conformance::run_all(&MemCas::new());
}

/// A boxed trait object is itself a `CasBackend`/`ReclaimableCas`: every method,
/// defaulted ones included, must forward (issue #123).
#[test]
fn boxed_reclaimable_passes_full_conformance() {
    let boxed = Box::new(MemCas::new()) as Box<dyn ReclaimableCas>;
    nessie_cas_conformance::run_all(&boxed);
    assert!(nessie_backend_core::CasBackend::as_reclaimable(&boxed).is_some());
}
