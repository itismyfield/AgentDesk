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
         Before ADK API work, inspect `GET /api/docs` or `GET /api/docs/{category}`. If docs are missing/wrong, do not use sqlite fallback; report one `API_FRICTION: {...}` marker with endpoint, summary, workaround, and suggested_fix."
            .to_string(),
    )
}

pub(super) fn shared_agent_rules_lookup() -> &'static str {
    "\n\n[Shared Agent Rules Index]\n\
     - Keep changes scoped, verified, and no broader than the current request.\n\
     - Verify user claims against code/data before acting.\n\
     - Prefer `rg` and narrow reads; avoid dumping long tool output.\n\
     - Do not use sqlite for ADK operational data; inspect `/api/docs` first.\n\
     - Source-of-truth map: `docs/source-of-truth.md`; read it before editing prompts, config, skills, policies, or memory surfaces.\n\
     - Memory scope map: `docs/memory-scope.md`; read it before memory cleanup or scope decisions.\n\
     - Full shared prompt source: `~/ObsidianVault/RemoteVault/adk-config/agents/_shared.prompt.md`; read it only when the task needs the detailed shared policy."
}

#[derive(Clone, Debug)]
struct AgentPerformancePromptCacheEntry {
    hour_bucket: i64,
    section: Option<String>,
}

static AGENT_PERFORMANCE_PROMPT_CACHE: OnceLock<
    Mutex<HashMap<String, AgentPerformancePromptCacheEntry>>,
> = OnceLock::new();

/// Hour-aligned cache bucket used by the self-feedback prompt block (#1103).
/// Returning the same bucket guarantees the same cached string is served for
/// the entire hour, which is what makes the system prompt prefix stable
/// (Anthropic prefix cache hits) until the next hourly rollup tick.
pub(super) fn agent_performance_hour_bucket() -> i64 {
    chrono::Utc::now().timestamp() / 3600
}

/// Look up the cached self-feedback section if it is still valid for the
/// supplied hour bucket. Returns `Some(Some(string))` for a fresh hit with a
/// payload, `Some(None)` for a fresh hit that previously resolved to `None`,
/// or `None` when no entry is fresh (caller must repopulate).
fn lookup_cached_agent_performance_section(
    cache_key: &str,
    hour_bucket: i64,
) -> Option<Option<String>> {
    let cache = AGENT_PERFORMANCE_PROMPT_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let guard = cache.lock().ok()?;
    let entry = guard.get(cache_key)?;
    if entry.hour_bucket == hour_bucket {
        Some(entry.section.clone())
    } else {
        None
    }
}

fn store_agent_performance_section(cache_key: String, hour_bucket: i64, section: Option<String>) {
    let cache = AGENT_PERFORMANCE_PROMPT_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Ok(mut guard) = cache.lock() {
        guard.insert(
            cache_key,
            AgentPerformancePromptCacheEntry {
                hour_bucket,
                section,
            },
        );
    }
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
pub(super) fn reset_agent_performance_cache_for_tests() {
    let cache = AGENT_PERFORMANCE_PROMPT_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Ok(mut guard) = cache.lock() {
        guard.clear();
    }
}

/// Resolve the self-feedback section for the supplied role binding using a
/// caller-provided loader. Extracted so tests can drive the cache without
/// touching the live database (#1103).
pub(super) fn agent_performance_prompt_section_with_loader<L>(
    role_binding: Option<&RoleBinding>,
    profile: DispatchProfile,
    hour_bucket: i64,
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
    if let Some(cached) = lookup_cached_agent_performance_section(&cache_key, hour_bucket) {
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

    store_agent_performance_section(cache_key, hour_bucket, section.clone());
    section
}

pub(super) fn agent_performance_prompt_section(
    role_binding: Option<&RoleBinding>,
    profile: DispatchProfile,
) -> Option<String> {
    agent_performance_prompt_section_with_loader(
        role_binding,
        profile,
        agent_performance_hour_bucket(),
        |role_id| crate::services::discord::internal_api::get_agent_quality_prompt_section(role_id),
    )
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
