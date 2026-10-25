//! The on-disk CAS must pass every CAS conformance suite.

use nessie_cas_disk::DiskCas;

#[test]
fn disk_cas_passes_full_conformance() {
    let dir = std::env::temp_dir().join(format!("nessie-cas-disk-conf-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    nessie_cas_conformance::run_all(&DiskCas::open(&dir).expect("open"));
    let _ = std::fs::remove_dir_all(&dir);
}
