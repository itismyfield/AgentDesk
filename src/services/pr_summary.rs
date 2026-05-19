//! PR summary cache (#2654).
//!
//! Audit workflow report `audit/2026-05-19` observed that agent sessions
//! repeatedly invoke `gh pr view --json …` for the same PRs — e.g. the
//! chunk-01 mega-session executed `gh pr (view|checks|create|merge) × 1229`.
//! The fetch is purely idempotent given a stable PR head SHA, so we cache the
//! payload keyed by `(repo, pr_number)` with the head SHA recorded alongside.
//!
//! This module is the canonical fetcher. Routes / agent tooling should call
//! [`fetch`] (or the global [`shared`] cache) instead of shelling out to
//! `gh pr view` directly. The cache may be invalidated explicitly by
//! webhook-style listeners via [`PrSummaryCache::invalidate`] when GitHub
//! signals a `pull_request.synchronize` / `closed` / `merged` event.
//!
//! Sizing: in-memory only, with an LRU-ish bound on entries. We do not yet
//! persist to Postgres because the data is cheap to refetch and we want to
//! avoid coupling the cache to migrations.

use std::sync::Arc;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use dashmap::DashMap;
use serde::{Deserialize, Serialize};

use crate::github::{self, PrView};

/// Default TTL for an open PR — short, because reviews / commits land
/// continuously. The cache is still a big win at this TTL since a single
/// session typically references the same PR several times per second when
/// it's iterating on a review.
pub const DEFAULT_OPEN_TTL: Duration = Duration::from_secs(60);

/// Default TTL for a closed or merged PR — long, because the metadata is
/// effectively immutable from the workflow's perspective.
pub const DEFAULT_CLOSED_TTL: Duration = Duration::from_secs(60 * 60);

/// Cap on the number of cached PRs. When exceeded we evict the oldest
/// `fetched_at` entries. We deliberately keep this small — the working set
/// for a typical session is well under 100.
pub const DEFAULT_MAX_ENTRIES: usize = 512;

/// Identifies a cached PR. We normalise `repo` to lowercase to avoid storing
/// the same PR twice under "Owner/Repo" vs "owner/repo".
#[derive(Debug, Clone, Eq, Hash, PartialEq)]
struct CacheKey {
    repo: String,
    pr_number: i64,
}

impl CacheKey {
    fn new(repo: &str, pr_number: i64) -> Self {
        Self {
            repo: repo.trim().to_ascii_lowercase(),
            pr_number,
        }
    }
}

/// One cached entry. The `head_sha` is the value GitHub returned on the
/// last successful fetch — webhook handlers can compare against this to
/// decide whether they need to invalidate.
#[derive(Debug, Clone)]
struct CacheEntry {
    /// Time the value was fetched. Used for TTL and eviction.
    fetched_at: Instant,
    /// PR state at fetch time. We use this to pick the long vs short TTL
    /// without having to inspect the payload on every lookup.
    state: PrState,
    /// Head ref OID. May be `None` for very fresh PRs that haven't yet
    /// landed a commit.
    head_sha: Option<String>,
    /// The PR payload itself, ready to hand back to callers.
    view: Arc<PrView>,
}

/// Coarse classification of PR state. We do not need the full GitHub enum
/// at this layer — only whether the entry is "live" (open) or "settled"
/// (closed/merged), which determines TTL.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum PrState {
    Open,
    Settled,
}

impl PrState {
    fn from_view(view: &PrView) -> Self {
        // GitHub returns "OPEN", "CLOSED", or "MERGED". Anything else
        // (unexpected values from future GitHub API changes) is treated as
        // settled to err on the side of *less* caching, not more.
        match view.state.to_ascii_uppercase().as_str() {
            "OPEN" => PrState::Open,
            _ => PrState::Settled,
        }
    }
}

/// Result of a successful lookup. Callers receive the payload plus a
/// freshness signal so they can decide whether to render a "cached at"
/// disclosure to the user.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PrSummary {
    /// The repository the PR lives in, normalised to lowercase.
    pub repo: String,
    /// PR number.
    pub pr_number: i64,
    /// Whether this response came from the cache or required a fresh fetch.
    pub cache_hit: bool,
    /// Age in seconds since the cached payload was last refreshed from
    /// GitHub. Always 0 on a fresh fetch.
    pub age_seconds: u64,
    /// Head ref OID as known to GitHub at fetch time. May be `None`.
    pub head_sha: Option<String>,
    /// Full payload.
    pub view: PrView,
}

/// Options accepted by [`PrSummaryCache::fetch`].
#[derive(Debug, Clone, Default)]
pub struct FetchOptions {
    /// If true, ignore any cached entry and refetch from GitHub.
    pub force_refresh: bool,
    /// If supplied, treat the cached entry as stale unless its
    /// `head_sha` matches. Useful when the caller has just learned the
    /// PR's current head SHA from another channel (e.g. a webhook) and
    /// wants idempotent behavior.
    pub expected_head_sha: Option<String>,
}

impl FetchOptions {
    pub fn force() -> Self {
        Self {
            force_refresh: true,
            expected_head_sha: None,
        }
    }
}

/// In-memory PR summary cache. Cloning is cheap (`Arc`-shared map).
#[derive(Clone)]
pub struct PrSummaryCache {
    inner: Arc<Inner>,
}

struct Inner {
    entries: DashMap<CacheKey, CacheEntry>,
    open_ttl: Duration,
    closed_ttl: Duration,
    max_entries: usize,
}

impl Default for PrSummaryCache {
    fn default() -> Self {
        Self::new(DEFAULT_OPEN_TTL, DEFAULT_CLOSED_TTL, DEFAULT_MAX_ENTRIES)
    }
}

impl PrSummaryCache {
    /// Construct a fresh cache with explicit TTLs and capacity.
    pub fn new(open_ttl: Duration, closed_ttl: Duration, max_entries: usize) -> Self {
        Self {
            inner: Arc::new(Inner {
                entries: DashMap::new(),
                open_ttl,
                closed_ttl,
                // Always allow at least one entry — a zero cap would
                // silently disable caching, which is almost never the
                // operator's intent.
                max_entries: max_entries.max(1),
            }),
        }
    }

    /// Look up a PR, optionally bypassing the cache. The fetcher callback
    /// is invoked only on a miss / forced refresh. Tests use a synchronous
    /// callback to avoid spawning real `gh` processes; the production
    /// path uses [`fetch`].
    ///
    /// The fetcher receives `(repo, pr_number)` and is expected to return
    /// a fully-populated [`PrView`] on success or a stringified error.
    pub fn fetch_with<F>(
        &self,
        repo: &str,
        pr_number: i64,
        opts: &FetchOptions,
        fetcher: F,
    ) -> Result<PrSummary, String>
    where
        F: FnOnce(&str, i64) -> Result<PrView, String>,
    {
        let key = CacheKey::new(repo, pr_number);
        let now = Instant::now();

        // Fast path: look in cache first.
        if !opts.force_refresh
            && let Some(entry) = self.inner.entries.get(&key)
        {
            let age = now.saturating_duration_since(entry.fetched_at);
            let ttl = match entry.state {
                PrState::Open => self.inner.open_ttl,
                PrState::Settled => self.inner.closed_ttl,
            };
            let sha_matches = match &opts.expected_head_sha {
                // If the caller knows the current head SHA and it doesn't
                // match the cached one, we *must* refetch — the PR has
                // moved on under us. Empty strings are treated as "no
                // expectation" so callers can pass `Some("")` without
                // accidentally invalidating every entry.
                Some(expected) if !expected.is_empty() => {
                    entry.head_sha.as_deref() == Some(expected.as_str())
                }
                _ => true,
            };
            if age < ttl && sha_matches {
                return Ok(PrSummary {
                    repo: key.repo.clone(),
                    pr_number: key.pr_number,
                    cache_hit: true,
                    age_seconds: age.as_secs(),
                    head_sha: entry.head_sha.clone(),
                    view: PrView::clone(&entry.view),
                });
            }
        }

        // Slow path: fetch from GitHub. We do not hold any DashMap lock
        // across the fetch — `dashmap::get` releases its shard guard when
        // we drop the `Ref`, which we do above.
        let view = fetcher(&key.repo, pr_number)?;
        let head_sha = view.head_ref_oid.clone();
        let state = PrState::from_view(&view);
        let view_arc = Arc::new(view);

        // Evict before inserting so we never exceed the cap.
        self.evict_if_needed();

        self.inner.entries.insert(
            key.clone(),
            CacheEntry {
                fetched_at: now,
                state,
                head_sha: head_sha.clone(),
                view: Arc::clone(&view_arc),
            },
        );

        Ok(PrSummary {
            repo: key.repo,
            pr_number: key.pr_number,
            cache_hit: false,
            age_seconds: 0,
            head_sha,
            view: PrView::clone(&view_arc),
        })
    }

    /// Production fetcher: calls `gh pr view` via the `github` adapter.
    pub fn fetch(
        &self,
        repo: &str,
        pr_number: i64,
        opts: &FetchOptions,
    ) -> Result<PrSummary, String> {
        self.fetch_with(repo, pr_number, opts, github::fetch_pr_view)
    }

    /// Drop the cached entry for a PR. Safe to call when there is no
    /// matching entry (no-op). Webhook handlers should call this on
    /// `pull_request.synchronize`, `pull_request.closed`, and any
    /// review-state event that changes the visible summary.
    pub fn invalidate(&self, repo: &str, pr_number: i64) {
        let key = CacheKey::new(repo, pr_number);
        self.inner.entries.remove(&key);
    }

    /// Drop every entry. Used by tests and operator tooling.
    pub fn clear(&self) {
        self.inner.entries.clear();
    }

    /// Current number of cached entries. Exposed for instrumentation.
    pub fn len(&self) -> usize {
        self.inner.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.inner.entries.is_empty()
    }

    /// Evict the oldest entry when at capacity. We only evict a single
    /// entry per call so the cost is bounded by the cap — important
    /// because eviction runs on the fetch hot path.
    fn evict_if_needed(&self) {
        if self.inner.entries.len() < self.inner.max_entries {
            return;
        }
        let mut oldest: Option<(CacheKey, Instant)> = None;
        for entry in self.inner.entries.iter() {
            let fetched_at = entry.value().fetched_at;
            match &oldest {
                None => oldest = Some((entry.key().clone(), fetched_at)),
                Some((_, ts)) if fetched_at < *ts => {
                    oldest = Some((entry.key().clone(), fetched_at))
                }
                _ => {}
            }
        }
        if let Some((victim, _)) = oldest {
            self.inner.entries.remove(&victim);
        }
    }
}

/// Process-wide cache shared by routes / agent integrations. Constructed
/// lazily so binaries that never touch the GitHub path do not pay the
/// allocation cost.
pub fn shared() -> &'static PrSummaryCache {
    static CACHE: OnceLock<PrSummaryCache> = OnceLock::new();
    CACHE.get_or_init(PrSummaryCache::default)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn stub_view(state: &str, sha: &str) -> PrView {
        PrView {
            number: 42,
            state: state.to_string(),
            title: "test".to_string(),
            body: None,
            url: "https://example.test/pr/42".to_string(),
            is_draft: false,
            head_ref_oid: Some(sha.to_string()),
            head_ref_name: Some("feat/x".to_string()),
            base_ref_name: Some("main".to_string()),
            mergeable: None,
            merge_state_status: None,
            author: None,
            labels: vec![],
            files: vec![],
            reviews: vec![],
            comments: vec![],
            status_check_rollup: vec![],
            created_at: None,
            updated_at: None,
            merged_at: None,
            closed_at: None,
            additions: None,
            deletions: None,
            changed_files: None,
        }
    }

    #[test]
    fn cache_hit_on_repeated_lookup() {
        let cache = PrSummaryCache::default();
        let calls = AtomicUsize::new(0);
        let fetcher = |_repo: &str, _pr: i64| {
            calls.fetch_add(1, Ordering::SeqCst);
            Ok(stub_view("OPEN", "sha-1"))
        };

        let first = cache
            .fetch_with("Owner/Repo", 42, &FetchOptions::default(), fetcher)
            .unwrap();
        assert!(!first.cache_hit);

        let calls2 = AtomicUsize::new(0);
        let second = cache
            .fetch_with(
                // Different casing should still hit the cache because we
                // normalize the key.
                "owner/repo",
                42,
                &FetchOptions::default(),
                |_, _| {
                    calls2.fetch_add(1, Ordering::SeqCst);
                    Ok(stub_view("OPEN", "sha-1"))
                },
            )
            .unwrap();
        assert!(second.cache_hit);
        assert_eq!(calls2.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn force_refresh_bypasses_cache() {
        let cache = PrSummaryCache::default();
        cache
            .fetch_with("o/r", 1, &FetchOptions::default(), |_, _| {
                Ok(stub_view("OPEN", "sha-a"))
            })
            .unwrap();

        let called = AtomicUsize::new(0);
        let refreshed = cache
            .fetch_with("o/r", 1, &FetchOptions::force(), |_, _| {
                called.fetch_add(1, Ordering::SeqCst);
                Ok(stub_view("OPEN", "sha-b"))
            })
            .unwrap();
        assert!(!refreshed.cache_hit);
        assert_eq!(called.load(Ordering::SeqCst), 1);
        assert_eq!(refreshed.head_sha.as_deref(), Some("sha-b"));
    }

    #[test]
    fn expected_sha_mismatch_triggers_refetch() {
        let cache = PrSummaryCache::default();
        cache
            .fetch_with("o/r", 1, &FetchOptions::default(), |_, _| {
                Ok(stub_view("OPEN", "sha-a"))
            })
            .unwrap();

        let opts = FetchOptions {
            force_refresh: false,
            expected_head_sha: Some("sha-NEW".to_string()),
        };
        let calls = AtomicUsize::new(0);
        let result = cache
            .fetch_with("o/r", 1, &opts, |_, _| {
                calls.fetch_add(1, Ordering::SeqCst);
                Ok(stub_view("OPEN", "sha-NEW"))
            })
            .unwrap();
        assert!(!result.cache_hit);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(result.head_sha.as_deref(), Some("sha-NEW"));
    }

    #[test]
    fn expected_sha_empty_is_no_op() {
        // Operators may legitimately pass `Some("")` to mean "no expectation".
        // Make sure it does not accidentally evict every entry.
        let cache = PrSummaryCache::default();
        cache
            .fetch_with("o/r", 1, &FetchOptions::default(), |_, _| {
                Ok(stub_view("OPEN", "sha-a"))
            })
            .unwrap();
        let opts = FetchOptions {
            force_refresh: false,
            expected_head_sha: Some(String::new()),
        };
        let result = cache
            .fetch_with("o/r", 1, &opts, |_, _| panic!("should not refetch"))
            .unwrap();
        assert!(result.cache_hit);
    }

    #[test]
    fn invalidate_drops_entry() {
        let cache = PrSummaryCache::default();
        cache
            .fetch_with("o/r", 5, &FetchOptions::default(), |_, _| {
                Ok(stub_view("OPEN", "sha-a"))
            })
            .unwrap();
        assert_eq!(cache.len(), 1);
        cache.invalidate("o/r", 5);
        assert_eq!(cache.len(), 0);
    }

    #[test]
    fn open_ttl_expires_entry() {
        // We can't actually advance Instant in tests cheaply, so we use a
        // zero-duration TTL to force every lookup to be considered stale.
        let cache = PrSummaryCache::new(Duration::ZERO, Duration::ZERO, 8);
        cache
            .fetch_with("o/r", 7, &FetchOptions::default(), |_, _| {
                Ok(stub_view("OPEN", "sha-a"))
            })
            .unwrap();
        let calls = AtomicUsize::new(0);
        let second = cache
            .fetch_with("o/r", 7, &FetchOptions::default(), |_, _| {
                calls.fetch_add(1, Ordering::SeqCst);
                Ok(stub_view("OPEN", "sha-b"))
            })
            .unwrap();
        assert!(!second.cache_hit);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn closed_pr_uses_longer_ttl() {
        // Open TTL is zero (always stale), closed TTL is large (always
        // fresh). A merged PR should still be a hit on the second lookup.
        let cache = PrSummaryCache::new(Duration::ZERO, Duration::from_secs(3600), 8);
        cache
            .fetch_with("o/r", 9, &FetchOptions::default(), |_, _| {
                Ok(stub_view("MERGED", "sha-z"))
            })
            .unwrap();
        let second = cache
            .fetch_with("o/r", 9, &FetchOptions::default(), |_, _| {
                panic!("merged PR should be a cache hit")
            })
            .unwrap();
        assert!(second.cache_hit);
    }

    #[test]
    fn eviction_caps_entries() {
        let cache = PrSummaryCache::new(
            Duration::from_secs(60),
            Duration::from_secs(60),
            // Cap of 2 — third insert should evict the oldest.
            2,
        );
        for n in 1..=3 {
            cache
                .fetch_with("o/r", n, &FetchOptions::default(), |_, _| {
                    Ok(stub_view("OPEN", &format!("sha-{n}")))
                })
                .unwrap();
        }
        assert!(cache.len() <= 2, "cache exceeded cap: {}", cache.len());
    }

    #[test]
    fn fetcher_error_is_propagated_and_entry_unchanged() {
        let cache = PrSummaryCache::default();
        cache
            .fetch_with("o/r", 1, &FetchOptions::default(), |_, _| {
                Ok(stub_view("OPEN", "sha-a"))
            })
            .unwrap();
        let err = cache
            .fetch_with("o/r", 1, &FetchOptions::force(), |_, _| {
                Err("network down".to_string())
            })
            .unwrap_err();
        assert!(err.contains("network"));
        // The old entry is still there even though the refresh failed —
        // we don't tear down good data on a transient gh failure.
        assert_eq!(cache.len(), 1);
    }
}
