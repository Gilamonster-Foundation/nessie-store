//! The scrub-and-repair half of a maintenance pass.
//!
//! Each pass verifies a bounded slice of the local inventory. A corrupt blob is taken
//! out — quarantined and restored from a peer in `write_only`, which never deletes;
//! reclaimed in `lru`, where a later read refetches it — and every healthy or
//! restored blob in a mirrored `write_only` node is topped up to R copies with the
//! same push a write uses.

use std::io::Cursor;

use nessie_backend_core::{BackendError, CasBackend, LocalBlob};
use nessie_cas_store::StorageMode;

use super::CasNode;

/// What one scrub-and-repair pass found and did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScrubReport {
    /// Blobs read and verified.
    pub scanned: usize,
    /// Blobs that no longer hash to their digest.
    pub corrupt: usize,
    /// `write_only`: corrupt blobs refetched from a peer and put back.
    pub restored: usize,
    /// `write_only`: corrupt blobs left in quarantine (no peer had them, or the
    /// capacity gate refused the restore).
    pub unrestored: usize,
    /// `lru`: corrupt blobs dropped from the cache.
    pub reclaimed: usize,
    /// Mirrored `write_only`: blobs still short of R copies after the push.
    pub under_replicated: usize,
    /// Mirrored `write_only`: blobs this pass brought up to R by sending copies.
    pub replicated: usize,
}

impl CasNode {
    /// Verify the next slice of the local inventory (at most `scrub_bytes_per_pass`
    /// bytes, but always at least one blob so a pass makes progress), then repair.
    ///
    /// The cursor is the last digest scanned and lives in memory: a restart starts
    /// over from the beginning of the inventory. It wraps after the last blob.
    pub(super) fn scrub_pass(&self) -> anyhow::Result<ScrubReport> {
        let mut blobs = self.store.local_blobs()?;
        blobs.sort_by_cached_key(|b| b.digest.to_string());
        let mut cursor = self.scrub_cursor.lock().expect("scrub cursor poisoned");
        let start = cursor.as_ref().map_or(0, |after| {
            blobs.partition_point(|b| b.digest.to_string() <= *after)
        });
        let mut report = ScrubReport::default();
        let mut spent = 0u64;
        let mut next = start;
        while let Some(blob) = blobs.get(next) {
            if next > start && spent >= self.scrub_budget {
                break;
            }
            spent = spent.saturating_add(blob.size_bytes);
            self.scrub_one(blob, &mut report);
            next += 1;
        }
        // Past the last blob: wrap. Otherwise resume after the last one visited.
        *cursor = (next > start && next < blobs.len()).then(|| blobs[next - 1].digest.to_string());
        Ok(report)
    }

    fn scrub_one(&self, blob: &LocalBlob, report: &mut ScrubReport) {
        let digest = &blob.digest;
        match self.store.verify_local(digest) {
            Ok(true) => report.scanned += 1,
            Ok(false) => {
                report.scanned += 1;
                report.corrupt += 1;
                tracing::warn!(%digest, "scrub: blob fails verification");
                if !self.take_out_corrupt(blob, report) {
                    return;
                }
            }
            // Reclaimed under us, or unreadable right now: not a verdict on the bytes.
            Err(BackendError::BlobNotFound(_)) => return,
            Err(e) => {
                tracing::warn!(%digest, %e, "scrub: cannot read blob, skipped");
                return;
            }
        }
        self.repair(blob, report);
    }

    /// Remove the corrupt copy; `true` if the blob is healthy and held again.
    fn take_out_corrupt(&self, blob: &LocalBlob, report: &mut ScrubReport) -> bool {
        let digest = &blob.digest;
        let before = report.restored;
        let result = match &self.store.policy().mode {
            StorageMode::Lru(_) => self
                .store
                .reclaim_corrupt(digest, blob.size_bytes)
                .map(|_| report.reclaimed += 1),
            StorageMode::WriteOnly(_) => self.restore(blob).map(|()| report.restored += 1),
        };
        match result {
            Ok(()) => report.restored > before,
            Err(e) => {
                tracing::error!(%digest, %e, "scrub: corrupt blob left quarantined");
                report.unrestored += 1;
                false
            }
        }
    }

    /// `write_only`: quarantine, refetch (digest-verified) and put back through the
    /// capacity gate. Any failure leaves the blob quarantined.
    fn restore(&self, blob: &LocalBlob) -> Result<(), BackendError> {
        let digest = &blob.digest;
        self.store.quarantine(digest, blob.size_bytes)?;
        let mut bytes = Vec::new();
        std::io::Read::read_to_end(&mut self.store.get(digest)?, &mut bytes)
            .map_err(|e| BackendError::Internal(format!("refetch {digest}: {e}")))?;
        self.put_keyed_local(digest, &mut Cursor::new(bytes))
    }

    /// Mirrored `write_only`: push the blob until R - 1 peers hold it.
    fn repair(&self, blob: &LocalBlob, report: &mut ScrubReport) {
        let Some(m) = &self.mirror else {
            return;
        };
        match m.replicator.push_counting(&blob.digest, m.peer_copies) {
            Ok(p) if p.held >= m.peer_copies.get() => report.replicated += usize::from(p.sent > 0),
            Ok(_) => report.under_replicated += 1,
            Err(e) => {
                tracing::warn!(digest = %blob.digest, %e, "repair: cannot push");
                report.under_replicated += 1;
            }
        }
    }
}
