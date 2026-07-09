//! Layer rendering — system / role / policy / memory / recovery prompt
//! sections that are concatenated into the final system prompt string.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use super::DispatchProfile;
use crate::services::discord::UserRecord;
use crate::services::discord::settings::RoleBinding;

pub(super) const CONTEXT_COMPRESSION_SECTION_ORDER: &str =
    "`Goal`, `Progress`, `Decisions`, `Files`, `Next`";
pub(super) const STALE_TOOL_RESULT_PLACEHOLDER_EXAMPLE: &str =
    "[이전 결과 — 3줄 요약: cargo test failed in src/foo.rs because ...]";

pub(super) fn context_compression_guidance() -> String {
    format!(
        "[Context Compression]\n\
         When conversation compaction happens (`/compact`, automatic compaction, or equivalent summarization), \
         rewrite prior context using these sections in order: {CONTEXT_COMPRESSION_SECTION_ORDER}.\n\
         - Keep each section short, factual, and focused on the latest state.\n\
         - Preserve unresolved blockers, assumptions, failures, and the latest user intent.\n\
         - In `Files`, list only files that still matter and why they matter.\n\
         - Replace stale tool chatter, raw logs, and old command output with placeholders like {STALE_TOOL_RESULT_PLACEHOLDER_EXAMPLE}.\n\
         - Prefer outcomes and follow-up implications over verbatim output, and drop already-resolved repetition once summarized."
    )
}

pub(super) fn tool_output_efficiency_guidance() -> &'static str {
    "[Tool Output Efficiency]\n\
     Large tool results persist in context and increase cost for every subsequent turn.\n\
     - Bash/Read: If output would exceed 10 lines, summarize the result instead of pasting raw output\n\
     - Bash: Use LIMIT clauses for SQL, pipe to head/grep for filtering, avoid tail with large line counts\n\
     - Read: Use offset/limit to read specific sections; do not read entire files when a section is enough\n\
     - Grep: Set head_limit, use narrow glob/type filters, avoid broad patterns that match hundreds of lines\n\
     - Prefer targeted queries over exhaustive dumps"
}

pub(super) fn api_friction_guidance(profile: DispatchProfile) -> Option<String> {
    (profile == DispatchProfile::Full).then_some(
        "\n\n[ADK API Usage]\n\
         Before ADK API work, inspect `GET /api/docs` or `GET /api/docs/{category}`. If docs are missing/wrong, do not use sqlite fallback; report one `API_FRICTION: {...}` marker with endpoint, summary, workaround, and suggested_fix. The runtime stores valid markers under Memento `topic=api-friction` when Memento is configured."
            .to_string(),
    )
}

/// True when the agent's current working directory is (or is rooted at) an
/// AgentDesk checkout — detected by the presence of *both* repo-relative
/// source-of-truth documents. Used to gate repo-relative prompt references
/// (`docs/source-of-truth.md`, `docs/memory-scope.md`) so they are never
/// injected into agents whose workspace is a *different* repository (#4314),
/// where those files do not exist and the reference would point at nothing.
pub(super) fn workspace_has_agentdesk_docs(current_path: &str) -> bool {
    workspace_has_agentdesk_docs_with(current_path, |p| std::path::Path::new(p).exists())
}

/// Filesystem-injectable seam for [`workspace_has_agentdesk_docs`] so tests
/// can drive the path-existence decision deterministically without touching
/// the real filesystem (#4314).
pub(super) fn workspace_has_agentdesk_docs_with(
    current_path: &str,
    exists: impl Fn(&str) -> bool,
) -> bool {
    let base = current_path.trim().trim_end_matches('/');
    if base.is_empty() {
        return false;
    }
    exists(&format!("{base}/docs/source-of-truth.md"))
        && exists(&format!("{base}/docs/memory-scope.md"))
}

pub(super) fn shared_agent_rules_lookup(current_path: &str) -> String {
    shared_agent_rules_lookup_with(current_path, |p| std::path::Path::new(p).exists())
}

/// Filesystem-injectable seam for [`shared_agent_rules_lookup`]. The generic
/// rules and the absolute `~/ObsidianVault/...` shared-prompt source are always
/// injected; only the two repo-relative `docs/*` references are gated on the
/// current workspace actually being an AgentDesk checkout (#4314).
fn shared_agent_rules_lookup_with(current_path: &str, exists: impl Fn(&str) -> bool) -> String {
    let mut section = String::from(
        "\n\n[Shared Agent Rules Index]\n\
         - Keep changes scoped, verified, and no broader than the current request.\n\
         - Verify user claims against code/data before acting.\n\
         - Prefer `rg` and narrow reads; avoid dumping long tool output.\n\
         - Do not use sqlite for ADK operational data; inspect `/api/docs` first.",
    );
    if workspace_has_agentdesk_docs_with(current_path, &exists) {
        section.push_str(
            "\n- Source-of-truth map: `docs/source-of-truth.md`; read it before editing prompts, config, skills, policies, or memory surfaces.\n\
             - Memory scope map: `docs/memory-scope.md`; read it before memory cleanup or scope decisions.",
        );
    }
    section.push_str(
        "\n- Full shared prompt source: `~/ObsidianVault/RemoteVault/adk-config/agents/_shared.prompt.md`; read it only when the task needs the detailed shared policy.",
    );
    section
}

#[derive(Clone, Debug)]
struct AgentPerformancePromptCacheEntry {
    /// Day-aligned bucket used to decide cache freshness. Stored as `i64`
    /// (not the field name) so the existing tests that thread arbitrary
    /// integer buckets keep working — the field is opaque to the cache
    /// layer, the caller decides the bucket cadence.
    day_bucket: i64,
    section: Option<String>,
}

static AGENT_PERFORMANCE_PROMPT_CACHE: OnceLock<
    Mutex<HashMap<String, AgentPerformancePromptCacheEntry>>,
> = OnceLock::new();

/// Observability counters for the prompt prefix cache (#2666). Exposed via
/// [`agent_performance_cache_metrics`]. Hot path: incremented on every
/// lookup so the cache hit-rate can be inspected from the dashboard /
/// health endpoint without instrumenting individual call sites.
static AGENT_PERFORMANCE_CACHE_HITS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);
static AGENT_PERFORMANCE_CACHE_MISSES: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// Day-aligned cache bucket used by the self-feedback prompt block (#1103,
/// #2666). The Agent Performance rollup is computed once per day, so a
/// daily bucket gives ~100% cache hit rate within a single UTC day —
/// matching the upstream rollup cadence. The previous implementation used
/// an hourly bucket, which forced a DB re-fetch every hour even though the
/// underlying rollup never changed within the day.
///
/// The day boundary is UTC; rollovers are observable via the `day_bucket`
/// number changing. Operators can force a refresh mid-day via
/// [`invalidate_agent_performance_cache`].
pub(super) fn agent_performance_day_bucket() -> i64 {
    chrono::Utc::now().timestamp() / 86_400
}

/// Compatibility alias for the legacy name. Retained because external
/// tests still reference `agent_performance_hour_bucket`; the function now
/// returns a *day* bucket, but the name was deliberately kept to minimize
/// the call-site diff. Prefer [`agent_performance_day_bucket`] in new code.
// #3034: test-only contract surface (legacy-name compatibility alias).
#[allow(dead_code)]
pub(super) fn agent_performance_hour_bucket() -> i64 {
    agent_performance_day_bucket()
}

/// Snapshot of cache observability counters. `(hits, misses)`. Atomically
/// consistent only per-field; we accept a torn read between the two values
/// since they are used for log/metric emission, not for control flow.
// #3034: cache-observability surface (#2666); exercised by tests, dashboard/
// health-endpoint wiring pending.
#[allow(dead_code)]
pub fn agent_performance_cache_metrics() -> (u64, u64) {
    (
        AGENT_PERFORMANCE_CACHE_HITS.load(std::sync::atomic::Ordering::Relaxed),
        AGENT_PERFORMANCE_CACHE_MISSES.load(std::sync::atomic::Ordering::Relaxed),
    )
}

/// Explicitly drop every cached self-feedback entry. Intended for the
/// daily rollup writer to call after persisting new data, so the next turn
/// picks up the fresh snapshot without waiting for the UTC day boundary.
///
/// Idempotent — safe to call when no entries exist.
// #3034: cache-invalidation hook for the daily rollup writer (#2666);
// exercised by tests, rollup-writer wiring pending.
#[allow(dead_code)]
pub fn invalidate_agent_performance_cache() {
    let cache = AGENT_PERFORMANCE_PROMPT_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Ok(mut guard) = cache.lock() {
        guard.clear();
    }
}

/// Drop the cached entry for a single role. Useful when a per-role
/// rollup is refreshed while leaving others stale.
// #3034: per-role cache-invalidation hook (#2666); exercised by tests,
// rollup-writer wiring pending.
#[allow(dead_code)]
pub fn invalidate_agent_performance_cache_for_role(role_id: &str) {
    let cache = AGENT_PERFORMANCE_PROMPT_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Ok(mut guard) = cache.lock() {
        guard.remove(role_id);
    }
}

/// Look up the cached self-feedback section if it is still valid for the
/// supplied hour bucket. Returns `Some(Some(string))` for a fresh hit with a
/// payload, `Some(None)` for a fresh hit that previously resolved to `None`,
/// or `None` when no entry is fresh (caller must repopulate).
fn lookup_cached_agent_performance_section(
    cache_key: &str,
    day_bucket: i64,
) -> Option<Option<String>> {
    let cache = AGENT_PERFORMANCE_PROMPT_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let guard = cache.lock().ok()?;
    let entry = guard.get(cache_key)?;
    if entry.day_bucket == day_bucket {
        AGENT_PERFORMANCE_CACHE_HITS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Some(entry.section.clone())
    } else {
        None
    }
}

fn store_agent_performance_section(cache_key: String, day_bucket: i64, section: Option<String>) {
    AGENT_PERFORMANCE_CACHE_MISSES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let cache = AGENT_PERFORMANCE_PROMPT_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Ok(mut guard) = cache.lock() {
        guard.insert(
            cache_key,
            AgentPerformancePromptCacheEntry {
                day_bucket,
                section,
            },
        );
    }
}

/// Resolve the self-feedback section for the supplied role binding using a
/// caller-provided loader. Extracted so tests can drive the cache without
/// touching the live database (#1103).
pub(super) fn agent_performance_prompt_section_with_loader<L>(
    role_binding: Option<&RoleBinding>,
    profile: DispatchProfile,
    day_bucket: i64,
    loader: L,
) -> Option<String>
where
    L: FnOnce(&str) -> Result<Option<String>, String>,
{
    let binding = role_binding?;
    // A/B toggle (#1103): the channel-level `self_feedback_enabled` flag (named
    // `quality_feedback_injection_enabled` on the resolved binding) gates the
    // entire injection. ReviewLite turns also skip — they already strip
    // optional context for cost.
    if profile != DispatchProfile::Full || !binding.quality_feedback_injection_enabled {
        return None;
    }

    let cache_key = binding.role_id.clone();
    if let Some(cached) = lookup_cached_agent_performance_section(&cache_key, day_bucket) {
        return cached;
    }

    let section = match loader(&binding.role_id) {
        Ok(section) => section,
        Err(error) => {
            tracing::warn!(
                role_id = %binding.role_id,
                "[quality] failed to load agent performance prompt section: {error}"
            );
            return None;
        }
    };

    store_agent_performance_section(cache_key, day_bucket, section.clone());
    section
}

pub(super) fn agent_performance_prompt_section(
    role_binding: Option<&RoleBinding>,
    profile: DispatchProfile,
) -> Option<String> {
    agent_performance_prompt_section_with_loader(
        role_binding,
        profile,
        agent_performance_day_bucket(),
        |role_id| crate::services::discord::internal_api::get_agent_quality_prompt_section(role_id),
    )
}

/// Test-only helper that resets the cache state and the hit/miss counters.
/// Not gated on the removed SQLite-only harness because the bucket-cadence regression
/// tests (#2666) need it under the default test build too.
#[cfg(test)]
pub(super) fn reset_agent_performance_cache_for_layer_rendering_tests() {
    let cache = AGENT_PERFORMANCE_PROMPT_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Ok(mut guard) = cache.lock() {
        guard.clear();
    }
    AGENT_PERFORMANCE_CACHE_HITS.store(0, std::sync::atomic::Ordering::Relaxed);
    AGENT_PERFORMANCE_CACHE_MISSES.store(0, std::sync::atomic::Ordering::Relaxed);
}

pub(super) fn render_channel_participants(
    discord_context: &str,
    channel_participants: &[UserRecord],
) -> String {
    let is_dm_context = discord_context.trim() == "Discord context: DM";
    let mut lines = vec!["Channel participants:".to_string()];
    if channel_participants.is_empty() {
        lines.push("- none recorded yet".to_string());
        return lines.join("\n");
    }

    for (idx, user) in channel_participants.iter().enumerate() {
        let mut line = format!("- {}", user.label());
        if is_dm_context && channel_participants.len() == 1 && idx == 0 {
            line.push_str(" [DM requester]");
        }
        lines.push(line);
    }
    lines.join("\n")
}

#[cfg(test)]
mod workspace_aware_shared_rules_tests {
    //! #4314 — the repo-relative `docs/*` references must only appear when the
    //! agent's workspace actually is an AgentDesk checkout. The `_with(exists)`
    //! seam makes the path-existence decision injectable so these are
    //! deterministic and prove the guard bidirectionally (removing the guard
    //! makes one direction fail on its own assert, not on a compile error).
    use super::*;

    #[test]
    fn helper_requires_both_docs_and_rejects_empty() {
        // Both docs present → AgentDesk workspace.
        assert!(workspace_has_agentdesk_docs_with("/repo", |_| true));
        // Neither present → foreign workspace.
        assert!(!workspace_has_agentdesk_docs_with("/repo", |_| false));
        // Only one of the two present → still not treated as AgentDesk (AND).
        assert!(!workspace_has_agentdesk_docs_with("/repo", |p| p.ends_with("source-of-truth.md")));
        assert!(!workspace_has_agentdesk_docs_with("/repo", |p| p.ends_with("memory-scope.md")));
        // Empty / whitespace path → false without touching the filesystem.
        assert!(!workspace_has_agentdesk_docs_with("   ", |_| panic!(
            "must not probe empty path"
        )));
    }

    #[test]
    fn helper_probes_current_path_rooted_docs() {
        // The probed paths are `<cwd>/docs/source-of-truth.md` and
        // `<cwd>/docs/memory-scope.md`, and a trailing slash is tolerated.
        let mut probed: Vec<String> = Vec::new();
        let cell = std::cell::RefCell::new(&mut probed);
        let result = workspace_has_agentdesk_docs_with("/some/repo/", |p| {
            cell.borrow_mut().push(p.to_string());
            true
        });
        assert!(result);
        assert!(probed.contains(&"/some/repo/docs/source-of-truth.md".to_string()));
        assert!(probed.contains(&"/some/repo/docs/memory-scope.md".to_string()));
    }

    #[test]
    fn foreign_workspace_omits_repo_relative_doc_refs() {
        // T1 (guard mutation, A block): with the docs absent, the two
        // repo-relative references must NOT be injected, but the generic block
        // and the absolute shared-prompt source stay. Removing the guard so the
        // refs are always injected makes THIS assert fail on its own.
        let section = shared_agent_rules_lookup_with("/foreign/repo", |_| false);
        assert!(section.contains("[Shared Agent Rules Index]"));
        assert!(section.contains("Do not use sqlite for ADK operational data"));
        assert!(section.contains("_shared.prompt.md"));
        assert!(
            !section.contains("docs/source-of-truth.md"),
            "foreign workspace must not reference docs/source-of-truth.md, got: {section}"
        );
        assert!(
            !section.contains("docs/memory-scope.md"),
            "foreign workspace must not reference docs/memory-scope.md, got: {section}"
        );
    }

    #[test]
    fn agentdesk_workspace_keeps_repo_relative_doc_refs() {
        // T2 (reverse mutation, A block): with the docs present, both
        // repo-relative references must be injected. Forcing the guard to
        // always omit makes THIS assert fail on its own.
        let section = shared_agent_rules_lookup_with("/agentdesk", |_| true);
        assert!(section.contains("[Shared Agent Rules Index]"));
        assert!(
            section.contains("docs/source-of-truth.md"),
            "AgentDesk workspace must keep docs/source-of-truth.md, got: {section}"
        );
        assert!(
            section.contains("docs/memory-scope.md"),
            "AgentDesk workspace must keep docs/memory-scope.md, got: {section}"
        );
        assert!(section.contains("_shared.prompt.md"));
    }
}

#[cfg(test)]
mod bucket_cadence_tests {
    //! #2666 — verify that the prompt prefix cache now matches the daily
    //! cadence of the upstream Agent Performance rollup, and that the
    //! observability counters / explicit invalidation hooks behave.
    use super::*;
    use std::sync::Mutex;

    // The cache + counters are process-global. Serialize the tests in this
    // module so they don't observe each other's atomic increments.
    static BUCKET_TEST_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn day_bucket_increments_at_utc_midnight_only() {
        let _guard = BUCKET_TEST_LOCK.lock().unwrap();
        // 86_400 = seconds in a day. Two timestamps in the same UTC day
        // must collapse to the same bucket; two in adjacent days must not.
        let same_day_a = 86_400i64 * 100;
        let same_day_b = same_day_a + 86_399; // last second of the same day
        let next_day = same_day_a + 86_400;
        assert_eq!(same_day_a / 86_400, same_day_b / 86_400);
        assert_ne!(same_day_a / 86_400, next_day / 86_400);
    }

    #[test]
    fn hour_bucket_alias_returns_day_bucket() {
        let _guard = BUCKET_TEST_LOCK.lock().unwrap();
        // The legacy name is retained for source-compat, but it now
        // returns the daily value too. We assert by sampling both at
        // (approximately) the same instant; on a slow machine the second
        // call could roll the day, so retry up to a few times.
        for _ in 0..5 {
            let day = agent_performance_day_bucket();
            let hour = agent_performance_hour_bucket();
            if day == hour {
                return;
            }
        }
        panic!("agent_performance_hour_bucket must return the day bucket");
    }

    #[test]
    fn invalidate_drops_cached_entry() {
        let _guard = BUCKET_TEST_LOCK.lock().unwrap();
        reset_agent_performance_cache_for_layer_rendering_tests();
        // Hand-craft an entry without going through a RoleBinding so we
        // don't need the Discord settings types in this layer's tests.
        store_agent_performance_section("role-x".into(), 1, Some("payload-v1".into()));
        assert_eq!(
            lookup_cached_agent_performance_section("role-x", 1),
            Some(Some("payload-v1".into()))
        );
        invalidate_agent_performance_cache();
        assert_eq!(
            lookup_cached_agent_performance_section("role-x", 1),
            None,
            "invalidate must remove all entries"
        );
    }

    #[test]
    fn invalidate_for_role_only_touches_that_role() {
        let _guard = BUCKET_TEST_LOCK.lock().unwrap();
        reset_agent_performance_cache_for_layer_rendering_tests();
        store_agent_performance_section("role-A".into(), 7, Some("a".into()));
        store_agent_performance_section("role-B".into(), 7, Some("b".into()));
        invalidate_agent_performance_cache_for_role("role-A");
        assert_eq!(
            lookup_cached_agent_performance_section("role-A", 7),
            None,
            "role-A entry should be gone"
        );
        assert_eq!(
            lookup_cached_agent_performance_section("role-B", 7),
            Some(Some("b".into())),
            "role-B entry must survive a targeted invalidation"
        );
    }

    #[test]
    fn metrics_counters_track_hits_and_misses() {
        let _guard = BUCKET_TEST_LOCK.lock().unwrap();
        reset_agent_performance_cache_for_layer_rendering_tests();
        // First lookup = miss path (cache empty -> store -> miss count
        // ++). We exercise store directly because the helper increments
        // the miss counter every time a value is stored.
        store_agent_performance_section("role-m".into(), 9, Some("v".into()));
        let (h1, m1) = agent_performance_cache_metrics();
        assert_eq!((h1, m1), (0, 1));

        // Same-bucket lookup is a hit.
        assert_eq!(
            lookup_cached_agent_performance_section("role-m", 9),
            Some(Some("v".into()))
        );
        let (h2, m2) = agent_performance_cache_metrics();
        assert_eq!((h2, m2), (1, 1));

        // Different bucket = miss (returns None to caller). The hit
        // counter must NOT advance.
        assert_eq!(lookup_cached_agent_performance_section("role-m", 10), None);
        let (h3, m3) = agent_performance_cache_metrics();
        assert_eq!((h3, m3), (1, 1));
    }

    #[test]
    fn bucket_rollover_causes_miss_then_refill() {
        let _guard = BUCKET_TEST_LOCK.lock().unwrap();
        reset_agent_performance_cache_for_layer_rendering_tests();
        store_agent_performance_section("role-r".into(), 1, Some("day1".into()));
        // Day +1: stale entry -> lookup returns None.
        assert_eq!(lookup_cached_agent_performance_section("role-r", 2), None);
        // Caller refills with day-2 payload.
        store_agent_performance_section("role-r".into(), 2, Some("day2".into()));
        assert_eq!(
            lookup_cached_agent_performance_section("role-r", 2),
            Some(Some("day2".into()))
        );
        // The old entry was overwritten in place; querying day-1 again
        // is now a miss (no rolling history).
        assert_eq!(lookup_cached_agent_performance_section("role-r", 1), None);
    }
}
