//! A static-peer swarm for the content-addressed store.
//!
//! No mesh or broker runs in the lab, so peers are a configured list of base URLs
//! speaking `GET|HEAD|PUT /cas/v1/blobs/{algo}:{hex}`. Everything here sits behind
//! seams that already exist, so `CasStore` is untouched:
//!
//! - [`StaticRouter`]: a [`ContentRouter`](nessie_backend_core::ContentRouter) whose
//!   `providers` are the peers that *answer* `HEAD 200` (never the whole list — an
//!   over-claiming router would let a cache node evict a sole copy).
//! - [`SwarmCas`]: wraps a local [`ReclaimableCas`](nessie_backend_core::ReclaimableCas)
//!   and, on a local miss, fetches through from peers via `put_keyed`, which verifies.
//! - [`StaticDurabilityOracle`] / [`HttpReplicator`]: the cache-eviction gates.
//!
//! **Lab only:** the wire is unauthenticated plain HTTP; mTLS is the fast-follow.
//! Integrity does not depend on the wire: every byte is verified against its digest
//! before it is stored or served.

mod durable;
mod peers;
mod router;
mod swarm;

pub use durable::{HttpReplicator, StaticDurabilityOracle};
pub use peers::{Peer, PeerSet};
pub use router::StaticRouter;
pub use swarm::SwarmCas;

#[cfg(test)]
mod stub;

#[cfg(test)]
mod tests {
    use std::num::NonZeroUsize;
    use std::sync::Arc;

    use nessie_backend_core::{BackendError, CasBackend, ContentRouter, Digest, ReclaimableCas};
    use nessie_backend_mem::MemCas;
    use nessie_cas_store::{DurabilityOracle, Replicator};

    use crate::stub::{self, Blobs};
    use crate::{HttpReplicator, Peer, PeerSet, StaticDurabilityOracle, StaticRouter, SwarmCas};

    fn set(peers: &[(&str, bool)]) -> Arc<PeerSet> {
        Arc::new(
            PeerSet::new(
                peers
                    .iter()
                    .map(|(u, d)| Peer {
                        url: (*u).into(),
                        durable: *d,
                    })
                    .collect(),
            )
            .unwrap(),
        )
    }

    fn plant(blobs: &Blobs, bytes: &[u8]) -> Digest {
        let d = Digest::compute(bytes);
        blobs.lock().unwrap().insert(d.to_string(), bytes.to_vec());
        d
    }

    fn read(mut r: Box<dyn std::io::Read + Send>) -> Vec<u8> {
        let mut v = Vec::new();
        r.read_to_end(&mut v).unwrap();
        v
    }

    #[test]
    fn router_lists_only_peers_that_hold_the_blob() {
        let (a, a_blobs) = stub::spawn();
        let (b, _) = stub::spawn();
        let d = plant(&a_blobs, b"x");
        let router = StaticRouter::new(set(&[(&a, false), (&b, false)]));
        let got = router.providers(&d).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].as_str(), a);
        assert!(router.announce(&d).is_ok() && router.withdraw(&d).is_ok());
    }

    #[test]
    fn miss_is_filled_from_a_peer_and_kept_locally() {
        let (url, blobs) = stub::spawn();
        let d = plant(&blobs, b"restore me");
        let swarm = SwarmCas::new(MemCas::new(), set(&[(&url, true)]));
        assert!(!swarm.local().has(&d).unwrap());
        assert_eq!(read(swarm.get(&d).unwrap()), b"restore me");
        assert!(swarm.local().has(&d).unwrap(), "fetched blob is now local");
    }

    #[test]
    fn tampered_body_is_rejected_and_nothing_is_stored() {
        let (url, blobs) = stub::spawn();
        let d = Digest::compute(b"honest");
        blobs
            .lock()
            .unwrap()
            .insert(d.to_string(), b"forged".to_vec());
        let swarm = SwarmCas::new(MemCas::new(), set(&[(&url, true)]));
        assert!(matches!(swarm.get(&d), Err(BackendError::BlobNotFound(_))));
        assert!(
            !swarm.local().has(&d).unwrap(),
            "nothing stored from a bad body"
        );
        assert!(swarm.local().iter_local().unwrap().is_empty());
    }

    #[test]
    fn a_bad_peer_is_skipped_for_a_good_one() {
        let (bad, bad_blobs) = stub::spawn();
        let (good, good_blobs) = stub::spawn();
        let d = plant(&good_blobs, b"real");
        bad_blobs
            .lock()
            .unwrap()
            .insert(d.to_string(), b"fake".to_vec());
        let swarm = SwarmCas::new(MemCas::new(), set(&[(&bad, false), (&good, false)]));
        assert_eq!(read(swarm.get(&d).unwrap()), b"real");
    }

    #[test]
    fn unreachable_peers_are_a_miss_not_a_panic() {
        let swarm = SwarmCas::new(MemCas::new(), set(&[("http://127.0.0.1:1", true)]));
        let d = Digest::compute(b"nobody has this");
        assert!(matches!(swarm.get(&d), Err(BackendError::BlobNotFound(_))));
    }

    #[test]
    fn oracle_asks_only_durable_peers() {
        let (cache, cache_blobs) = stub::spawn();
        let (durable, durable_blobs) = stub::spawn();
        let only_cache = plant(&cache_blobs, b"c");
        let both = plant(&durable_blobs, b"d");
        let oracle = StaticDurabilityOracle::new(set(&[(&cache, false), (&durable, true)]));
        assert!(!oracle.durably_held(&only_cache).unwrap());
        assert!(oracle.durably_held(&both).unwrap());
    }

    #[test]
    fn a_peer_labelled_durable_but_running_lru_is_not_a_durable_holder() {
        let (lru, lru_blobs) = stub::spawn_mode("lru");
        let (wo, wo_blobs) = stub::spawn_mode("write_only");
        let d = plant(&lru_blobs, b"on an lru node");
        let oracle = StaticDurabilityOracle::new(set(&[(&lru, true)]));
        assert!(
            !oracle.durably_held(&d).unwrap(),
            "durable = true is only a hint"
        );
        // The same hint on a write_only peer is believed.
        let d2 = plant(&wo_blobs, b"on a write_only node");
        let oracle = StaticDurabilityOracle::new(set(&[(&wo, true), (&lru, true)]));
        assert!(oracle.durably_held(&d2).unwrap());
    }

    #[test]
    fn replicator_puts_to_durable_peers_until_target() {
        let (cache, cache_blobs) = stub::spawn();
        let (d1, d1_blobs) = stub::spawn();
        let (d2, d2_blobs) = stub::spawn();
        let local = Arc::new(MemCas::new());
        let digest = local.put(&mut &b"push me"[..]).unwrap();
        let rep = HttpReplicator::new(set(&[(&cache, false), (&d1, true), (&d2, true)]), local);
        rep.replicate(&digest, NonZeroUsize::new(1).unwrap())
            .unwrap();
        assert!(d1_blobs.lock().unwrap().contains_key(&digest.to_string()));
        assert!(
            !d2_blobs.lock().unwrap().contains_key(&digest.to_string()),
            "stops at target"
        );
        assert!(
            cache_blobs.lock().unwrap().is_empty(),
            "never pushes to a non-durable peer"
        );
    }

    #[test]
    fn size_and_has_fall_through_to_a_peer_without_fetching_the_body() {
        let (url, blobs) = stub::spawn();
        let d = plant(&blobs, b"held only by the peer");
        let swarm = SwarmCas::new(MemCas::new(), set(&[(&url, true)]));
        assert_eq!(swarm.size(&d).unwrap(), Some(21));
        assert!(
            !swarm.has(&d).unwrap(),
            "remote readability is size/get, not has"
        );
        assert!(
            swarm.local().iter_local().unwrap().is_empty(),
            "HEAD only, no body stored"
        );
        let absent = Digest::compute(b"nobody");
        assert_eq!(swarm.size(&absent).unwrap(), None);
        assert!(!swarm.has(&absent).unwrap());
    }

    fn pushable(peers: &[(&str, bool)]) -> (HttpReplicator, Digest) {
        let local = Arc::new(MemCas::new());
        let d = local.put(&mut &b"count me"[..]).unwrap();
        (HttpReplicator::new(set(peers), local), d)
    }

    #[test]
    fn push_counts_an_already_held_peer_once_and_does_not_reput_it() {
        let (d1, d1_blobs) = stub::spawn();
        let (d2, d2_blobs) = stub::spawn();
        let (rep, d) = pushable(&[(&d1, true), (&d2, true)]);
        d1_blobs
            .lock()
            .unwrap()
            .insert(d.to_string(), b"sentinel".to_vec());
        assert_eq!(rep.push(&d, NonZeroUsize::new(2).unwrap()).unwrap(), 2);
        assert_eq!(
            d1_blobs.lock().unwrap()[&d.to_string()],
            b"sentinel",
            "held peer untouched"
        );
        assert!(d2_blobs.lock().unwrap().contains_key(&d.to_string()));
    }

    #[test]
    fn push_returns_the_true_count_on_shortfall() {
        let (d1, _) = stub::spawn();
        let (rep, d) = pushable(&[(&d1, true), ("http://127.0.0.1:1", true)]);
        assert_eq!(rep.push(&d, NonZeroUsize::new(2).unwrap()).unwrap(), 1);
    }

    #[test]
    fn a_duplicate_url_is_one_holder_not_two() {
        let (d1, _) = stub::spawn();
        let (rep, d) = pushable(&[(&d1, true), (&format!("{d1}/"), true)]);
        assert_eq!(rep.push(&d, NonZeroUsize::new(2).unwrap()).unwrap(), 1);
    }

    #[test]
    fn push_counts_only_write_only_peers() {
        let (lru_has, lru_has_blobs) = stub::spawn_mode("lru");
        let (lru_not, lru_not_blobs) = stub::spawn_mode("lru");
        let (wo, wo_blobs) = stub::spawn_mode("write_only");
        let (rep, d) = pushable(&[(&lru_has, true), (&lru_not, true), (&wo, true)]);
        lru_has_blobs
            .lock()
            .unwrap()
            .insert(d.to_string(), b"x".to_vec());
        // A mislabelled lru peer does NOT count, whether or not it already had the blob.
        assert_eq!(rep.push(&d, NonZeroUsize::new(3).unwrap()).unwrap(), 1);
        assert!(wo_blobs.lock().unwrap().contains_key(&d.to_string()));
        assert!(
            lru_not_blobs.lock().unwrap().contains_key(&d.to_string()),
            "pushed, not counted"
        );
    }

    #[test]
    fn write_only_fetch_serves_verified_bytes_without_storing_them() {
        let (url, blobs) = stub::spawn();
        let d = plant(&blobs, b"serve, do not keep");
        let swarm = SwarmCas::new(MemCas::new(), set(&[(&url, true)])).with_cache_fills(false);
        assert_eq!(read(swarm.get(&d).unwrap()), b"serve, do not keep");
        assert!(!swarm.local().has(&d).unwrap());
        assert!(!swarm.has(&d).unwrap(), "has is local-only");
        // A forged body is still rejected, never served.
        let forged = Digest::compute(b"honest");
        blobs
            .lock()
            .unwrap()
            .insert(forged.to_string(), b"forged".to_vec());
        assert!(matches!(
            swarm.get(&forged),
            Err(BackendError::BlobNotFound(_))
        ));
    }
}
