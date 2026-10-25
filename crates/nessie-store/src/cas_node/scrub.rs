//! The scrub-and-repair half of a maintenance pass.
//!
//! Each pass verifies a bounded slice of the local inventory. A corrupt blob is taken
//! out — quarantined and restored from a peer in `write_only`, which never deletes;
//! reclaimed in `lru`, where a later read refetches it — and every healthy or
//! restored blob in a mirrored `write_only` node is topped up to R copies with the
//! same push a write uses.

use std::io::Cursor;

use nessie_backend_core::{BackendError, CasBackend, Digest, LocalBlob};
use nessie_cas_store::StorageMode;

use super::CasNode;

/// What one scrub-and-repair pass found and did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScrubReport {
    /// Blobs read and verified.
    pub scanned: usize,
    /// Blobs that no longer hash to their digest.
    pub corrupt: usize,
    /// `write_only`: blobs refetched from a peer and put back, whether found corrupt
    /// this pass or left pending by an earlier one.
    pub restored: usize,
    /// `write_only`: corrupt blobs found this pass that could not be restored yet
    /// (no peer had them, or the capacity gate refused).
    pub unrestored: usize,
    /// `write_only`: quarantined files retained at the end of the pass. They stay
    /// charged against capacity for good — corruption costs capacity.
    pub quarantined_total: usize,
    /// `write_only`: digests in quarantine with no live copy at the end of the
    /// pass — damage still waiting for a peer or for room.
    pub pending_restore: usize,
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
        if self.scrub_budget == 0 {
            return Ok(ScrubReport::default());
        }
        let mut report = ScrubReport::default();
        self.retry_pending(&mut report)?;
        let mut blobs = self.store.local_blobs()?;
        blobs.sort_by_cached_key(|b| b.digest.to_string());
        let mut cursor = self.scrub_cursor.lock().expect("scrub cursor poisoned");
        let start = cursor.as_ref().map_or(0, |after| {
            blobs.partition_point(|b| b.digest.to_string() <= *after)
        });
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
        self.count_quarantine(&mut report)?;
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
        let result = match &self.store.policy().mode {
            StorageMode::Lru(_) => self
                .store
                .reclaim_corrupt(digest)
                .map(|_| report.reclaimed += 1)
                .map(|()| false),
            StorageMode::WriteOnly(_) => self
                .store
                .quarantine(digest)
                .and_then(|_| self.restore(digest))
                .map(|()| true),
        };
        match result {
            Ok(restored) => {
                report.restored += usize::from(restored);
                restored
            }
            Err(e) => {
                tracing::error!(%digest, %e, "scrub: corrupt blob left quarantined");
                report.unrestored += 1;
                false
            }
        }
    }

    /// `write_only`: refetch (digest-verified) and put back through the capacity
    /// gate, which reserves new room — the quarantined copy stays charged. Any
    /// failure leaves the blob quarantined.
    fn restore(&self, digest: &Digest) -> Result<(), BackendError> {
        let mut bytes = Vec::new();
        std::io::Read::read_to_end(&mut self.store.get(digest)?, &mut bytes)
            .map_err(|e| BackendError::Internal(format!("refetch {digest}: {e}")))?;
        self.put_keyed_local(digest, &mut Cursor::new(bytes))
    }

    /// `write_only`: every quarantined digest with no live copy is a restore that
    /// failed earlier (peer down, no room); try again now.
    fn retry_pending(&self, report: &mut ScrubReport) -> anyhow::Result<()> {
        if !matches!(self.store.policy().mode, StorageMode::WriteOnly(_)) {
            return Ok(());
        }
        for digest in self.pending_digests()? {
            match self.restore(&digest) {
                Ok(()) => report.restored += 1,
                Err(e) => tracing::warn!(%digest, %e, "scrub: pending restore still failing"),
            }
        }
        Ok(())
    }

    /// Digests in quarantine that have no live copy, each once.
    fn pending_digests(&self) -> anyhow::Result<Vec<Digest>> {
        let mut seen = std::collections::HashSet::new();
        let mut out = Vec::new();
        for q in self.store.quarantined()? {
            if seen.insert(q.digest.clone()) && !self.store.has(&q.digest)? {
                out.push(q.digest);
            }
        }
        Ok(out)
    }

    fn count_quarantine(&self, report: &mut ScrubReport) -> anyhow::Result<()> {
        if matches!(self.store.policy().mode, StorageMode::WriteOnly(_)) {
            report.quarantined_total = self.store.quarantined()?.len();
            report.pending_restore = self.pending_digests()?.len();
        }
        Ok(())
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
