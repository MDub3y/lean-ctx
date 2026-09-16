use md5::{Digest, Md5};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::SystemTime;

/// The single terminal reuse classification for one eligible cacheable read.
///
/// A request can pass through several cache layers, but it must be counted in
/// exactly one bucket.  In particular, a session-cache rendering that is later
/// collapsed by kernel dedup is an [`UnchangedStub`](Self::UnchangedStub), not
/// two independent hits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReuseOutcome {
    Cold,
    DiskContentHit,
    RenderCacheHit,
    UnchangedStub,
    CrossFileRef,
    FreshBypass,
    Stale,
    VariantEvicted,
    PolicyBypass,
}

impl ReuseOutcome {
    const fn is_render_reuse(self) -> bool {
        matches!(
            self,
            Self::RenderCacheHit | Self::UnchangedStub | Self::CrossFileRef
        )
    }

    const fn is_ctx_read_eligible(self) -> bool {
        !matches!(self, Self::FreshBypass | Self::PolicyBypass)
    }
}

/// Snapshot for the three non-overlapping cache-reuse measurements.
///
/// `eligible_ctx_reads` intentionally excludes explicit fresh reads and
/// policy-disabled cache reads: neither was allowed to reuse a rendering.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReuseSnapshot {
    pub cold: u64,
    pub disk_content_hits: u64,
    pub render_cache_hits: u64,
    pub unchanged_stubs: u64,
    pub cross_file_refs: u64,
    pub fresh_bypasses: u64,
    pub stale: u64,
    pub variant_evicted: u64,
    pub policy_bypasses: u64,
    pub eligible_ctx_reads: u64,
    pub reused_renderings: u64,
    pub eligible_search_reads: u64,
}

impl ReuseSnapshot {
    /// Percentage of eligible `ctx_read`s served by an already-rendered result.
    pub fn read_reuse_rate(&self) -> f64 {
        rate(self.reused_renderings, self.eligible_ctx_reads)
    }

    /// Percentage of eligible search file reads served by `content_cache`.
    pub fn disk_reuse_rate(&self) -> f64 {
        rate(self.disk_content_hits, self.eligible_search_reads)
    }
}

struct ReuseCounters {
    cold: AtomicU64,
    disk_content_hits: AtomicU64,
    render_cache_hits: AtomicU64,
    unchanged_stubs: AtomicU64,
    cross_file_refs: AtomicU64,
    fresh_bypasses: AtomicU64,
    stale: AtomicU64,
    variant_evicted: AtomicU64,
    policy_bypasses: AtomicU64,
    eligible_ctx_reads: AtomicU64,
    reused_renderings: AtomicU64,
    eligible_search_reads: AtomicU64,
}

impl ReuseCounters {
    const fn new() -> Self {
        Self {
            cold: AtomicU64::new(0),
            disk_content_hits: AtomicU64::new(0),
            render_cache_hits: AtomicU64::new(0),
            unchanged_stubs: AtomicU64::new(0),
            cross_file_refs: AtomicU64::new(0),
            fresh_bypasses: AtomicU64::new(0),
            stale: AtomicU64::new(0),
            variant_evicted: AtomicU64::new(0),
            policy_bypasses: AtomicU64::new(0),
            eligible_ctx_reads: AtomicU64::new(0),
            reused_renderings: AtomicU64::new(0),
            eligible_search_reads: AtomicU64::new(0),
        }
    }
}

static REUSE_COUNTERS: ReuseCounters = ReuseCounters::new();

fn rate(numerator: u64, denominator: u64) -> f64 {
    if denominator == 0 {
        0.0
    } else {
        numerator as f64 * 100.0 / denominator as f64
    }
}

fn bump_outcome(counters: &ReuseCounters, outcome: ReuseOutcome) {
    let counter = match outcome {
        ReuseOutcome::Cold => &counters.cold,
        ReuseOutcome::DiskContentHit => &counters.disk_content_hits,
        ReuseOutcome::RenderCacheHit => &counters.render_cache_hits,
        ReuseOutcome::UnchangedStub => &counters.unchanged_stubs,
        ReuseOutcome::CrossFileRef => &counters.cross_file_refs,
        ReuseOutcome::FreshBypass => &counters.fresh_bypasses,
        ReuseOutcome::Stale => &counters.stale,
        ReuseOutcome::VariantEvicted => &counters.variant_evicted,
        ReuseOutcome::PolicyBypass => &counters.policy_bypasses,
    };
    counter.fetch_add(1, Ordering::Relaxed);
}

fn record_ctx_read_outcome_into(counters: &ReuseCounters, outcome: ReuseOutcome) {
    bump_outcome(counters, outcome);
    if outcome.is_ctx_read_eligible() {
        counters.eligible_ctx_reads.fetch_add(1, Ordering::Relaxed);
        if outcome.is_render_reuse() {
            counters.reused_renderings.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// Record exactly one terminal outcome for a completed `ctx_read`.
pub fn record_ctx_read_outcome(outcome: ReuseOutcome) {
    record_ctx_read_outcome_into(&REUSE_COUNTERS, outcome);
}

fn record_search_content_read_into(counters: &ReuseCounters, hit: bool) {
    counters
        .eligible_search_reads
        .fetch_add(1, Ordering::Relaxed);
    bump_outcome(
        counters,
        if hit {
            ReuseOutcome::DiskContentHit
        } else {
            ReuseOutcome::Cold
        },
    );
}

/// Record exactly one content-cache outcome for one eligible search file read.
pub fn record_search_content_read(hit: bool) {
    record_search_content_read_into(&REUSE_COUNTERS, hit);
}

/// Return a lock-free point-in-time view of reuse counters.
#[must_use]
pub fn reuse_snapshot() -> ReuseSnapshot {
    snapshot_from(&REUSE_COUNTERS)
}

fn snapshot_from(counters: &ReuseCounters) -> ReuseSnapshot {
    ReuseSnapshot {
        cold: counters.cold.load(Ordering::Relaxed),
        disk_content_hits: counters.disk_content_hits.load(Ordering::Relaxed),
        render_cache_hits: counters.render_cache_hits.load(Ordering::Relaxed),
        unchanged_stubs: counters.unchanged_stubs.load(Ordering::Relaxed),
        cross_file_refs: counters.cross_file_refs.load(Ordering::Relaxed),
        fresh_bypasses: counters.fresh_bypasses.load(Ordering::Relaxed),
        stale: counters.stale.load(Ordering::Relaxed),
        variant_evicted: counters.variant_evicted.load(Ordering::Relaxed),
        policy_bypasses: counters.policy_bypasses.load(Ordering::Relaxed),
        eligible_ctx_reads: counters.eligible_ctx_reads.load(Ordering::Relaxed),
        reused_renderings: counters.reused_renderings.load(Ordering::Relaxed),
        eligible_search_reads: counters.eligible_search_reads.load(Ordering::Relaxed),
    }
}

pub fn file_mtime(path: &str) -> Option<SystemTime> {
    // #1780: a transient failure here is indistinguishable from "the file is
    // gone" once it collapses into `None`, and `is_cache_entry_stale` reads
    // `Some(cached)` against `None` as a change. Retry first; `NotFound` — the
    // common and meaningful case — is not retried and still returns `None`
    // immediately.
    retry_while_transient(|| std::fs::metadata(path).and_then(|m| m.modified())).ok()
}

pub fn is_cache_entry_stale(path: &str, cached_mtime: Option<SystemTime>) -> bool {
    let current = file_mtime(path);
    match (cached_mtime, current) {
        // Both unavailable (e.g. WSL DrvFS): can't tell → assume fresh (conservative).
        (None, None) => false,
        // One side missing: metadata changed or appeared/disappeared → stale.
        (Some(_), None) | (None, Some(_)) => true,
        // `!=`, not `>`: a *backward* mtime (git checkout, touch -t, snapshot
        // restore) is just as much a content change as a forward one.
        (Some(cached), Some(current)) => current != cached,
    }
}

/// Files larger than this are not content-hashed for stub verification; the
/// mtime check alone decides. Keeps the stub fast-path O(small-file-read).
const VERIFY_HASH_CAP_BYTES: u64 = 8 * 1024 * 1024;

fn cache_verify_enabled() -> bool {
    std::env::var("LEAN_CTX_CACHE_VERIFY").map_or(true, |v| v != "0")
}

/// #1780: tells "the file changed" apart from "we could not look at it".
///
/// `NotFound` is a real change — the file is gone — and is reported at once.
/// Every other failure is treated as possibly transient. Enumerating kinds
/// would be brittle: Windows reports a briefly-held file as
/// `ERROR_SHARING_VIOLATION` or `ERROR_ACCESS_DENIED` depending on the holder,
/// and their mapping onto `ErrorKind` is not something this crate can pin down
/// on a POSIX host. Retrying costs a few milliseconds on a file that really is
/// unreadable, and the verdict for that case is unchanged: stale.
fn io_error_may_be_transient(error: &std::io::Error) -> bool {
    error.kind() != std::io::ErrorKind::NotFound
}

/// Runs `op`, retrying briefly while it fails in a way that may be transient.
///
/// The fail-closed guarantee is untouched: when every attempt fails the caller
/// still treats the entry as stale, so a wrong `[unchanged]` stub remains
/// impossible. What this removes is the *non-determinism* — on Windows a file
/// can still be locked moments after a write, which made the stub path
/// re-deliver content that had not changed (#1780).
fn retry_while_transient<T>(mut op: impl FnMut() -> std::io::Result<T>) -> std::io::Result<T> {
    const ATTEMPTS: usize = 3;
    const BACKOFF: std::time::Duration = std::time::Duration::from_millis(15);

    let mut result = op();
    for _ in 1..ATTEMPTS {
        match result {
            Err(ref error) if io_error_may_be_transient(error) => {
                std::thread::sleep(BACKOFF);
                result = op();
            }
            // Success, or a failure that says something definite (`NotFound`):
            // either way there is nothing a retry would tell us.
            _ => return result,
        }
    }
    result
}

/// Staleness with content verification: like [`is_cache_entry_stale`], but when
/// the mtime claims "unchanged", additionally compares the md5 of the on-disk
/// content against the cached hash.
///
/// mtime alone cannot be trusted for *correctness*: same-second writes are
/// invisible on coarse-granularity filesystems (HFS+ 1s, FAT 2s) and mtimes can
/// be restored by tools. Serving an `[unchanged]` stub for changed content
/// would silently mislead the agent — the worst failure mode a context layer
/// can have. The extra disk read costs microseconds for typical source files;
/// the stub's token savings are unaffected. Opt out: `LEAN_CTX_CACHE_VERIFY=0`.
///
/// Note: entries whose stored content differs from disk by design (e.g. secret
/// redaction) hash differently and therefore never serve stubs — conservative
/// and correct.
///
/// #1780: both filesystem calls below retry briefly before giving up, so a
/// transient lock is not mistaken for a content change. The mtime probe that
/// runs first is covered too — [`file_mtime`] retries in the same way — so all
/// three filesystem touches on this path share the behaviour. An unreadable
/// file is still declared stale — that verdict is never *wrong*, only wasteful
/// — but it no longer happens for a file that merely had a writer's handle open
/// a moment ago.
pub fn is_cache_entry_stale_verified(
    path: &str,
    cached_mtime: Option<SystemTime>,
    cached_hash: &str,
) -> bool {
    let mtime_stale = is_cache_entry_stale(path, cached_mtime);

    if !mtime_stale && (cached_hash.is_empty() || !cache_verify_enabled()) {
        return false;
    }

    if cached_hash.is_empty() {
        return mtime_stale;
    }

    let Ok(meta) = retry_while_transient(|| std::fs::metadata(path)) else {
        return true;
    };
    if meta.len() > VERIFY_HASH_CAP_BYTES {
        return mtime_stale;
    }
    match retry_while_transient(|| std::fs::read(path)) {
        Ok(bytes) => compute_md5(&String::from_utf8_lossy(&bytes)) != cached_hash,
        Err(_) => true,
    }
}
pub(super) fn compute_md5(content: &str) -> String {
    let mut hasher = Md5::new();
    hasher.update(content.as_bytes());
    crate::core::agent_identity::hex_encode(&hasher.finalize())
}

#[cfg(test)]
mod tests {
    use super::{
        ReuseCounters, ReuseOutcome, ReuseSnapshot, record_ctx_read_outcome_into,
        record_search_content_read_into, snapshot_from,
    };

    #[test]
    fn every_read_records_one_exclusive_terminal_outcome() {
        let counters = ReuseCounters::new();
        record_ctx_read_outcome_into(&counters, ReuseOutcome::RenderCacheHit);
        record_ctx_read_outcome_into(&counters, ReuseOutcome::UnchangedStub);
        record_ctx_read_outcome_into(&counters, ReuseOutcome::FreshBypass);
        record_ctx_read_outcome_into(&counters, ReuseOutcome::PolicyBypass);
        record_search_content_read_into(&counters, true);
        record_search_content_read_into(&counters, false);

        let snapshot = snapshot_from(&counters);
        assert_eq!(snapshot.render_cache_hits, 1);
        assert_eq!(snapshot.unchanged_stubs, 1);
        assert_eq!(snapshot.fresh_bypasses, 1);
        assert_eq!(snapshot.policy_bypasses, 1);
        assert_eq!(snapshot.disk_content_hits, 1);
        assert_eq!(snapshot.cold, 1);
        assert_eq!(snapshot.eligible_ctx_reads, 2);
        assert_eq!(snapshot.reused_renderings, 2);
        assert_eq!(snapshot.eligible_search_reads, 2);
    }

    #[test]
    fn reuse_rates_keep_read_and_disk_denominators_separate() {
        let metrics = ReuseSnapshot {
            reused_renderings: 3,
            eligible_ctx_reads: 4,
            disk_content_hits: 2,
            eligible_search_reads: 5,
            ..ReuseSnapshot::default()
        };

        assert_eq!(metrics.read_reuse_rate(), 75.0);
        assert_eq!(metrics.disk_reuse_rate(), 40.0);
    }

    #[test]
    fn only_render_reuse_outcomes_count_for_ctx_read_reuse() {
        assert!(ReuseOutcome::RenderCacheHit.is_render_reuse());
        assert!(ReuseOutcome::UnchangedStub.is_render_reuse());
        assert!(ReuseOutcome::CrossFileRef.is_render_reuse());
        assert!(!ReuseOutcome::DiskContentHit.is_render_reuse());
        assert!(!ReuseOutcome::FreshBypass.is_render_reuse());
        assert!(!ReuseOutcome::Stale.is_render_reuse());
    }

    #[test]
    fn bypasses_are_not_eligible_for_read_reuse() {
        assert!(!ReuseOutcome::FreshBypass.is_ctx_read_eligible());
        assert!(!ReuseOutcome::PolicyBypass.is_ctx_read_eligible());
        assert!(ReuseOutcome::Stale.is_ctx_read_eligible());
    }

    #[test]
    fn stale_mtime_but_same_hash_is_not_stale() {
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("poll.txt");
        std::fs::write(&p, "terminal output line 1").unwrap();

        let mtime1 = std::fs::metadata(&p).unwrap().modified().unwrap();
        let hash = super::compute_md5("terminal output line 1");

        std::thread::sleep(std::time::Duration::from_millis(50));
        {
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .truncate(true)
                .open(&p)
                .unwrap();
            f.write_all(b"terminal output line 1").unwrap();
        }
        let mtime2 = std::fs::metadata(&p).unwrap().modified().unwrap();
        assert_ne!(mtime1, mtime2, "mtime must differ after rewrite");

        let stale = super::is_cache_entry_stale_verified(p.to_str().unwrap(), Some(mtime1), &hash);
        assert!(
            !stale,
            "same content with different mtime must NOT be stale"
        );
    }

    #[test]
    fn stale_mtime_with_changed_content_is_stale() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("changed.txt");
        std::fs::write(&p, "version 1").unwrap();

        let mtime1 = std::fs::metadata(&p).unwrap().modified().unwrap();
        let hash = super::compute_md5("version 1");

        std::thread::sleep(std::time::Duration::from_millis(50));
        std::fs::write(&p, "version 2 with new content").unwrap();

        let stale = super::is_cache_entry_stale_verified(p.to_str().unwrap(), Some(mtime1), &hash);
        assert!(stale, "changed content must be stale");
    }

    #[test]
    fn same_mtime_same_hash_is_not_stale() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("stable.txt");
        std::fs::write(&p, "stable content").unwrap();

        let mtime = std::fs::metadata(&p).unwrap().modified().unwrap();
        let hash = super::compute_md5("stable content");

        let stale = super::is_cache_entry_stale_verified(p.to_str().unwrap(), Some(mtime), &hash);
        assert!(!stale, "unchanged file must not be stale");
    }

    /// #1780: a verification read that fails once and then succeeds must not be
    /// reported as a content change. On Windows a file can still be locked
    /// moments after a write, which made the stub path non-deterministic.
    ///
    /// This exercises the retry itself, not the platform: a real sharing
    /// violation cannot be produced on a POSIX host.
    #[test]
    fn a_transient_failure_is_retried_before_giving_up() {
        let attempts = std::cell::Cell::new(0_u32);
        let result = super::retry_while_transient(|| {
            let n = attempts.get() + 1;
            attempts.set(n);
            if n < 3 {
                Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "file is locked by another handle",
                ))
            } else {
                Ok(42_u32)
            }
        });
        assert_eq!(result.unwrap(), 42);
        assert_eq!(attempts.get(), 3, "must keep trying until it succeeds");
    }

    /// A missing file is a *real* change, not a transient failure — reporting it
    /// immediately matters, because retrying would delay every genuine deletion.
    #[test]
    fn a_missing_file_is_not_retried() {
        let attempts = std::cell::Cell::new(0_u32);
        let result: std::io::Result<()> = super::retry_while_transient(|| {
            attempts.set(attempts.get() + 1);
            Err(std::io::Error::new(std::io::ErrorKind::NotFound, "gone"))
        });
        assert!(result.is_err());
        assert_eq!(attempts.get(), 1, "NotFound must not be retried");
    }

    /// The fail-closed guarantee is unchanged by #1780: content that cannot be
    /// verified is still treated as stale, so a wrong `[unchanged]` stub stays
    /// impossible. The retry only removes the *non-determinism*, never the
    /// safety.
    #[test]
    fn content_that_never_becomes_readable_is_still_stale() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("never-existed.txt");
        let stale = super::is_cache_entry_stale_verified(
            missing.to_str().unwrap(),
            Some(std::time::SystemTime::now()),
            &super::compute_md5("whatever was cached"),
        );
        assert!(stale, "unverifiable content must stay fail-closed");
    }

    #[test]
    fn empty_hash_falls_back_to_mtime_only() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("nohash.txt");
        std::fs::write(&p, "content").unwrap();

        let mtime = std::fs::metadata(&p).unwrap().modified().unwrap();
        assert!(
            !super::is_cache_entry_stale_verified(p.to_str().unwrap(), Some(mtime), ""),
            "same mtime + empty hash = not stale"
        );

        std::thread::sleep(std::time::Duration::from_millis(50));
        std::fs::write(&p, "content").unwrap();
        assert!(
            super::is_cache_entry_stale_verified(p.to_str().unwrap(), Some(mtime), ""),
            "different mtime + empty hash = stale (no hash to rescue)"
        );
    }

    #[test]
    fn large_file_uses_mtime_only() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("big.txt");
        let big = "x".repeat(9 * 1024 * 1024);
        std::fs::write(&p, &big).unwrap();

        let mtime = std::fs::metadata(&p).unwrap().modified().unwrap();
        let hash = super::compute_md5(&big);

        assert!(
            !super::is_cache_entry_stale_verified(p.to_str().unwrap(), Some(mtime), &hash),
            "same mtime on large file = not stale"
        );

        std::thread::sleep(std::time::Duration::from_millis(50));
        std::fs::write(&p, &big).unwrap();
        assert!(
            super::is_cache_entry_stale_verified(p.to_str().unwrap(), Some(mtime), &hash),
            "large files skip hash check even if content matches"
        );
    }

    #[test]
    fn deleted_file_is_stale() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("ephemeral.txt");
        std::fs::write(&p, "temp").unwrap();

        let mtime = std::fs::metadata(&p).unwrap().modified().unwrap();
        let hash = super::compute_md5("temp");

        std::fs::remove_file(&p).unwrap();

        assert!(
            super::is_cache_entry_stale_verified(p.to_str().unwrap(), Some(mtime), &hash),
            "deleted file must be stale"
        );
    }

    #[test]
    fn empty_file_hash_matches_correctly() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("empty.txt");
        std::fs::write(&p, "").unwrap();

        let mtime = std::fs::metadata(&p).unwrap().modified().unwrap();
        let hash = super::compute_md5("");

        std::thread::sleep(std::time::Duration::from_millis(50));
        std::fs::write(&p, "").unwrap();

        assert!(
            !super::is_cache_entry_stale_verified(p.to_str().unwrap(), Some(mtime), &hash),
            "empty file rewritten with same content must not be stale"
        );
    }

    #[test]
    fn rapid_touch_cycles_stay_cached() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("rapid.txt");
        let content = "rapid polling content that stays stable";
        std::fs::write(&p, content).unwrap();

        let mtime_initial = std::fs::metadata(&p).unwrap().modified().unwrap();
        let hash = super::compute_md5(content);

        for _ in 0..10 {
            std::thread::sleep(std::time::Duration::from_millis(10));
            std::fs::write(&p, content).unwrap();

            let stale = super::is_cache_entry_stale_verified(
                p.to_str().unwrap(),
                Some(mtime_initial),
                &hash,
            );
            assert!(
                !stale,
                "repeated rewrites of same content must never be stale"
            );
        }
    }

    #[test]
    fn binary_content_hash_works() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("binary.bin");
        let data: Vec<u8> = (0..=255).collect();
        std::fs::write(&p, &data).unwrap();

        let mtime = std::fs::metadata(&p).unwrap().modified().unwrap();
        let hash = super::compute_md5(&String::from_utf8_lossy(&data));

        std::thread::sleep(std::time::Duration::from_millis(50));
        std::fs::write(&p, &data).unwrap();

        assert!(
            !super::is_cache_entry_stale_verified(p.to_str().unwrap(), Some(mtime), &hash),
            "binary content with same bytes must not be stale"
        );

        let mut changed = data.clone();
        changed[100] = 0;
        std::fs::write(&p, &changed).unwrap();
        assert!(
            super::is_cache_entry_stale_verified(p.to_str().unwrap(), Some(mtime), &hash),
            "binary content with 1 byte changed must be stale"
        );
    }
}
