//! Live ZFS conformance — runs the full conformance suite against a REAL pool.
//!
//! Gated behind the `live-zfs` feature AND the `NESSIE_TEST_POOL` env var so it
//! never runs in hermetic CI. Enable on a host with a disposable loopback pool:
//!
//! ```text
//! NESSIE_TEST_POOL=nessie-test cargo test -p nessie-backend-zfs --features live-zfs -- --nocapture
//! ```

#![cfg(feature = "live-zfs")]

use nessie_backend_zfs::{SystemRunner, ZfsBackend, ZfsConfig};

#[test]
fn live_conformance() {
    let Ok(pool) = std::env::var("NESSIE_TEST_POOL") else {
        eprintln!("SKIP: NESSIE_TEST_POOL unset (no real pool to test against)");
        return;
    };
    let cfg = ZfsConfig {
        pool,
        ..ZfsConfig::default()
    };
    let backend = ZfsBackend::new(SystemRunner, cfg);
    nessie_backend_conformance::run_all(&backend);
}
