use anyhow::Result;
use serde_json::json;
use sqlx::PgPool;

use crate::db::agents::AgentChannelBindings;
use crate::db::agents::load_agent_channel_bindings_pg;
use crate::services::git::GitCommand;
use crate::services::provider::ProviderKind;

use super::dispatch_channel::provider_from_channel_suffix;

#[derive(Debug, Clone, PartialEq, Eq)]
struct DispatchExecutionTarget {
    reviewed_commit: String,
    branch: Option<String>,
    worktree_path: Option<String>,
    target_repo: Option<String>,
}

fn execution_target_from_dir(dir: &str) -> Option<DispatchExecutionTarget> {
    if !std::path::Path::new(dir).is_dir() {
        return None;
    }
    let reviewed_commit = crate::services::platform::git_head_commit(dir)?;
    let checked_out_branch = crate::services::platform::shell::git_branch_name(dir);
    let branch = crate::services::platform::shell::git_branch_containing_commit(
        dir,
        &reviewed_commit,
        checked_out_branch.as_deref(),
        None,
    )
    .or(checked_out_branch);
    Some(DispatchExecutionTarget {
        reviewed_commit,
        branch,
        worktree_path: Some(dir.to_string()),
        target_repo: None,
    })
}

pub(super) fn json_string_field<'a>(value: &'a serde_json::Value, key: &str) -> Option<&'a str> {
    value
        .get(key)
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
}

pub(crate) fn sandbox_preflight_metadata_disables_external_side_effects(
    metadata: Option<&serde_json::Value>,
) -> bool {
    metadata.is_some_and(|metadata| {
        metadata
            .get("sandbox_preflight")
            .and_then(serde_json::Value::as_bool)
            == Some(true)
            && metadata
                .get("production_mutation_allowed")
                .and_then(serde_json::Value::as_bool)
                == Some(false)
    })
}

pub(crate) async fn sandbox_preflight_card_disables_external_side_effects(
    pool: &PgPool,
    card_id: &str,
) -> bool {
    let metadata = sqlx::query_scalar::<_, Option<serde_json::Value>>(
        "SELECT metadata
         FROM kanban_cards
         WHERE id = $1",
    )
    .bind(card_id)
    .fetch_optional(pool)
    .await;
    let Ok(Some(Some(metadata))) = metadata else {
        return false;
    };
    sandbox_preflight_metadata_disables_external_side_effects(Some(&metadata))
}

pub(super) fn json_map_string_field<'a>(
    map: &'a serde_json::Map<String, serde_json::Value>,
    key: &str,
) -> Option<&'a str> {
    map.get(key)
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ReviewCounterModelProviderResolution {
    pub source_provider: ProviderKind,
    pub target_provider: ProviderKind,
    pub reason: String,
}

fn review_context_provider(
    context: &serde_json::Map<String, serde_json::Value>,
    key: &str,
) -> Option<ProviderKind> {
    json_map_string_field(context, key).and_then(ProviderKind::from_str)
}

fn review_main_provider(bindings: &AgentChannelBindings) -> Option<(ProviderKind, &'static str)> {
    bindings
        .provider
        .as_deref()
        .and_then(ProviderKind::from_str)
        .map(|provider| (provider, "agent_main_provider"))
        .or_else(|| {
            bindings
                .resolved_primary_provider_kind()
                .map(|provider| (provider, "agent_current_provider"))
        })
        .or_else(|| {
            bindings
                .primary_channel()
                .as_deref()
                .and_then(provider_from_channel_suffix)
                .and_then(ProviderKind::from_str)
                .map(|provider| (provider, "agent_primary_channel_suffix"))
        })
}

pub(crate) fn resolve_review_counter_model_provider(
    bindings: &AgentChannelBindings,
    context: &serde_json::Map<String, serde_json::Value>,
) -> Option<ReviewCounterModelProviderResolution> {
    let (source_provider, source_reason) =
        if let Some(provider) = review_context_provider(context, "implementer_provider") {
            (provider, "explicit_implementer_provider")
        } else if let Some(provider) = review_context_provider(context, "from_provider") {
            (provider, "explicit_from_provider")
        } else {
            review_main_provider(bindings)?
        };

    let target_provider = source_provider.counterpart();
    let reason = format!(
        "{}:{}=>{}",
        source_reason,
        source_provider.as_str(),
        target_provider.as_str()
    );

    Some(ReviewCounterModelProviderResolution {
        source_provider,
        target_provider,
        reason,
    })
}

fn apply_review_counter_model_provider_context(
    obj: &mut serde_json::Map<String, serde_json::Value>,
    bindings: &AgentChannelBindings,
) {
    let Some(resolution) = resolve_review_counter_model_provider(bindings, obj) else {
        return;
    };

    obj.insert(
        "from_provider".to_string(),
        json!(resolution.source_provider.as_str()),
    );
    obj.insert(
        "target_provider".to_string(),
        json!(resolution.target_provider.as_str()),
    );
    obj.insert(
        "counter_model_resolution_reason".to_string(),
        json!(resolution.reason),
    );
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct DispatchSessionStrategy {
    pub reset_provider_state: bool,
    pub recreate_tmux: bool,
}

/// #762: Provenance of the `target_repo` field in a review dispatch context.
///
/// `build_review_context` needs to know whether the caller *actually* pinned
/// a `target_repo` on this invocation, or whether the field was auto-injected
/// by the dispatch create path from the card's canonical scope. When the
/// external `target_repo` becomes unrecoverable, card-scoped fallbacks
/// silently redirect the reviewer to unrelated code UNLESS we can distinguish
/// "caller said so" (safe to fallback) from "we made it up" (must fail closed).
///
/// Prior behavior inferred this from the (possibly mutated) context passed in,
/// which broke the moment any upstream injected `target_repo` before calling
/// `build_review_context` (see `dispatch_create.rs`). Make the signal explicit
/// instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TargetRepoSource {
    /// Caller (e.g. REST API client) pinned `target_repo` explicitly on this
    /// dispatch request. Card-scoped fallbacks may honor it.
    CallerSupplied,
    /// `target_repo` was either absent from the caller context OR was
    /// auto-injected by the dispatch create path from `card.repo_id`.
    /// Treat as card-scoped default — fail closed on unrecoverable externals.
    CardScopeDefault,
}

pub(crate) fn dispatch_type_session_strategy_default(
    dispatch_type: Option<&str>,
) -> Option<DispatchSessionStrategy> {
    match dispatch_type {
        Some("implementation") | Some("review") | Some("rework") => Some(DispatchSessionStrategy {
            reset_provider_state: true,
            recreate_tmux: false,
        }),
        Some("review-decision") => Some(DispatchSessionStrategy::default()),
        _ => None,
    }
}

pub(crate) fn dispatch_type_force_new_session_default(dispatch_type: Option<&str>) -> Option<bool> {
    dispatch_type_session_strategy_default(dispatch_type)
        .map(|strategy| strategy.reset_provider_state)
}

pub(crate) fn dispatch_type_requires_fresh_worktree(dispatch_type: Option<&str>) -> bool {
    matches!(dispatch_type, Some("implementation" | "rework"))
}

pub(crate) fn dispatch_type_uses_thread_routing(_dispatch_type: Option<&str>) -> bool {
    true
}

/// Resolve the per-dispatch session strategy from the incoming context.
///
/// ## `force_new_session` semantics — read this first (#800)
///
/// Despite its suggestive name, the `force_new_session` flag (and its alias
/// `reset_provider_state`) only controls **provider-side session state** — i.e.
/// whether the underlying agent CLI (Claude / Codex) should start a fresh
/// conversation versus reusing the previous transcript. It is purely an input
/// to [`DispatchSessionStrategy::reset_provider_state`].
///
/// **What `force_new_session` does NOT do:**
/// - It does not delete or recreate the worktree directory.
/// - It does not clear `worktree_path` / `worktree_branch` from the new
///   dispatch context.
/// - It does not interact with the worktree-reuse path. That logic lives in
///   [`latest_completed_work_dispatch_target`], which now (per #800) validates
///   that any recorded `worktree_path` still exists on disk before re-injecting
///   it. Stale/missing paths are dropped automatically and the downstream
///   fallback chain (`resolve_card_worktree`, `resolve_card_issue_commit_target`,
///   repo-HEAD recovery) resolves a fresh execution target.
///
/// If you want a "blow away the worktree state" reset, that lives in the
/// `reset_full=true` branch of `POST /api/kanban-cards/:id/reopen`, which
/// invokes `cleanup_force_transition_revert_fields_on_conn` to scrub the
/// recorded worktree metadata from `task_dispatches` JSON columns. See
/// `src/kanban/transition_cleanup.rs:strip_stale_worktree_metadata_from_dispatches_on_conn`.
pub(crate) fn dispatch_session_strategy_from_context(
    context: Option<&serde_json::Value>,
    dispatch_type: Option<&str>,
) -> DispatchSessionStrategy {
    let default = dispatch_type_session_strategy_default(dispatch_type).unwrap_or_default();
    let reset_provider_state = context
        .and_then(|value| value.get("reset_provider_state"))
        .and_then(|value| value.as_bool())
        .or_else(|| {
            context
                .and_then(|value| value.get("force_new_session"))
                .and_then(|value| value.as_bool())
        })
        .unwrap_or(default.reset_provider_state);
    let recreate_tmux = context
        .and_then(|value| value.get("recreate_tmux"))
        .and_then(|value| value.as_bool())
        .unwrap_or(default.recreate_tmux);

    DispatchSessionStrategy {
        reset_provider_state,
        recreate_tmux,
    }
}

pub(super) fn dispatch_context_with_session_strategy(
    dispatch_type: &str,
    context: &serde_json::Value,
) -> serde_json::Value {
    let Some(_) = dispatch_type_session_strategy_default(Some(dispatch_type)) else {
        return context.clone();
    };

    let mut context = if context.is_object() {
        context.clone()
    } else {
        json!({})
    };

    let strategy = dispatch_session_strategy_from_context(Some(&context), Some(dispatch_type));
    if let Some(obj) = context.as_object_mut() {
        obj.insert(
            "reset_provider_state".to_string(),
            json!(strategy.reset_provider_state),
        );
        obj.insert("recreate_tmux".to_string(), json!(strategy.recreate_tmux));
        obj.insert(
            "force_new_session".to_string(),
            json!(strategy.reset_provider_state),
        );
    }

    context
}

pub(super) fn dispatch_context_worktree_target(
    context: &serde_json::Value,
) -> Result<Option<(String, Option<String>)>> {
    let Some(path) = json_string_field(context, "worktree_path") else {
        return Ok(None);
    };
    if !std::path::Path::new(path).is_dir() {
        tracing::warn!(
            "[dispatch] Ignoring explicit worktree_path '{}' because the path does not exist or is not a directory; falling back to canonical worktree resolution",
            path
        );
        return Ok(None);
    }

    let branch = json_string_field(context, "worktree_branch")
        .or_else(|| json_string_field(context, "branch"))
        .map(str::to_string)
        .or_else(|| crate::services::platform::shell::git_branch_name(path));

    Ok(Some((path.to_string(), branch)))
}

fn is_card_scoped_worktree_path(path: &str, branch: Option<&str>) -> bool {
    let resolved_branch = branch
        .map(str::to_string)
        .or_else(|| crate::services::platform::shell::git_branch_name(path));
    let repo_root = crate::services::platform::resolve_repo_dir();
    let is_repo_root = repo_root.as_deref() == Some(path);
    let is_non_main_branch = resolved_branch
        .as_deref()
        .map(|value| value != "main" && value != "master")
        .unwrap_or(false);
    !is_repo_root || is_non_main_branch
}

/// Words that immediately precede a `#N` reference in a *back-reference*
/// context — i.e. the commit subject is pointing AT issue N rather than
/// claiming ownership of it. Matched case-insensitively against the token
/// immediately before `#N`.
///
/// Important: we deliberately do NOT include GitHub "closing" verbs such as
/// `fixes`, `closes`, `resolves`, `fix`, `close`, `resolve` — those *are*
/// ownership claims (they cause GitHub to close the issue on merge), so a
/// subject like `Fix #523 in path/to/file` must continue to validate. The
/// list below is restricted to verbs/abbreviations that exclusively signal
/// a back-reference relationship.
const BACK_REFERENCE_VERBS: &[&str] = &[
    "refs",
    "ref",
    "reverts",
    "revert",
    "re",
    "see",
    "cf",
    "related",
    "relates",
    "cross-ref",
    "cross-refs",
    "follow-up",
    "followup",
];

/// Extract the lowercase word token preceding byte offset `end_pos` in
/// `subject`, skipping any intervening whitespace and ASCII punctuation.
/// Returns `None` only when the preceding context contains no word
/// characters at all (i.e. the reference is effectively the first
/// meaningful token in the subject).
///
/// Why skip punctuation: real commits use forms like `cf. #523`,
/// `refs: #523`, and `see (#523)` where the back-reference verb is
/// separated from the reference by punctuation. Treating punctuation as a
/// hard boundary would silently re-admit those subjects as ownership
/// claims — exactly the false-positive Codex flagged in round-4 review of
/// #2372.
fn word_token_before(subject: &str, end_pos: usize) -> Option<String> {
    let bytes = subject.as_bytes();
    // Walk backwards over non-word characters (whitespace + ASCII
    // punctuation, including `(`, `[`, `:`, `.`, `,`, `;`, `—`, etc.).
    let mut end = end_pos;
    while end > 0 {
        let ch = bytes[end - 1];
        if ch.is_ascii_alphanumeric() || ch == b'-' || ch == b'_' {
            break;
        }
        end -= 1;
    }
    if end == 0 {
        return None;
    }
    // Walk backwards over word characters (alnum, `-`, `_`).
    let mut start = end;
    while start > 0 {
        let ch = bytes[start - 1];
        if ch.is_ascii_alphanumeric() || ch == b'-' || ch == b'_' {
            start -= 1;
        } else {
            break;
        }
    }
    if start == end {
        return None;
    }
    Some(subject[start..end].to_ascii_lowercase())
}

/// Returns `true` when the word token preceding `pos` (skipping whitespace
/// and punctuation) is a known back-reference verb. Used both for raw
/// `#N` occurrences and for parenthesised forms — e.g. `see (#N)` must be
/// rejected because the verb sits before the opening `(`.
fn preceded_by_back_reference_verb(subject: &str, pos: usize) -> bool {
    match word_token_before(subject, pos) {
        Some(prev) => BACK_REFERENCE_VERBS.iter().any(|v| *v == prev),
        None => false,
    }
}

/// Check whether a commit message references the given card's GitHub issue number.
///
/// Used to cross-validate dispatch-history commits so a poisoned `reviewed_commit`
/// from an unrelated issue cannot propagate through review→rework cycles (#269).
///
/// Returns `false` (reject → fallback) when verification is impossible (repo dir
/// missing, git unreachable, commit not locally available). This fail-closed
/// design ensures the fallback chain always reaches `resolve_card_worktree()` or
/// `resolve_card_issue_commit_target()` when the dispatch-history commit can't
/// be confirmed as belonging to this issue.
///
/// Accepted forms (the patterns AgentDesk itself generates / observes — locked
/// in by `commit_subject_references_issue_*` tests):
///   - `#N <subject>`              — leading hash reference (project convention)
///   - `[#N] <subject>` / `(#N) <subject>` — bracketed/parenthesised leading reference
///   - `(#N)` at the end of subject — squash-merge suffix (e.g. `feat: bar (#N)`)
///   - Single-reference subjects where the only `#N` in the subject is NOT
///     preceded by a back-reference verb. Covers the natural history-style
///     `Harden auto-queue phase gate repair #2211` (description ends with
///     `#N`) and `Fix #523 in path/to/file` (leading closing verb).
///
/// Rejected — multi-reference subjects where N is not in a canonical position,
/// and single-reference subjects where the `#N` token is immediately preceded
/// by a back-reference verb (`refs`, `reverts`, `see`, `cf`, …) — see
/// `BACK_REFERENCE_VERBS`. Issue #2372 (Codex follow-up to #2200 sub-fix 2):
/// the previous variant allowed `refs #N` / `reverts #N` to pass because the
/// stricter canonical-position check only ran when a competing reference
/// existed; this version distinguishes ownership descriptions from explicit
/// back-references regardless of how many `#N` tokens are present.
fn commit_subject_references_issue(subject: &str, issue_number: i64) -> bool {
    let needle = format!("#{issue_number}");
    // Collect all delimiter-bounded `#<digits>` references in the subject
    // along with the byte offsets of every match of our needle so we can
    // inspect the immediately-preceding token for back-reference verbs.
    let mut all_refs: Vec<&str> = Vec::new();
    let mut needle_positions: Vec<usize> = Vec::new();
    let bytes = subject.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'#' {
            let before_ok = if i == 0 {
                true
            } else {
                let prev = bytes[i - 1];
                !prev.is_ascii_alphanumeric() && prev != b'_'
            };
            let digits_start = i + 1;
            let mut j = digits_start;
            while j < bytes.len() && bytes[j].is_ascii_digit() {
                j += 1;
            }
            if before_ok && j > digits_start {
                let after_ok = if j == bytes.len() {
                    true
                } else {
                    let next = bytes[j];
                    !next.is_ascii_alphanumeric() && next != b'_'
                };
                if after_ok {
                    let token = &subject[i..j];
                    all_refs.push(token);
                    if token == needle {
                        needle_positions.push(i);
                    }
                }
            }
            i = j.max(i + 1);
        } else {
            i += 1;
        }
    }
    if needle_positions.is_empty() {
        return false;
    }

    let competing_ref = all_refs.iter().any(|r| *r != needle);
    let trimmed = subject.trim();
    let canonical_squash = format!("({})", needle);

    // Canonical leading reference: trimmed subject starts with `#N`
    // directly followed by a non-word character (or end-of-string).
    let canonical_leading = if let Some(rest) = trimmed.strip_prefix(&needle) {
        rest.chars()
            .next()
            .map(|ch| !ch.is_ascii_alphanumeric() && ch != '_')
            .unwrap_or(true)
    } else {
        false
    };
    // Canonical bracketed leading reference: `[#N]` or `(#N)` at the start
    // of the trimmed subject. We require the immediately preceding context
    // (in the *full* subject, not trimmed) to not be a back-reference verb
    // — otherwise `see (#523)` and similar would slip through this branch.
    let leading_bracket_pos = if trimmed.starts_with(&format!("[{}]", needle)) {
        subject.find(&format!("[{}]", needle))
    } else if trimmed.starts_with(&canonical_squash) {
        subject.find(&canonical_squash)
    } else {
        None
    };
    let canonical_brackets = match leading_bracket_pos {
        Some(pos) => !preceded_by_back_reference_verb(subject, pos),
        None => false,
    };
    // Canonical trailing-squash reference: subject ends with `(#N)`. This
    // is the GitHub squash-merge form — but ONLY when the preceding word
    // is not a back-reference verb. `see (#N)` / `cf (#N)` / `refs (#N)`
    // are back-references and must be rejected (Codex round-4 finding).
    let canonical_trailing_squash = if trimmed.ends_with(&canonical_squash) {
        let suffix_pos = subject.rfind(&canonical_squash).unwrap_or(0);
        !preceded_by_back_reference_verb(subject, suffix_pos)
    } else {
        false
    };
    if canonical_leading || canonical_brackets || canonical_trailing_squash {
        return true;
    }

    // Non-canonical position with competing references → reject as ambiguous.
    if competing_ref {
        return false;
    }

    // Single-reference, non-canonical position: accept iff the word token
    // preceding `#N` (skipping whitespace + punctuation) is not a known
    // back-reference verb. Covers `cf. #N` / `refs: #N` / `see — #N` and
    // similar punctuated forms (Codex round-4 finding for #2372).
    for pos in &needle_positions {
        if preceded_by_back_reference_verb(subject, *pos) {
            continue;
        }
        return true;
    }
    false
}

/// #2341 / #2200 sub-3 (carried forward from PR #2336 HIGH 1): tri-state
/// scope verification. The bool helper above collapses three distinct
/// outcomes into a single `false`, which is safe for the in-scope dispute
/// path (where `false` means "fall back to a stricter check") but unsafe for
/// the out-of-scope close path — there, `false` was being treated as "proven
/// out-of-scope, allow close". The `Unknown` arm covers transient repo/git
/// failures, missing repo_dir, and unreachable commits. Out-of-scope close
/// must fail-closed on `Unknown`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScopeCheck {
    /// The commit's subject references this card's issue number, or no
    /// issue number is recorded for the card (historical default-true).
    InScope,
    /// The commit was successfully inspected and its subject does NOT
    /// reference this card's issue. The only outcome that affirmatively
    /// proves out-of-scope.
    OutOfScope,
    /// Scope verification could not complete (repo dir unavailable, git log
    /// failed, commit not reachable, etc.). Callers on the out-of-scope
    /// close path MUST treat this as a refusal.
    Unknown,
}

fn git_commit_exists(dir: &str, commit_sha: &str) -> bool {
    GitCommand::new()
        .repo(dir)
        .args(["cat-file", "-e", &format!("{commit_sha}^{{commit}}")])
        .run_output()
        .is_ok()
}

/// Codex round-4: determine whether `worktree_path` and `repo_dir` describe
/// the same underlying git repository (same object store / common dir).
///
/// Used to refuse `pr_tracking.worktree_path` values that point at a
/// completely different checkout, even when `pr_tracking.repo_id` matches
/// the card's scope. A foreign `worktree_path` recorded against the same
/// `repo_id` would otherwise be treated as a valid review surface.
fn worktree_path_belongs_to_repo(worktree_path: &str, repo_dir: &str) -> bool {
    if worktree_path.is_empty() || repo_dir.is_empty() {
        return false;
    }
    let common_dir = |dir: &str| -> Option<std::path::PathBuf> {
        let output = GitCommand::new()
            .repo(dir)
            .args(["rev-parse", "--git-common-dir"])
            .run_output()
            .ok()?;
        let raw = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if raw.is_empty() {
            return None;
        }
        let pb = std::path::PathBuf::from(&raw);
        let absolute = if pb.is_absolute() {
            pb
        } else {
            std::path::PathBuf::from(dir).join(pb)
        };
        // Fail-closed on canonicalize errors: a partial-FS glitch (EACCES on
        // a parent, NFS hiccup) that canonicalizes one side but not the
        // other would otherwise compare raw vs. resolved paths and produce
        // a silent false-negative. Returning None here forces the caller's
        // fail-closed branch and surfaces the failure in logs.
        match std::fs::canonicalize(&absolute) {
            Ok(canon) => Some(canon),
            Err(err) => {
                tracing::warn!(
                    target: "dispatch_context",
                    path = %absolute.display(),
                    error = %err,
                    "worktree_path_belongs_to_repo: canonicalize failed; treating as foreign"
                );
                None
            }
        }
    };
    match (common_dir(worktree_path), common_dir(repo_dir)) {
        (Some(a), Some(b)) => a == b,
        // If either side cannot answer, fail-closed.
        _ => false,
    }
}

/// #682: Exact-HEAD check — returns true only when the worktree's current
/// HEAD resolves to `commit_sha`. Git's object store is shared across
/// worktrees of the same repo, so `git cat-file -e` (git_commit_exists)
/// is satisfied by any commit anywhere in the repo; `merge-base --is-ancestor`
/// additionally accepts any descendant HEAD, which means a path that was
/// recycled for follow-up work still passes — but the filesystem state the
/// reviewer sees is the descendant, not the reviewed commit. Exact HEAD
/// match is the only way to guarantee the on-disk state matches the
/// reviewed commit.
// reason: worktree HEAD validation helper for the review-target resolver cluster;
// callers are cfg/test-gated in the default lib build. See #3034.
#[allow(dead_code)]
fn worktree_head_matches_commit(dir: &str, commit_sha: &str) -> bool {
    let Ok(output) = GitCommand::new()
        .repo(dir)
        .args(["rev-parse", "HEAD"])
        .run_output()
    else {
        return false;
    };
    let head = String::from_utf8_lossy(&output.stdout).trim().to_string();
    head == commit_sha
}

/// #2237 item 3: `git_tracked_change_paths*` shells out to `git status` and
/// can block for a meaningful amount of time on a large monorepo worktree.
/// Every async review-target resolver path that calls it must move the
/// invocation onto a blocking thread so the tokio worker that drives every
/// other dispatch task is not stalled.
///
/// Returns an empty `Vec` both when the worktree is genuinely clean and
/// when the underlying git invocation or the join handle fails. The
/// failure case is logged at warn level for observability so it does not
/// silently look like "clean worktree" without any trace.
// reason: async clean-worktree probe for the review-target resolver paths;
// invoked from cfg/test-gated resolver flows in the default lib build. See #3034.
#[allow(dead_code)]
async fn dirty_tracked_change_paths_async(path: &str) -> Vec<String> {
    let path_owned = path.to_string();
    let join_result = tokio::task::spawn_blocking(move || {
        crate::services::platform::shell::git_tracked_change_paths(&path_owned)
    })
    .await;
    match join_result {
        // `git_tracked_change_paths` returns `Some(empty)` for a clean
        // worktree and `None` when the git invocation itself fails — both
        // are mapped to "no dirty paths visible" here, matching the
        // pre-change `.unwrap_or_default()` semantics.
        Ok(maybe_paths) => maybe_paths.unwrap_or_default(),
        Err(join_err) => {
            tracing::warn!(
                "[dispatch] spawn_blocking join failed for git_tracked_change_paths '{}': {}. \
                 Treating as clean; runtime may be shutting down.",
                path,
                join_err
            );
            Vec::new()
        }
    }
}

/// Outcome of a single clean-worktree probe. Lets callers (e.g. the
/// pr_tracking resolver) distinguish a transient dirty state — worth a short
/// retry per #2254 item 4 — from a genuinely contaminated or git-broken
/// worktree, which must fail-closed immediately.
#[derive(Debug, Clone)]
enum ReviewWorktreeProbe {
    /// HEAD matches the reviewed commit and the worktree has no tracked
    /// changes. The owned `String` is the path to surface to the reviewer.
    Clean(String),
    /// HEAD does not match the reviewed commit, or the path is not a
    /// directory. Retrying will not change this — bail to the next fallback.
    NotMatching,
    /// HEAD matches but tracked changes exist. The caller may retry once
    /// after a short backoff (transient writer settling).
    DirtyTransient(Vec<String>),
    /// `git status` itself failed (index lock, permission denied, corrupt
    /// repo). Fail-closed at the caller — pre-#2254 this was silently
    /// treated as "clean" via `unwrap_or_default()`.
    GitFailure(String),
}

fn dirty_paths_sample(dirty_paths: &[String]) -> String {
    let sample = dirty_paths
        .iter()
        .take(3)
        .cloned()
        .collect::<Vec<_>>()
        .join(", ");
    if sample.is_empty() {
        String::new()
    } else if dirty_paths.len() > 3 {
        format!(" ({sample}, +{} more)", dirty_paths.len() - 3)
    } else {
        format!(" ({sample})")
    }
}

/// Blocking half of the review worktree probe.
///
/// #2237 follow-up: async review-target resolution must not run git HEAD or
/// status checks on the tokio worker. Keep the exact-HEAD validation and the
/// strict tracked-status check in one `spawn_blocking` unit so callers never
/// observe a clean status for a different HEAD than the reviewed commit.
fn probe_clean_exact_review_worktree_blocking(path: String, commit: String) -> ReviewWorktreeProbe {
    if !std::path::Path::new(&path).is_dir() {
        return ReviewWorktreeProbe::NotMatching;
    }

    let Ok(output) = GitCommand::new()
        .repo(&path)
        .args(["rev-parse", "HEAD"])
        .run_output()
    else {
        return ReviewWorktreeProbe::NotMatching;
    };
    let head = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if head != commit {
        return ReviewWorktreeProbe::NotMatching;
    }

    match crate::services::platform::shell::git_tracked_change_paths_strict(&path) {
        Ok(dirty_paths) if dirty_paths.is_empty() => ReviewWorktreeProbe::Clean(path),
        Ok(dirty_paths) => ReviewWorktreeProbe::DirtyTransient(dirty_paths),
        Err(err) => ReviewWorktreeProbe::GitFailure(err),
    }
}

async fn probe_clean_exact_review_worktree(
    card_id: &str,
    source: &str,
    path: &str,
    commit: &str,
) -> ReviewWorktreeProbe {
    let path_owned = path.to_string();
    let commit_owned = commit.to_string();
    let probe = match tokio::task::spawn_blocking(move || {
        probe_clean_exact_review_worktree_blocking(path_owned, commit_owned)
    })
    .await
    {
        Ok(probe) => probe,
        Err(join_err) => {
            ReviewWorktreeProbe::GitFailure(format!("spawn_blocking join failed: {join_err}"))
        }
    };

    match &probe {
        ReviewWorktreeProbe::DirtyTransient(dirty_paths) => {
            tracing::warn!(
                "[dispatch] Review dispatch for card {}: {} worktree_path '{}' for commit {} has tracked changes{}",
                card_id,
                source,
                path,
                &commit[..8.min(commit.len())],
                dirty_paths_sample(dirty_paths)
            );
        }
        ReviewWorktreeProbe::GitFailure(err) => {
            // #2254 bonus: previously `unwrap_or_default()` silently treated
            // an index-lock or permission error as a clean worktree, which
            // bypassed the dirty-state guard. Fail-closed instead.
            tracing::warn!(
                "[dispatch] Review dispatch for card {}: git status failed for {} worktree '{}' (commit {}) — refusing to emit worktree_path: {}",
                card_id,
                source,
                path,
                &commit[..8.min(commit.len())],
                err
            );
        }
        ReviewWorktreeProbe::Clean(_) | ReviewWorktreeProbe::NotMatching => {}
    }

    probe
}

// reason: review-worktree cleanliness resolver; callers are cfg/test-gated in
// the default lib build. See #3034.
#[allow(dead_code)]
async fn clean_exact_review_worktree_path(
    card_id: &str,
    source: &str,
    path: &str,
    commit: &str,
) -> Option<String> {
    match probe_clean_exact_review_worktree(card_id, source, path, commit).await {
        ReviewWorktreeProbe::Clean(p) => Some(p),
        // #2254 item 4 / bonus: dirty (transient) and git-failure both
        // fail-closed at this single-shot helper. Callers that want the
        // retry-once-on-transient-dirty behavior use
        // `probe_clean_exact_review_worktree` directly.
        ReviewWorktreeProbe::NotMatching
        | ReviewWorktreeProbe::DirtyTransient(_)
        | ReviewWorktreeProbe::GitFailure(_) => None,
    }
}

// reason: work-dispatch classifier shared with the outbox route; lib-build
// callers are cfg/test-gated. See #3034.
#[allow(dead_code)]
fn is_work_dispatch_type(dispatch_type: Option<&str>) -> bool {
    matches!(dispatch_type, Some("implementation") | Some("rework"))
}

// reason: work-completion-evidence predicate for the dispatch-result gate;
// lib-build callers are cfg/test-gated. See #3034.
#[allow(dead_code)]
fn result_has_work_completion_evidence(result: &serde_json::Value) -> bool {
    json_string_field(result, "completed_commit").is_some()
        || json_string_field(result, "assistant_message").is_some()
        || result
            .get("agent_response_present")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
        || json_string_field(result, "work_outcome").is_some()
}

fn apply_review_target_context(
    target: &DispatchExecutionTarget,
    obj: &mut serde_json::Map<String, serde_json::Value>,
) {
    obj.insert(
        "reviewed_commit".to_string(),
        json!(target.reviewed_commit.clone()),
    );
    // #2237 item 2: when the resolver returns None for branch/worktree_path/
    // target_repo, symmetrically remove any stale value from the persisted
    // context. Previously only `worktree_path` was cleared, allowing a prior
    // failed rereview's `branch` / `worktree_branch` (alias used by prompt
    // builder, merge-base injection, and review-target fallback at line
    // ~1245) or `target_repo` to leak into the new dispatch context.
    if let Some(branch) = target.branch.as_deref() {
        obj.insert("branch".to_string(), json!(branch));
    } else {
        obj.remove("branch");
        // `worktree_branch` is treated as a branch synonym by downstream
        // review-target consumers, so we must clear it whenever `branch`
        // is cleared — otherwise a stale `worktree_branch` would still
        // feed into prompt construction.
        obj.remove("worktree_branch");
    }
    if let Some(path) = target.worktree_path.as_deref() {
        obj.insert("worktree_path".to_string(), json!(path));
    } else {
        obj.remove("worktree_path");
    }
    if let Some(target_repo) = target.target_repo.as_deref() {
        obj.insert("target_repo".to_string(), json!(target_repo));
    } else {
        obj.remove("target_repo");
    }
}

fn apply_review_target_warning(
    obj: &mut serde_json::Map<String, serde_json::Value>,
    reason: &str,
    warning: &str,
) {
    obj.insert("review_target_reject_reason".to_string(), json!(reason));
    obj.insert("review_target_warning".to_string(), json!(warning));
}

/// #762: Normalize a `target_repo` value for comparison.
///
/// Two repo references describe the same local repo iff their
/// `resolve_repo_dir_for_target` results canonicalize to the same path. This
/// handles mixed "org/name" / "/abs/path" / "~/path" forms without tripping on
/// trivial string differences.
fn normalized_target_repo_path(target_repo: Option<&str>) -> Option<std::path::PathBuf> {
    let target_repo = target_repo
        .map(str::trim)
        .filter(|value| !value.is_empty())?;
    let resolved = crate::services::platform::shell::resolve_repo_dir_for_target(Some(target_repo))
        .ok()
        .flatten()?;
    Some(std::fs::canonicalize(&resolved).unwrap_or_else(|_| std::path::PathBuf::from(&resolved)))
}

/// #762: Decide whether the historical work dispatch's `target_repo` risks
/// silently redirecting a review to unrelated code when card-scoped
/// fallbacks run.
///
/// A recorded `work_target_repo` is safe iff it demonstrably resolves to the
/// same local repo as the card's canonical scope. Any other outcome —
/// different resolved path, unresolvable work_target_repo, or no card-side
/// anchor — is treated as "external and unrecoverable" to fail closed.
fn historical_target_repo_differs_from_card(
    work_target_repo: Option<&str>,
    card_scope_repo: Option<&str>,
) -> bool {
    let Some(work) = work_target_repo
        .map(str::trim)
        .filter(|value| !value.is_empty())
    else {
        return false;
    };
    let card = card_scope_repo
        .map(str::trim)
        .filter(|value| !value.is_empty());

    // Cheap string compare first — avoids touching the filesystem on the
    // common case where the two references were copied from the same source.
    if let Some(card_str) = card.as_deref() {
        if work == card_str {
            return false;
        }
    }

    let work_path = normalized_target_repo_path(Some(work));
    let card_path = card.and_then(|value| normalized_target_repo_path(Some(value)));
    match (work_path, card_path) {
        (Some(w), Some(c)) => w != c,
        // If only one side resolves, we cannot prove the two references
        // describe the same repo — treat as external-divergent so the
        // card-scoped fallback path does not silently redirect.
        (Some(_), None) => true,
        (None, Some(_)) => true,
        // #762 (C): when NEITHER side resolves we still have a concrete
        // `work_target_repo` string recorded against the historical work
        // dispatch. We cannot prove it matches the card scope — in fact the
        // card scope is unresolvable too. Previously this returned `false`
        // and let the card-scoped fallback chain run, which made
        // `resolve_repo_dir_for_target(None)` redirect the reviewer into the
        // default repo. Treat this as divergent so the caller fails closed
        // on an unrecoverable external target_repo instead of silently
        // reviewing unrelated code in the default repo.
        (None, None) => true,
    }
}

pub(crate) const REVIEW_QUALITY_SCOPE_REMINDER: &str =
    "기존 DoD/기능 검증과 함께 아래 품질 항목도 반드시 확인하세요.";
pub(crate) const REVIEW_VERDICT_IMPROVE_GUIDANCE: &str = "기능이 맞더라도 아래 품질 항목에서 실제 문제가 하나라도 보이면 `VERDICT: improve`로 판정하세요.";
pub(crate) const REVIEW_QUALITY_CHECKLIST: [&str; 5] = [
    "race condition / 동시성 이슈: 공유 상태 경쟁, TOCTOU, 중복 처리, 순서 역전",
    "에러 핸들링 누락: unwrap/panic, 빈 catch, 실패·timeout·retry 누락",
    "edge case: null/빈 배열, 타임아웃, 네트워크 실패, 재시도 후 중복 상태",
    "리소스 정리 누락: drop, cleanup, stash/worktree/session restore 정리 여부",
    "기존 코드와의 경로 충돌: 같은 상태를 여러 곳에서 수정하거나 기존 자동화와 상충",
];

fn inject_review_quality_context(obj: &mut serde_json::Map<String, serde_json::Value>) {
    obj.entry("review_quality_scope_reminder".to_string())
        .or_insert_with(|| json!(REVIEW_QUALITY_SCOPE_REMINDER));
    obj.entry("review_verdict_guidance".to_string())
        .or_insert_with(|| json!(REVIEW_VERDICT_IMPROVE_GUIDANCE));
    obj.entry("review_quality_checklist".to_string())
        .or_insert_with(|| json!(REVIEW_QUALITY_CHECKLIST));
}

pub(super) fn inject_review_merge_base_context(
    obj: &mut serde_json::Map<String, serde_json::Value>,
) {
    if obj.contains_key("merge_base") {
        return;
    }

    let path = obj
        .get("worktree_path")
        .and_then(|value| value.as_str())
        .filter(|value| !value.is_empty())
        .map(str::to_string);
    let branch = obj
        .get("branch")
        .and_then(|value| value.as_str())
        .or_else(|| obj.get("worktree_branch").and_then(|value| value.as_str()))
        .filter(|value| !value.is_empty())
        .map(str::to_string);
    let reviewed_commit = obj
        .get("reviewed_commit")
        .and_then(|value| value.as_str())
        .filter(|value| !value.is_empty())
        .map(str::to_string);
    let (Some(path), Some(branch), Some(reviewed_commit)) = (path, branch, reviewed_commit) else {
        return;
    };

    if crate::services::platform::shell::is_mainlike_branch(&branch) {
        tracing::warn!(
            "[dispatch] skipping review merge-base injection for branch '{}' at commit {}: main-like branch would produce an empty diff range",
            branch,
            &reviewed_commit[..8.min(reviewed_commit.len())]
        );
        return;
    }

    let Some(merge_base) = crate::services::platform::shell::git_merge_base(&path, "main", &branch)
    else {
        tracing::warn!(
            "[dispatch] skipping review merge-base injection for branch '{}' at commit {}: git merge-base returned no result",
            branch,
            &reviewed_commit[..8.min(reviewed_commit.len())]
        );
        return;
    };

    if merge_base == reviewed_commit {
        tracing::warn!(
            "[dispatch] skipping review merge-base injection for branch '{}' at commit {}: merge-base resolved to the reviewed commit",
            branch,
            &reviewed_commit[..8.min(reviewed_commit.len())]
        );
        return;
    }
    obj.insert("merge_base".to_string(), json!(merge_base));
}

/// #2254 item 1 — SQLite codepath mirror decision.
///
/// `resolve_pr_tracking_review_target_pg` (PG) is the sole production
/// rereview-target resolver. Historical SQLite-shaped helper names from the
/// removed test harness are not a supported runtime fallback.

/// Review-target fields that steer the agent's execution state (which commit
/// to check out, which worktree to inspect, which branch to compare against).
///
/// #761: These fields must be treated as untrusted when they arrive via the
/// public dispatch-create API. A caller could craft a context that pins the
/// review to any commit/path. `build_review_context` with
/// `ReviewTargetTrust::Untrusted` strips them before running the
/// validation/refresh chain. The trust signal is passed **out-of-band** as a
/// function parameter — never read from the JSON context — so no
/// client-controlled field can opt out of stripping.
pub(super) const UNTRUSTED_REVIEW_TARGET_FIELDS: &[&str] =
    &["reviewed_commit", "worktree_path", "branch", "target_repo"];

/// Trust boundary for review-target fields on the incoming context.
///
/// #761 (Codex round-2): The round-1 design used a `_trusted_review_target`
/// sentinel inside the context JSON. That made trust client-controlled — a
/// crafted POST /api/dispatches body could set it and bypass stripping. This
/// enum is the replacement: it is an out-of-band Rust-type parameter, not a
/// JSON field. API-sourced code paths (`POST /api/dispatches` → dispatch
/// service → `create_dispatch_core_internal` → `build_review_context`) always
/// pass `Untrusted`. Only internal callers that legitimately pre-populate
/// review-target fields (e.g. tests simulating a specific target_repo
/// recovery path) may opt in via `Trusted`.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(crate) enum ReviewTargetTrust {
    /// Review-target fields in the incoming context are UNTRUSTED and will be
    /// stripped. The validation/refresh chain then resolves them fresh from
    /// the card's history (latest completed work dispatch → worktree lookup →
    /// issue commit recovery → repo HEAD fallback). This is the default for
    /// anything reachable via the public HTTP API.
    Untrusted,
    /// Review-target fields in the incoming context are TRUSTED and will be
    /// passed through to the downstream validation/refresh chain. Only use
    /// this from internal call sites where the fields came from a
    /// first-party source (not user-controlled JSON).
    #[allow(dead_code)]
    Trusted,
}

// ────────────────────────────────────────────────────────────────────────────
// #847 / #848 (Phase B+): PG-native (sqlx) variants of dispatch-context helpers.
//
// Additive only — the rusqlite originals above remain in use until #850 lands
// the final swap. `create_dispatch_core_internal` now prefers
// `build_review_context_pg` for review dispatches when a `PgPool` is present,
// but the rest of the rusqlite stack stays live until #850 / #843 complete the
// broader `Db`/caller cleanup.
//
// ## TargetRepoSource preservation (#762, #847)
//
// `resolve_card_target_repo_ref_pg` returns the same value the rusqlite
// variant would for any given `(card_id, context)` input. The
// `TargetRepoSource` provenance flag is computed independently in
// `create_dispatch_core_internal` from the *raw caller-supplied context*
// BEFORE either resolver runs (`dispatch_create.rs:236-240`), so the choice
// of backend never affects provenance. Tests below pin this invariant.
// ────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Default)]
struct CardDispatchInfoPg {
    issue_number: Option<i64>,
    repo_id: Option<String>,
}

fn warn_and_flatten_pg_optional<T>(
    result: Result<Option<T>, sqlx::Error>,
    card_id: &str,
    context: &'static str,
) -> Option<T> {
    match result {
        Ok(value) => value,
        Err(error) => {
            tracing::warn!(card_id = %card_id, "{context}: {error}");
            None
        }
    }
}

async fn load_card_dispatch_info_pg(pool: &PgPool, card_id: &str) -> Option<CardDispatchInfoPg> {
    // PG schema: kanban_cards.github_issue_number is BIGINT after migration
    // 0008. Decode natively as i64 for parity with the rusqlite signature.
    let row = warn_and_flatten_pg_optional(
        sqlx::query_as::<_, (Option<i64>, Option<String>)>(
            "SELECT github_issue_number, repo_id FROM kanban_cards WHERE id = $1",
        )
        .bind(card_id)
        .fetch_optional(pool)
        .await,
        card_id,
        "failed to load postgres card dispatch info",
    )?;
    Some(CardDispatchInfoPg {
        issue_number: row.0,
        repo_id: row.1,
    })
}

async fn load_card_issue_repo_pg(
    pool: &PgPool,
    card_id: &str,
) -> Option<(Option<i64>, Option<String>)> {
    load_card_dispatch_info_pg(pool, card_id)
        .await
        .map(|info| (info.issue_number, info.repo_id))
}

async fn load_card_pr_number_pg(pool: &PgPool, card_id: &str) -> Option<i64> {
    // PG schema: pr_tracking.pr_number is BIGINT after migration 0008.
    warn_and_flatten_pg_optional(
        sqlx::query_as::<_, (Option<i64>,)>("SELECT pr_number FROM pr_tracking WHERE card_id = $1")
            .bind(card_id)
            .fetch_optional(pool)
            .await,
        card_id,
        "failed to load postgres card pr number",
    )
    .and_then(|row| row.0)
}

/// PG-native variant of [`resolve_parent_dispatch_context`].
///
/// Returns `(parent_dispatch_id, chain_depth)` after validating that the
/// referenced parent dispatch exists and belongs to the same card.
#[allow(dead_code)] // Wired into create_dispatch_core_internal under #850.
pub(super) async fn resolve_parent_dispatch_context(
    pool: &PgPool,
    card_id: &str,
    context: &serde_json::Value,
) -> Result<(Option<String>, i64)> {
    let Some(parent_dispatch_id) =
        json_string_field(context, "parent_dispatch_id").filter(|value| !value.is_empty())
    else {
        return Ok((None, 0));
    };

    // PG schema: task_dispatches.chain_depth is BIGINT after migration 0008.
    let row = sqlx::query_as::<_, (Option<String>, Option<i64>)>(
        "SELECT kanban_card_id, COALESCE(chain_depth, 0)
         FROM task_dispatches
         WHERE id = $1",
    )
    .bind(parent_dispatch_id)
    .fetch_optional(pool)
    .await
    .map_err(|e| {
        anyhow::anyhow!(
            "Cannot create dispatch for card {}: lookup parent_dispatch_id '{}' failed: {}",
            card_id,
            parent_dispatch_id,
            e
        )
    })?;

    let Some((parent_card_id, parent_chain_depth)) = row else {
        anyhow::bail!(
            "Cannot create dispatch for card {}: parent_dispatch_id '{}' not found",
            card_id,
            parent_dispatch_id
        );
    };

    if parent_card_id.as_deref() != Some(card_id) {
        anyhow::bail!(
            "Cannot create dispatch for card {}: parent_dispatch_id '{}' belongs to a different card",
            card_id,
            parent_dispatch_id
        );
    }

    Ok((
        Some(parent_dispatch_id.to_string()),
        parent_chain_depth.unwrap_or(0) + 1,
    ))
}

/// PG-native variant of [`resolve_card_target_repo_ref`].
///
/// Returns the same value the rusqlite variant would for the same input.
/// **Do NOT compute provenance here** — see the module-level note above.
pub(super) async fn resolve_card_target_repo_ref(
    pool: &PgPool,
    card_id: &str,
    context: Option<&serde_json::Value>,
) -> Option<String> {
    if let Some(context) = context {
        if let Some(target_repo) = json_string_field(context, "target_repo") {
            return Some(target_repo.to_string());
        }
        if let Some(worktree_path) = json_string_field(context, "worktree_path") {
            if let Some(path) =
                crate::services::platform::shell::resolve_repo_dir_for_target(Some(worktree_path))
                    .ok()
                    .flatten()
            {
                return Some(path);
            }
        }
    }

    let info = load_card_dispatch_info_pg(pool, card_id).await?;
    info.repo_id
}

async fn resolve_card_repo_dir_with_context_pg(
    pool: &PgPool,
    card_id: &str,
    context: Option<&serde_json::Value>,
    purpose: &str,
) -> Result<Option<String>> {
    let target_repo = resolve_card_target_repo_ref(pool, card_id, context).await;
    crate::services::platform::shell::resolve_repo_dir_for_target(target_repo.as_deref())
        .map_err(|e| anyhow::anyhow!("Cannot {purpose} for card {}: {}", card_id, e))
}

/// PG-native variant of [`resolve_card_worktree`].
///
/// Returns `(worktree_path, worktree_branch, head_commit)` derived from the
/// card's `github_issue_number` + resolved repo dir.
pub(crate) async fn resolve_card_worktree(
    pool: &PgPool,
    card_id: &str,
    context: Option<&serde_json::Value>,
) -> Result<Option<(String, String, String)>> {
    let Some((issue_number, _repo_id)) = load_card_issue_repo_pg(pool, card_id).await else {
        return Ok(None);
    };
    let Some(issue_number) = issue_number else {
        return Ok(None);
    };
    let Some(repo_dir) =
        resolve_card_repo_dir_with_context_pg(pool, card_id, context, "resolve worktree repo")
            .await?
    else {
        return Ok(None);
    };
    Ok(
        crate::services::platform::find_worktree_for_issue(&repo_dir, issue_number)
            .map(|wt| (wt.path, wt.branch, wt.commit)),
    )
}

pub(crate) async fn ensure_card_worktree(
    pool: &PgPool,
    card_id: &str,
    context: Option<&serde_json::Value>,
) -> Result<Option<(String, String, String, bool)>> {
    let Some((issue_number, _repo_id)) = load_card_issue_repo_pg(pool, card_id).await else {
        return Ok(None);
    };
    let Some(issue_number) = issue_number else {
        return Ok(None);
    };
    let Some(repo_dir) =
        resolve_card_repo_dir_with_context_pg(pool, card_id, context, "create worktree repo")
            .await?
    else {
        return Ok(None);
    };
    let ensured =
        crate::services::platform::shell::ensure_worktree_for_issue(&repo_dir, issue_number)
            .map_err(|error| {
                anyhow::anyhow!("Cannot create worktree for card {card_id}: {error}")
            })?;
    Ok(Some((
        ensured.path,
        ensured.branch,
        ensured.commit,
        ensured.created,
    )))
}

/// PG-native variant of [`inject_review_dispatch_identifiers`].
///
/// Mutates `obj` to add review-target identifiers (repo, issue/PR numbers,
/// verdict/decision endpoints).
pub(crate) async fn inject_review_dispatch_identifiers(
    pool: &PgPool,
    card_id: &str,
    dispatch_type: &str,
    obj: &mut serde_json::Map<String, serde_json::Value>,
) {
    let repo = match json_map_string_field(obj, "repo")
        .or_else(|| json_map_string_field(obj, "target_repo"))
        .map(str::to_string)
    {
        Some(value) => Some(value),
        None => {
            resolve_card_target_repo_ref(
                pool,
                card_id,
                Some(&serde_json::Value::Object(obj.clone())),
            )
            .await
        }
    };
    if let Some(repo) = repo {
        obj.entry("repo".to_string()).or_insert_with(|| json!(repo));
    }

    if let Some(issue_number) = load_card_issue_repo_pg(pool, card_id)
        .await
        .and_then(|(issue, _)| issue)
    {
        obj.entry("issue_number".to_string())
            .or_insert_with(|| json!(issue_number));
    }

    if let Some(pr_number) = load_card_pr_number_pg(pool, card_id).await {
        obj.entry("pr_number".to_string())
            .or_insert_with(|| json!(pr_number));
    }

    match dispatch_type {
        "review" => {
            obj.entry("verdict_endpoint".to_string())
                .or_insert_with(|| json!("POST /api/reviews/verdict"));
        }
        "review-decision" => {
            obj.entry("decision_endpoint".to_string())
                .or_insert_with(|| json!("POST /api/reviews/decision"));
        }
        _ => {}
    }
}

async fn resolve_review_target_branch_pg(
    pool: &PgPool,
    card_id: &str,
    dir: &str,
    reviewed_commit: &str,
    preferred_branch: Option<&str>,
) -> Option<String> {
    let issue_branch_hint = load_card_issue_repo_pg(pool, card_id)
        .await
        .and_then(|(issue_number, _)| issue_number.map(|value| value.to_string()));
    crate::services::platform::shell::git_branch_containing_commit(
        dir,
        reviewed_commit,
        preferred_branch,
        issue_branch_hint.as_deref(),
    )
    .or_else(|| preferred_branch.map(str::to_string))
    .or_else(|| crate::services::platform::shell::git_branch_name(dir))
}

pub(crate) async fn commit_belongs_to_card_issue_pg(
    pool: &PgPool,
    card_id: &str,
    commit_sha: &str,
    target_repo: Option<&str>,
) -> bool {
    let issue_number = load_card_issue_repo_pg(pool, card_id)
        .await
        .and_then(|(issue_number, _)| issue_number);
    let Some(issue_number) = issue_number else {
        return true;
    };
    let repo_dir_result =
        match crate::services::platform::shell::resolve_repo_dir_for_target(target_repo) {
            Ok(value) => Ok(value),
            Err(_) => resolve_card_repo_dir_with_context_pg(
                pool,
                card_id,
                None,
                "validate reviewed commit",
            )
            .await
            .map_err(|e| e.to_string()),
        };
    let repo_dir = match repo_dir_result {
        Ok(Some(repo_dir)) => repo_dir,
        Ok(None) => {
            tracing::warn!(
                "[dispatch] commit_belongs_to_card_issue: repo dir unavailable for card {} — rejecting to fallback",
                card_id
            );
            return false;
        }
        Err(err) => {
            tracing::warn!(
                "[dispatch] commit_belongs_to_card_issue: {} — rejecting to fallback",
                err
            );
            return false;
        }
    };
    let output = match GitCommand::new()
        .repo(&repo_dir)
        .args(["log", "--format=%s", "-n", "1", commit_sha])
        .run_output()
    {
        Ok(output) => output,
        Err(err) if err.status_code().is_some() => {
            tracing::warn!(
                "[dispatch] commit_belongs_to_card_issue: commit {} not reachable: {} — rejecting to fallback",
                &commit_sha[..8.min(commit_sha.len())],
                err
            );
            return false;
        }
        Err(err) => {
            tracing::warn!(
                "[dispatch] commit_belongs_to_card_issue: git log failed: {} — rejecting to fallback",
                err
            );
            return false;
        }
    };
    let subject = String::from_utf8_lossy(&output.stdout);
    commit_subject_references_issue(&subject, issue_number)
}

/// #2341 / #2200 sub-3 (carried forward from PR #2336 HIGH 1): tri-state
/// scope verification (Postgres). Caller on the out-of-scope close path MUST
/// treat `Unknown` as a refusal (503) — a transient repo/git failure must
/// never terminalize a card.
pub(crate) async fn commit_belongs_to_card_issue_pg_tri(
    pool: &PgPool,
    card_id: &str,
    commit_sha: &str,
    target_repo: Option<&str>,
) -> ScopeCheck {
    let issue_number = load_card_issue_repo_pg(pool, card_id)
        .await
        .and_then(|(issue_number, _)| issue_number);
    let Some(issue_number) = issue_number else {
        // Preserve historical semantic: no issue to verify against → InScope
        // (the bool helper returns true here). We cannot affirm out-of-scope
        // without an issue number to compare against.
        return ScopeCheck::InScope;
    };
    let repo_dir_result =
        match crate::services::platform::shell::resolve_repo_dir_for_target(target_repo) {
            Ok(value) => Ok(value),
            Err(_) => resolve_card_repo_dir_with_context_pg(
                pool,
                card_id,
                None,
                "validate reviewed commit",
            )
            .await
            .map_err(|e| e.to_string()),
        };
    let repo_dir = match repo_dir_result {
        Ok(Some(repo_dir)) => repo_dir,
        Ok(None) => {
            tracing::warn!(
                "[dispatch] commit_belongs_to_card_issue_tri: repo dir unavailable for card {} — Unknown",
                card_id
            );
            return ScopeCheck::Unknown;
        }
        Err(err) => {
            tracing::warn!(
                "[dispatch] commit_belongs_to_card_issue_tri: {} — Unknown",
                err
            );
            return ScopeCheck::Unknown;
        }
    };
    let output = match GitCommand::new()
        .repo(&repo_dir)
        .args(["log", "--format=%s", "-n", "1", commit_sha])
        .run_output()
    {
        Ok(output) => output,
        Err(err) if err.status_code().is_some() => {
            tracing::warn!(
                "[dispatch] commit_belongs_to_card_issue_tri: commit {} not reachable: {} — Unknown",
                &commit_sha[..8.min(commit_sha.len())],
                err
            );
            return ScopeCheck::Unknown;
        }
        Err(err) => {
            tracing::warn!(
                "[dispatch] commit_belongs_to_card_issue_tri: git log failed: {} — Unknown",
                err
            );
            return ScopeCheck::Unknown;
        }
    };
    let subject = String::from_utf8_lossy(&output.stdout);
    if commit_subject_references_issue(&subject, issue_number) {
        ScopeCheck::InScope
    } else {
        ScopeCheck::OutOfScope
    }
}

async fn refresh_review_target_worktree_pg(
    pool: &PgPool,
    card_id: &str,
    context: &serde_json::Value,
    target: &DispatchExecutionTarget,
) -> Result<Option<DispatchExecutionTarget>> {
    // Codex round-4 followup: every `worktree_path: Some(...)` emission
    // below must go through the stable-clean probe. A HEAD-match alone is
    // not enough — a later rework or concurrent writer could have dirtied
    // the same checkout. Returning `Some(target.clone())` here used to
    // accept a stale dirty worktree.
    if let Some(recorded) = target.worktree_path.as_deref() {
        if let ReviewWorktreeProbe::Clean(clean_path) = stable_clean_probe_with_transient_retry(
            card_id,
            "refresh recorded",
            recorded,
            &target.reviewed_commit,
        )
        .await
        {
            return Ok(Some(DispatchExecutionTarget {
                reviewed_commit: target.reviewed_commit.clone(),
                branch: target.branch.clone(),
                worktree_path: Some(clean_path),
                target_repo: target.target_repo.clone(),
            }));
        }
        // Recorded path is missing, HEAD-mismatched, unstable, or dirty.
        // Don't accept it; let the issue-worktree / repo-dir fallbacks run
        // (they will themselves probe). If they also fail we emit a
        // commit-only target below.
    }

    if let Some(stale_path) = target.worktree_path.as_deref() {
        tracing::warn!(
            "[dispatch] Review dispatch for card {}: latest work target path '{}' no longer holds commit {} — attempting fallback",
            card_id,
            stale_path,
            &target.reviewed_commit[..8.min(target.reviewed_commit.len())]
        );
    }

    let resolve_context = if let Some(tr) = target.target_repo.as_deref() {
        let mut merged = context.clone();
        if let Some(obj) = merged.as_object_mut() {
            obj.insert("target_repo".to_string(), json!(tr));
        }
        std::borrow::Cow::Owned(merged)
    } else {
        std::borrow::Cow::Borrowed(context)
    };

    if let Some((wt_path, wt_branch, _wt_commit)) =
        resolve_card_worktree(pool, card_id, Some(resolve_context.as_ref())).await?
    {
        let active_probe = stable_clean_probe_with_transient_retry(
            card_id,
            "refresh active worktree",
            &wt_path,
            &target.reviewed_commit,
        )
        .await;
        if !matches!(active_probe, ReviewWorktreeProbe::NotMatching) {
            let branch = resolve_review_target_branch_pg(
                pool,
                card_id,
                &wt_path,
                &target.reviewed_commit,
                target.branch.as_deref().or(Some(wt_branch.as_str())),
            )
            .await
            .or(Some(wt_branch));
            let clean_wt = match active_probe {
                ReviewWorktreeProbe::Clean(path) => Some(path),
                ReviewWorktreeProbe::DirtyTransient(_) | ReviewWorktreeProbe::GitFailure(_) => None,
                ReviewWorktreeProbe::NotMatching => unreachable!("checked above"),
            };
            if clean_wt.is_some() {
                tracing::info!(
                    "[dispatch] Review dispatch for card {}: refreshed worktree path to active issue worktree '{}' for commit {}",
                    card_id,
                    wt_path,
                    &target.reviewed_commit[..8.min(target.reviewed_commit.len())]
                );
            } else {
                tracing::warn!(
                    "[dispatch] Review dispatch for card {}: active issue worktree '{}' failed stable-clean probe — emitting commit-only target for {}",
                    card_id,
                    wt_path,
                    &target.reviewed_commit[..8.min(target.reviewed_commit.len())]
                );
            }
            return Ok(Some(DispatchExecutionTarget {
                reviewed_commit: target.reviewed_commit.clone(),
                branch,
                worktree_path: clean_wt,
                target_repo: target.target_repo.clone(),
            }));
        }

        tracing::warn!(
            "[dispatch] Review dispatch for card {}: active issue worktree HEAD does not match reviewed commit {} — skipping path refresh",
            card_id,
            &target.reviewed_commit[..8.min(target.reviewed_commit.len())]
        );
    }

    let fallback_repo_dir = if let Some(value) = target.target_repo.as_deref() {
        crate::services::platform::shell::resolve_repo_dir_for_target(Some(value))
            .ok()
            .flatten()
    } else {
        None
    };
    let fallback_repo_dir = match fallback_repo_dir {
        Some(repo_dir) => Some(repo_dir),
        None => resolve_card_repo_dir_with_context_pg(
            pool,
            card_id,
            Some(context),
            "recover review target repo",
        )
        .await
        .ok()
        .flatten(),
    };

    if let Some(repo_dir) = fallback_repo_dir {
        let repo_probe = stable_clean_probe_with_transient_retry(
            card_id,
            "refresh fallback repo_dir",
            &repo_dir,
            &target.reviewed_commit,
        )
        .await;
        if !matches!(repo_probe, ReviewWorktreeProbe::NotMatching) {
            let branch = resolve_review_target_branch_pg(
                pool,
                card_id,
                &repo_dir,
                &target.reviewed_commit,
                target.branch.as_deref(),
            )
            .await;
            let clean_repo = match repo_probe {
                ReviewWorktreeProbe::Clean(path) => Some(path),
                ReviewWorktreeProbe::DirtyTransient(_) | ReviewWorktreeProbe::GitFailure(_) => None,
                ReviewWorktreeProbe::NotMatching => unreachable!("checked above"),
            };
            if clean_repo.is_some() {
                tracing::info!(
                    "[dispatch] Review dispatch for card {}: falling back to repo dir '{}' for commit {} after stale worktree cleanup",
                    card_id,
                    repo_dir,
                    &target.reviewed_commit[..8.min(target.reviewed_commit.len())]
                );
            } else {
                tracing::warn!(
                    "[dispatch] Review dispatch for card {}: fallback repo dir '{}' failed stable-clean probe — emitting commit-only target for {}",
                    card_id,
                    repo_dir,
                    &target.reviewed_commit[..8.min(target.reviewed_commit.len())]
                );
            }
            return Ok(Some(DispatchExecutionTarget {
                reviewed_commit: target.reviewed_commit.clone(),
                branch,
                worktree_path: clean_repo,
                target_repo: target.target_repo.clone(),
            }));
        }

        tracing::warn!(
            "[dispatch] Review dispatch for card {}: repo_dir '{}' HEAD does not match reviewed commit {} — emitting reviewed_commit without worktree_path",
            card_id,
            repo_dir,
            &target.reviewed_commit[..8.min(target.reviewed_commit.len())]
        );
        if git_commit_exists(&repo_dir, &target.reviewed_commit) {
            let branch = resolve_review_target_branch_pg(
                pool,
                card_id,
                &repo_dir,
                &target.reviewed_commit,
                target.branch.as_deref(),
            )
            .await;
            return Ok(Some(DispatchExecutionTarget {
                reviewed_commit: target.reviewed_commit.clone(),
                branch,
                worktree_path: None,
                target_repo: target.target_repo.clone(),
            }));
        }
    }

    tracing::warn!(
        "[dispatch] Review dispatch for card {}: no usable worktree or repo path contains commit {} after stale worktree cleanup",
        card_id,
        &target.reviewed_commit[..8.min(target.reviewed_commit.len())]
    );
    Ok(None)
}

async fn latest_completed_work_dispatch_target_pg(
    pool: &PgPool,
    kanban_card_id: &str,
) -> Option<DispatchExecutionTarget> {
    let (result_raw, context_raw): (Option<String>, Option<String>) = sqlx::query_as(
        "SELECT result::text, context::text
         FROM task_dispatches
         WHERE kanban_card_id = $1
           AND dispatch_type IN ('implementation', 'rework')
           AND status = 'completed'
         ORDER BY updated_at DESC, id DESC
         LIMIT 1",
    )
    .bind(kanban_card_id)
    .fetch_optional(pool)
    .await
    .ok()
    .flatten()?;

    let result_json = result_raw
        .as_deref()
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(raw).ok());
    let context_json = context_raw
        .as_deref()
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(raw).ok());

    let raw_path = result_json
        .as_ref()
        .and_then(|v| json_string_field(v, "completed_worktree_path"))
        .or_else(|| {
            result_json
                .as_ref()
                .and_then(|v| json_string_field(v, "worktree_path"))
        })
        .or_else(|| {
            context_json
                .as_ref()
                .and_then(|v| json_string_field(v, "worktree_path"))
        });
    let raw_branch = result_json
        .as_ref()
        .and_then(|v| json_string_field(v, "completed_branch"))
        .or_else(|| {
            result_json
                .as_ref()
                .and_then(|v| json_string_field(v, "worktree_branch"))
        })
        .or_else(|| {
            result_json
                .as_ref()
                .and_then(|v| json_string_field(v, "branch"))
        })
        .or_else(|| {
            context_json
                .as_ref()
                .and_then(|v| json_string_field(v, "worktree_branch"))
        })
        .or_else(|| {
            context_json
                .as_ref()
                .and_then(|v| json_string_field(v, "branch"))
        })
        .map(str::to_string);

    let (path, branch) = match raw_path {
        Some(candidate) if std::path::Path::new(candidate).is_dir() => {
            (Some(candidate), raw_branch)
        }
        Some(stale) => {
            tracing::warn!(
                "[dispatch] Card {}: dropping stale completed-work worktree metadata — recorded path '{}' no longer exists; clearing branch '{}' and falling back to fresh worktree resolution",
                kanban_card_id,
                stale,
                raw_branch.as_deref().unwrap_or("<none>")
            );
            (None, None)
        }
        None => (None, raw_branch),
    };
    let reviewed_commit = result_json
        .as_ref()
        .and_then(|v| json_string_field(v, "completed_commit"))
        .or_else(|| {
            result_json
                .as_ref()
                .and_then(|v| json_string_field(v, "head_sha"))
        })
        .or_else(|| {
            result_json
                .as_ref()
                .and_then(|v| json_string_field(v, "reviewed_commit"))
        })
        .or_else(|| {
            context_json
                .as_ref()
                .and_then(|v| json_string_field(v, "completed_commit"))
        })
        .or_else(|| {
            context_json
                .as_ref()
                .and_then(|v| json_string_field(v, "head_sha"))
        })
        .or_else(|| {
            context_json
                .as_ref()
                .and_then(|v| json_string_field(v, "reviewed_commit"))
        })
        .map(str::to_string);
    let target_repo = match context_json
        .as_ref()
        .and_then(|v| json_string_field(v, "target_repo"))
        .or_else(|| {
            result_json
                .as_ref()
                .and_then(|v| json_string_field(v, "target_repo"))
        })
        .map(str::to_string)
    {
        Some(value) => Some(value),
        None => resolve_card_target_repo_ref(pool, kanban_card_id, None).await,
    };

    if let Some(reviewed_commit) = reviewed_commit {
        let fallback_repo_dir = if let Some(value) = target_repo.as_deref() {
            crate::services::platform::shell::resolve_repo_dir_for_target(Some(value))
                .ok()
                .flatten()
        } else {
            None
        };
        let fallback_repo_dir = match fallback_repo_dir {
            Some(repo_dir) => Some(repo_dir),
            None => resolve_card_repo_dir_with_context_pg(
                pool,
                kanban_card_id,
                None,
                "recover completed work repo",
            )
            .await
            .ok()
            .flatten(),
        };
        let worktree_path = path.map(str::to_string).or(fallback_repo_dir);
        let issue_branch_hint = load_card_issue_repo_pg(pool, kanban_card_id)
            .await
            .and_then(|(issue_number, _)| issue_number.map(|value| value.to_string()));
        let branch = branch
            .or_else(|| {
                worktree_path.as_deref().and_then(|path| {
                    crate::services::platform::shell::git_branch_containing_commit(
                        path,
                        &reviewed_commit,
                        None,
                        issue_branch_hint.as_deref(),
                    )
                })
            })
            .or_else(|| {
                worktree_path
                    .as_deref()
                    .and_then(crate::services::platform::shell::git_branch_name)
            });
        return Some(DispatchExecutionTarget {
            reviewed_commit,
            branch,
            worktree_path,
            target_repo,
        });
    }

    let trusted_path =
        path.filter(|candidate| is_card_scoped_worktree_path(candidate, branch.as_deref()));

    trusted_path
        .and_then(execution_target_from_dir)
        .map(|mut target| {
            target.target_repo = target_repo;
            target
        })
}

async fn resolve_card_issue_commit_target_pg(
    pool: &PgPool,
    card_id: &str,
    context: Option<&serde_json::Value>,
) -> Result<Option<DispatchExecutionTarget>> {
    let Some((issue_number, _repo_id)) = load_card_issue_repo_pg(pool, card_id).await else {
        return Ok(None);
    };
    let Some(issue_number) = issue_number else {
        return Ok(None);
    };
    let Some(repo_dir) =
        resolve_card_repo_dir_with_context_pg(pool, card_id, context, "resolve commit target repo")
            .await?
    else {
        return Ok(None);
    };
    let Some(reviewed_commit) =
        crate::services::platform::find_latest_commit_for_issue(&repo_dir, issue_number)
    else {
        return Ok(None);
    };
    let issue_branch_hint = issue_number.to_string();
    let branch = crate::services::platform::shell::git_branch_containing_commit(
        &repo_dir,
        &reviewed_commit,
        None,
        Some(&issue_branch_hint),
    )
    .or_else(|| crate::services::platform::shell::git_branch_name(&repo_dir))
    .or(Some("main".to_string()));
    let worktree_path = probe_clean_worktree_with_transient_retry(
        card_id,
        "issue-commit fallback repo",
        &repo_dir,
        &reviewed_commit,
    )
    .await;
    Ok(Some(DispatchExecutionTarget {
        reviewed_commit,
        branch,
        worktree_path,
        target_repo: resolve_card_target_repo_ref(pool, card_id, context).await,
    }))
}

/// Brief backoff between a transient-dirty observation and the retry probe.
/// Issue #2254 item 4: a parallel implementation turn can leave the worktree
/// momentarily dirty between writes; this window is in the 100-500ms range in
/// practice. 500ms keeps review-dispatch latency in the same order of
/// magnitude as a normal pg query while giving the writer time to settle.
const REREVIEW_TRANSIENT_DIRTY_RETRY_DELAY: std::time::Duration =
    std::time::Duration::from_millis(500);

/// Gap between the two stability probes that must BOTH report clean before
/// we restore `worktree_path` after a transient-dirty observation. A single
/// clean sample is not enough — a concurrent writer could be mid-cycle and
/// dirty the tree again immediately after our probe. ~80ms is short enough
/// to keep total latency under 600ms but long enough to catch a writer that
/// just emitted one file (typical edit-save loops are tens of ms).
const REREVIEW_STABILITY_PROBE_GAP: std::time::Duration = std::time::Duration::from_millis(80);

/// Helper: a single "stable-clean" probe that requires two consecutive
/// clean observations separated by [`REREVIEW_STABILITY_PROBE_GAP`] before
/// returning the path. Used both as the initial check and on the
/// post-transient-dirty retry.
///
/// Codex round-4: even the first observation must pass stability — a writer
/// that briefly clean-flips between bursts could otherwise be accepted on
/// the very first sample.
async fn stable_clean_probe(
    card_id: &str,
    source: &str,
    path: &str,
    commit: &str,
) -> ReviewWorktreeProbe {
    let first = probe_clean_exact_review_worktree(card_id, source, path, commit).await;
    match &first {
        ReviewWorktreeProbe::Clean(_) => {}
        // Non-clean outcomes propagate unchanged.
        _ => return first,
    }
    tokio::time::sleep(REREVIEW_STABILITY_PROBE_GAP).await;
    probe_clean_exact_review_worktree(card_id, source, path, commit).await
}

async fn stable_clean_probe_with_transient_retry(
    card_id: &str,
    source: &str,
    path: &str,
    commit: &str,
) -> ReviewWorktreeProbe {
    match stable_clean_probe(card_id, source, path, commit).await {
        ReviewWorktreeProbe::DirtyTransient(_) => {
            tokio::time::sleep(REREVIEW_TRANSIENT_DIRTY_RETRY_DELAY).await;
            match stable_clean_probe(card_id, source, path, commit).await {
                ReviewWorktreeProbe::Clean(p) => {
                    tracing::info!(
                        "[dispatch] Review dispatch for card {}: {} worktree '{}' settled stable-clean after transient-dirty retry (commit {})",
                        card_id,
                        source,
                        path,
                        &commit[..8.min(commit.len())]
                    );
                    ReviewWorktreeProbe::Clean(p)
                }
                probe => {
                    tracing::warn!(
                        "[dispatch] Review dispatch for card {}: {} worktree '{}' failed stability re-check after transient-dirty retry — falling back to commit-only review (commit {})",
                        card_id,
                        source,
                        path,
                        &commit[..8.min(commit.len())]
                    );
                    probe
                }
            }
        }
        probe => probe,
    }
}

/// Probe a candidate worktree with stability check, retrying once on
/// `DirtyTransient` after [`REREVIEW_TRANSIENT_DIRTY_RETRY_DELAY`].
///
/// #2254 item 4: short-lived dirty state during a parallel implementation
/// turn used to make rereview drop `worktree_path` entirely. We now retry
/// once if and only if the worktree's HEAD is still the reviewed commit
/// after the backoff — that condition is enforced inside
/// `probe_clean_exact_review_worktree` (it returns `NotMatching` the moment
/// HEAD diverges), so a retry can never accept an unrelated commit that
/// happens to land on the same path mid-window.
///
/// Codex round-3 & round-4: a single clean observation is insufficient —
/// a concurrent writer could re-dirty the tree right after our probe. We
/// require TWO consecutive clean probes separated by
/// [`REREVIEW_STABILITY_PROBE_GAP`] before emitting `worktree_path`. This
/// applies to both the initial sample and the post-backoff retry.
async fn probe_clean_worktree_with_transient_retry(
    card_id: &str,
    source: &str,
    path: &str,
    commit: &str,
) -> Option<String> {
    match stable_clean_probe_with_transient_retry(card_id, source, path, commit).await {
        ReviewWorktreeProbe::Clean(p) => Some(p),
        // NotMatching and GitFailure: fail-closed, no retry. A path that
        // doesn't hold the reviewed commit will not start holding it, and
        // a git-status failure (#2254 bonus) is an integrity signal we
        // must surface rather than mask.
        ReviewWorktreeProbe::NotMatching
        | ReviewWorktreeProbe::DirtyTransient(_)
        | ReviewWorktreeProbe::GitFailure(_) => None,
    }
}

/// Recover the (`pr_tracking.repo_id`)-bound repo identifier and return the
/// pair (target_repo, repo_dir) used by review dispatch fallbacks.
///
/// Returned `target_repo` is the *pr_tracking* repo_id when present (the
/// value the PR is tracked against), falling back to the card's scope only
/// when pr_tracking has no recorded repo. The `repo_dir` is the filesystem
/// path that target_repo resolves to.
async fn pr_tracking_repo_binding(
    pool: &PgPool,
    card_id: &str,
    context: Option<&serde_json::Value>,
    pr_tracking_repo_id: Option<String>,
) -> (Option<String>, Option<String>) {
    let target_repo = match pr_tracking_repo_id {
        Some(value) => Some(value),
        None => resolve_card_target_repo_ref(pool, card_id, context).await,
    };
    let repo_dir = if let Some(value) = target_repo.as_deref() {
        crate::services::platform::shell::resolve_repo_dir_for_target(Some(value))
            .ok()
            .flatten()
    } else {
        None
    };
    let repo_dir = match repo_dir {
        Some(repo_dir) => Some(repo_dir),
        None => resolve_card_repo_dir_with_context_pg(
            pool,
            card_id,
            context,
            "recover pr_tracking review target repo",
        )
        .await
        .ok()
        .flatten(),
    };
    (target_repo, repo_dir)
}

async fn resolve_pr_tracking_review_target_pg(
    pool: &PgPool,
    card_id: &str,
    context: Option<&serde_json::Value>,
) -> Result<Option<DispatchExecutionTarget>> {
    let Some((repo_id, tracked_worktree_path, tracked_branch, head_sha)) = sqlx::query_as::<
        _,
        (
            Option<String>,
            Option<String>,
            Option<String>,
            Option<String>,
        ),
    >(
        "SELECT repo_id, worktree_path, branch, head_sha
         FROM pr_tracking
         WHERE card_id = $1
           AND head_sha IS NOT NULL
           AND length(trim(head_sha)) > 0
         ORDER BY updated_at DESC
         LIMIT 1",
    )
    .bind(card_id)
    .fetch_optional(pool)
    .await
    .map_err(|error| anyhow::anyhow!("load postgres pr_tracking target for {card_id}: {error}"))?
    else {
        return Ok(None);
    };

    let Some(reviewed_commit) = head_sha
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
    else {
        return Ok(None);
    };

    // #2254 item 6 — repo cross-check (Codex round-3: strict).
    //
    // `pr_tracking.head_sha` is an unauthenticated string commit pointer. If
    // the pr_tracking row is bound to a different repo than the card (e.g.
    // a mis-routed integration write, or a submodule / cherry-pick where
    // the commit hash literally exists in two repos), we must not emit that
    // head_sha as the review target — the reviewer would run on unrelated
    // code.
    //
    // Strict rule: when pr_tracking has a non-empty `repo_id`, that value
    // MUST resolve to the same local repo as the card's canonical scope
    // (the card's own `repo_id`, falling back to the per-deployment default
    // repo when the card row's column is NULL). Any of the following
    // outcomes is treated as "unprovable equivalence" and rejects the
    // pr_tracking head_sha:
    //   - pr_tracking.repo_id and card scope refer to different repos
    //   - either side cannot be resolved to a local path
    //   - the card has no recoverable canonical repo at all
    let pr_tracking_repo_id = repo_id
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());
    if let Some(pr_repo) = pr_tracking_repo_id.as_deref() {
        // `resolve_card_target_repo_ref` already encodes the
        // "card.repo_id or default repo" fallback, so a NULL
        // `kanban_cards.repo_id` does not silently bypass the check.
        let card_scope_repo = resolve_card_target_repo_ref(pool, card_id, context).await;
        let mismatch = match card_scope_repo.as_deref() {
            Some(card_repo) => {
                historical_target_repo_differs_from_card(Some(pr_repo), Some(card_repo))
            }
            // No anchor to verify against — reject to fail-closed.
            None => true,
        };
        if mismatch {
            tracing::warn!(
                "[dispatch] Review dispatch for card {}: pr_tracking.repo_id '{}' cannot be proven equivalent to card scope '{:?}' — refusing pr_tracking head_sha {} (#2254 item 6)",
                card_id,
                pr_repo,
                card_scope_repo.as_deref(),
                &reviewed_commit[..8.min(reviewed_commit.len())]
            );
            return Ok(None);
        }
    }

    let (target_repo, repo_dir) =
        pr_tracking_repo_binding(pool, card_id, context, pr_tracking_repo_id).await;

    let tracked_worktree_path = tracked_worktree_path
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());
    let mut branch = tracked_branch
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());
    // Codex round-4: anchor the commit in the *verified* card-scope repo
    // before trusting any other lookup. A pr_tracking row could carry a
    // matching `repo_id` (so the round-3 cross-check passes) but still
    // record a `worktree_path` pointing at an unrelated checkout. Without
    // requiring the commit to be present in the card-scope `repo_dir`, we
    // would happily use the foreign worktree as `commit_lookup_dir` and
    // emit a target that points the reviewer at unrelated code.
    let commit_lookup_dir;
    let mut worktree_path = None;

    if let Some(card_repo_dir) = repo_dir.as_deref() {
        if git_commit_exists(card_repo_dir, &reviewed_commit) {
            commit_lookup_dir = Some(card_repo_dir.to_string());
        } else {
            tracing::warn!(
                "[dispatch] Review dispatch for card {}: pr_tracking head_sha {} is not present in card-scope repo '{}' — refusing to recover from external worktree (#2254 round-4)",
                card_id,
                &reviewed_commit[..8.min(reviewed_commit.len())],
                card_repo_dir
            );
            return Ok(None);
        }
    } else {
        // No verifiable card-scope repo at all — we cannot anchor the
        // commit's repo identity.
        tracing::warn!(
            "[dispatch] Review dispatch for card {}: pr_tracking head_sha {} has no card-scope repo to verify against — refusing",
            card_id,
            &reviewed_commit[..8.min(reviewed_commit.len())]
        );
        return Ok(None);
    }

    // Now `repo_dir` is verified to contain the reviewed commit. The
    // tracked worktree (if any) may emit `worktree_path` only when (a) it
    // lies inside the verified card-scope repo (same git common dir) and
    // (b) it currently has the commit checked out with a stable-clean
    // tree.
    if let Some(path) = tracked_worktree_path.as_deref() {
        if worktree_path_belongs_to_repo(path, repo_dir.as_deref().unwrap_or(""))
            && git_commit_exists(path, &reviewed_commit)
        {
            // #2254 item 4: retry once on transient-dirty before falling back.
            worktree_path = probe_clean_worktree_with_transient_retry(
                card_id,
                "pr_tracking",
                path,
                &reviewed_commit,
            )
            .await;
        } else {
            tracing::warn!(
                "[dispatch] Review dispatch for card {}: pr_tracking worktree_path '{}' does not belong to card-scope repo '{}' — ignoring as worktree source",
                card_id,
                path,
                repo_dir.as_deref().unwrap_or("<unknown>")
            );
        }
    }

    if let Some(card_repo_dir) = repo_dir.as_deref() {
        if worktree_path.is_none() {
            worktree_path = probe_clean_worktree_with_transient_retry(
                card_id,
                "pr_tracking repo",
                card_repo_dir,
                &reviewed_commit,
            )
            .await;
        }
    }

    let Some(commit_lookup_dir) = commit_lookup_dir else {
        tracing::warn!(
            "[dispatch] Review dispatch for card {}: pr_tracking head_sha {} is not recoverable from its worktree or repo mapping",
            card_id,
            &reviewed_commit[..8.min(reviewed_commit.len())]
        );
        return Ok(None);
    };

    branch = resolve_review_target_branch_pg(
        pool,
        card_id,
        &commit_lookup_dir,
        &reviewed_commit,
        branch.as_deref(),
    )
    .await
    .or(branch);

    Ok(Some(DispatchExecutionTarget {
        reviewed_commit,
        branch,
        worktree_path,
        target_repo,
    }))
}

async fn execution_target_from_dir_blocking_async(dir: &str) -> Option<DispatchExecutionTarget> {
    let dir_owned = dir.to_string();
    match tokio::task::spawn_blocking(move || execution_target_from_dir(&dir_owned)).await {
        Ok(target) => target,
        Err(join_err) => {
            tracing::warn!(
                "[dispatch] spawn_blocking join failed while resolving repo HEAD target '{}': {}",
                dir,
                join_err
            );
            None
        }
    }
}

async fn resolve_repo_head_fallback_target_pg(
    pool: &PgPool,
    kanban_card_id: &str,
    context: Option<&serde_json::Value>,
) -> Result<Option<DispatchExecutionTarget>> {
    let Some(repo_dir) = resolve_card_repo_dir_with_context_pg(
        pool,
        kanban_card_id,
        context,
        "resolve repo-root HEAD fallback target",
    )
    .await?
    else {
        return Ok(None);
    };

    // #1563 RC9 (refined #2254 Codex round-4): when the card already has a
    // completed implementation dispatch with a recorded completed_commit,
    // prefer that commit over repo HEAD. Concurrent sub-issues can leave
    // the main worktree contaminated, but the review target is the
    // implementation commit — not whatever HEAD currently points at — so
    // dirty paths in the worktree do not threaten *commit-level* review
    // correctness when we have a stable commit pin.
    //
    // HOWEVER, emitting `worktree_path = repo_dir` without verifying that
    // HEAD actually matches the pinned commit AND the worktree is clean
    // would point the reviewer at potentially unrelated on-disk state
    // (different HEAD, dirty tracked changes). Codex round-4: gate the
    // worktree_path emission behind the stable-clean exact-HEAD probe;
    // fall back to commit-only review when the probe fails.
    if let Some(commit) = latest_completed_dispatch_commit_for_card_pg(pool, kanban_card_id).await {
        let probed = probe_clean_worktree_with_transient_retry(
            kanban_card_id,
            "repo-head completed_commit",
            &repo_dir,
            &commit,
        )
        .await;
        let mut target = DispatchExecutionTarget {
            reviewed_commit: commit,
            branch: crate::services::platform::shell::git_branch_name(&repo_dir),
            worktree_path: probed,
            target_repo: None,
        };
        target.target_repo = resolve_card_target_repo_ref(pool, kanban_card_id, context).await;
        return Ok(Some(target));
    }

    let Some(mut target) = execution_target_from_dir_blocking_async(&repo_dir).await else {
        return Ok(None);
    };
    match stable_clean_probe_with_transient_retry(
        kanban_card_id,
        "repo-head fallback",
        &repo_dir,
        &target.reviewed_commit,
    )
    .await
    {
        ReviewWorktreeProbe::Clean(path) => {
            target.worktree_path = Some(path);
        }
        ReviewWorktreeProbe::DirtyTransient(dirty_paths) => {
            anyhow::bail!(
                "Cannot create review dispatch for card {}: repo-root HEAD fallback is unsafe while tracked changes exist{}",
                kanban_card_id,
                dirty_paths_sample(&dirty_paths)
            );
        }
        ReviewWorktreeProbe::GitFailure(err) => {
            anyhow::bail!(
                "Cannot create review dispatch for card {}: repo-root HEAD fallback is unsafe — git status check failed for '{}': {}",
                kanban_card_id,
                repo_dir,
                err
            );
        }
        ReviewWorktreeProbe::NotMatching => {
            tracing::warn!(
                "[dispatch] Review dispatch for card {}: repo-root HEAD fallback changed while probing '{}' — skipping unsafe worktree_path",
                kanban_card_id,
                repo_dir
            );
            return Ok(None);
        }
    }
    target.target_repo = resolve_card_target_repo_ref(pool, kanban_card_id, context).await;
    Ok(Some(target))
}

/// #1563 RC9 helper: latest completed/review commit for this card. Looks at
/// task_dispatches JSON across implementation and rework dispatches in
/// completed status. Used to skip the dirty-worktree guard in the repo-HEAD
/// fallback when we already have a stable commit pin for the card's review
/// target.
async fn latest_completed_dispatch_commit_for_card_pg(
    pool: &PgPool,
    kanban_card_id: &str,
) -> Option<String> {
    // #2237 item 5 (revised after Codex round 1): the previous CTE cast
    // `result::TEXT::jsonb` and `context::TEXT::jsonb` directly. A single
    // legacy/partial-write row with non-JSON garbage would raise a Postgres
    // cast error that aborted the entire query, and the `.ok().flatten()`
    // sink silently dropped the failure — masking the data quality issue
    // AND breaking the dirty-worktree fallback for every card the query
    // touched.
    //
    // Codex's review (high) pointed out that even a regex-prefix check
    // like `^\s*\{` lets a *truncated* object such as `{` or
    // `{"completed_commit":` through, which then still aborts the cast.
    // The robust fix is to stop doing the JSON parsing in Postgres
    // entirely: fetch the candidate rows in order and parse `result`/
    // `context` as JSON in Rust, where a per-row parse failure is local
    // and recoverable. This also keeps the implementation portable
    // across Postgres versions (no `pg_input_is_valid` dependency) and
    // makes parse failures observable via warn-level logs.
    let rows = match sqlx::query_as::<_, (Option<String>, Option<String>)>(
        "SELECT result::TEXT, context::TEXT
         FROM task_dispatches
         WHERE kanban_card_id = $1
           AND dispatch_type IN ('implementation', 'rework')
           AND status = 'completed'
         ORDER BY updated_at DESC, id DESC",
    )
    .bind(kanban_card_id)
    .fetch_all(pool)
    .await
    {
        Ok(rows) => rows,
        Err(error) => {
            tracing::warn!(
                "[dispatch] latest_completed_dispatch_commit_for_card_pg query failed for card {}: {}. \
                 Treating as 'no completed commit' so the caller can fall through to other resolvers.",
                kanban_card_id,
                error
            );
            return None;
        }
    };

    const COMMIT_KEYS: [&str; 3] = ["completed_commit", "head_sha", "reviewed_commit"];

    fn first_commit_from_blob(
        blob: Option<&str>,
        kanban_card_id: &str,
        column: &str,
    ) -> Option<String> {
        let text = blob.map(str::trim).filter(|value| !value.is_empty())?;
        match serde_json::from_str::<serde_json::Value>(text) {
            Ok(serde_json::Value::Object(obj)) => {
                for key in COMMIT_KEYS {
                    if let Some(value) = obj.get(key).and_then(|v| v.as_str()) {
                        let trimmed = value.trim();
                        if !trimmed.is_empty() {
                            return Some(trimmed.to_string());
                        }
                    }
                }
                None
            }
            Ok(_) => None,
            Err(parse_err) => {
                tracing::warn!(
                    "[dispatch] task_dispatches.{} for card {} failed to parse as JSON ({}); \
                     skipping row for commit-pin extraction.",
                    column,
                    kanban_card_id,
                    parse_err
                );
                None
            }
        }
    }

    for (result_blob, context_blob) in rows {
        if let Some(commit) =
            first_commit_from_blob(result_blob.as_deref(), kanban_card_id, "result")
        {
            return Some(commit);
        }
        if let Some(commit) =
            first_commit_from_blob(context_blob.as_deref(), kanban_card_id, "context")
        {
            return Some(commit);
        }
    }

    None
}

/// PG-native variant of [`build_review_context`].
///
/// Injects `reviewed_commit`, `branch`, `worktree_path`, and provider info.
/// Prefers worktree branch (if found for this card's issue) over main HEAD.
///
/// #761 (Codex round-2): `trust` is an out-of-band parameter. `Untrusted`
/// unconditionally strips `reviewed_commit` / `worktree_path` / `branch` /
/// `target_repo` from the incoming context before the validation/refresh
/// chain runs. No JSON field on `context` can toggle this behavior — the
/// previous `_trusted_review_target` sentinel has been removed. API-sourced
/// callers (anyone reaching `POST /api/dispatches`) MUST pass `Untrusted`;
/// internal callers that already have first-party review-target values may
/// opt into `Trusted`.
pub(super) async fn build_review_context(
    pool: &PgPool,
    kanban_card_id: &str,
    to_agent_id: &str,
    context: &serde_json::Value,
    trust: ReviewTargetTrust,
    target_repo_source: TargetRepoSource,
) -> Result<String> {
    // #762 (A): the caller tells us explicitly whether `target_repo` in
    // `context` originated from their request or from our own fallback
    // injection. Inferring this from `context["target_repo"].is_some()` is
    // unreliable because upstream (`dispatch_create.rs`) pre-injects
    // `target_repo` into the context from the card's scope BEFORE calling
    // this function — which would make every dispatch look caller-supplied
    // and silently disable the `external_target_repo_unrecoverable` filter.
    let caller_supplied_target_repo =
        matches!(target_repo_source, TargetRepoSource::CallerSupplied)
            && json_string_field(context, "target_repo").is_some();
    let caller_supplied_unrecoverable_target_repo =
        if matches!(trust, ReviewTargetTrust::Untrusted) && caller_supplied_target_repo {
            json_string_field(context, "target_repo").and_then(|value| {
                match crate::services::platform::shell::resolve_repo_dir_for_target(Some(value)) {
                    Ok(Some(_)) => None,
                    _ => Some(value.to_string()),
                }
            })
        } else {
            None
        };
    if let Some(external_repo) = caller_supplied_unrecoverable_target_repo.as_deref() {
        anyhow::bail!(
            "external_target_repo_unrecoverable: 리뷰 대상 커밋을 원래 외부 target_repo에서 복구할 수 없습니다. 카드 기본 레포로 폴백하면 무관한 코드가 리뷰되므로 중단합니다. ({})",
            external_repo
        );
    }
    let ctx_val = dispatch_context_with_session_strategy("review", context);
    let mut obj = ctx_val.as_object().cloned().unwrap_or_default();

    // #761: Strip untrusted review-target fields before any downstream code
    // consumes them. The trust decision is out-of-band (the `trust` parameter
    // on this function's signature, not a JSON field), so a malicious or buggy
    // POST /api/dispatches body cannot opt out of stripping. Any legacy
    // `_trusted_review_target` key in the payload is also removed so it
    // cannot leak into the persisted dispatch context and mislead future
    // readers into thinking it carries meaning.
    obj.remove("_trusted_review_target");
    if matches!(trust, ReviewTargetTrust::Untrusted) {
        let mut stripped: Vec<&str> = Vec::new();
        for field in UNTRUSTED_REVIEW_TARGET_FIELDS {
            if obj.remove(*field).is_some() {
                stripped.push(field);
            }
        }
        if !stripped.is_empty() {
            tracing::warn!(
                "[dispatch] Review dispatch for card {}: stripped untrusted review-target fields from input context ({}) — validation/refresh chain will resolve them from card history",
                kanban_card_id,
                stripped.join(", ")
            );
        }
    }

    let target_repo = resolve_card_target_repo_ref(
        pool,
        kanban_card_id,
        Some(&serde_json::Value::Object(obj.clone())),
    )
    .await;
    if let Some(target_repo) = target_repo.as_deref() {
        obj.entry("target_repo".to_string())
            .or_insert_with(|| json!(target_repo));
    }
    let ctx_snapshot = serde_json::Value::Object(obj.clone());
    let is_noop_verification =
        obj.get("review_mode").and_then(|v| v.as_str()) == Some("noop_verification");
    let is_rereview_dispatch = obj
        .get("rereview")
        .and_then(|value| value.as_bool())
        .unwrap_or(false);
    let sandbox_preflight_without_external_side_effects =
        sandbox_preflight_card_disables_external_side_effects(pool, kanban_card_id).await;
    let card_issue_repo = load_card_issue_repo_pg(pool, kanban_card_id).await;
    let card_issue_number = card_issue_repo
        .as_ref()
        .and_then(|(issue_number, _)| *issue_number);

    if !is_noop_verification && !obj.contains_key("reviewed_commit") {
        let latest_work_target = if sandbox_preflight_without_external_side_effects {
            None
        } else {
            latest_completed_work_dispatch_target_pg(pool, kanban_card_id).await
        };
        let validated_work_target = if let Some(target) = latest_work_target.as_ref() {
            let valid = card_issue_number.is_none()
                || commit_belongs_to_card_issue_pg(
                    pool,
                    kanban_card_id,
                    &target.reviewed_commit,
                    target.target_repo.as_deref().or(target_repo.as_deref()),
                )
                .await;
            if !valid {
                tracing::warn!(
                    "[dispatch] Review dispatch for card {}: work target commit {} doesn't match card issue — skipping to next fallback",
                    kanban_card_id,
                    &target.reviewed_commit[..8.min(target.reviewed_commit.len())]
                );
            }
            if valid {
                refresh_review_target_worktree_pg(pool, kanban_card_id, &ctx_snapshot, target)
                    .await?
            } else {
                None
            }
        } else {
            None
        };
        if let Some(target) = validated_work_target {
            apply_review_target_context(&target, &mut obj);
            tracing::info!(
                "[dispatch] Review dispatch for card {}: reusing latest work target (commit {}, branch: {:?}, path: {:?})",
                kanban_card_id,
                &target.reviewed_commit[..8.min(target.reviewed_commit.len())],
                target.branch.as_deref(),
                target.worktree_path.as_deref()
            );
        } else {
            if let Some(target) = latest_work_target.as_ref() {
                tracing::warn!(
                    "[dispatch] Review dispatch for card {}: rejecting latest work target commit {} and skipping worktree refresh fallback",
                    kanban_card_id,
                    &target.reviewed_commit[..8.min(target.reviewed_commit.len())]
                );
            }

            let card_repo_id = card_issue_repo
                .as_ref()
                .and_then(|(_, repo_id)| repo_id.clone());
            let historical_external_repo_unrecoverable = latest_work_target
                .as_ref()
                .filter(|_| validated_work_target.is_none())
                .filter(|_| !caller_supplied_target_repo)
                .and_then(|target| target.target_repo.as_deref())
                .filter(|work_repo| {
                    historical_target_repo_differs_from_card(
                        Some(work_repo),
                        card_repo_id.as_deref(),
                    )
                })
                .map(|value| value.to_string());

            let pr_tracking_target = if is_rereview_dispatch
                && !sandbox_preflight_without_external_side_effects
            {
                resolve_pr_tracking_review_target_pg(pool, kanban_card_id, Some(&ctx_snapshot))
                    .await?
            } else {
                if is_rereview_dispatch && sandbox_preflight_without_external_side_effects {
                    tracing::info!(
                        "[dispatch] Review dispatch for card {}: sandbox preflight suppresses pr_tracking target resolution",
                        kanban_card_id
                    );
                }
                None
            };

            if let Some(external_repo) = historical_external_repo_unrecoverable {
                apply_review_target_warning(
                    &mut obj,
                    "external_target_repo_unrecoverable",
                    "리뷰 대상 커밋을 원래 외부 target_repo에서 복구할 수 없습니다. 카드 기본 레포로 폴백하면 무관한 코드가 리뷰되므로 중단합니다.",
                );
                obj.insert("target_repo".to_string(), json!(external_repo.clone()));
                tracing::warn!(
                    "[dispatch] Review dispatch for card {}: historical external target_repo '{}' is unrecoverable — suppressing card-scoped fallback",
                    kanban_card_id,
                    external_repo
                );
            } else if let Some(target) = pr_tracking_target {
                apply_review_target_context(&target, &mut obj);
                tracing::info!(
                    "[dispatch] Review dispatch for card {}: rereview using pr_tracking target (commit {}, branch: {:?}, path: {:?})",
                    kanban_card_id,
                    &target.reviewed_commit[..8.min(target.reviewed_commit.len())],
                    target.branch.as_deref(),
                    target.worktree_path.as_deref()
                );
            } else if !sandbox_preflight_without_external_side_effects
                && let Some((wt_path, wt_branch, wt_commit)) =
                    resolve_card_worktree(pool, kanban_card_id, Some(&ctx_snapshot)).await?
            {
                let reviewed_commit = wt_commit.clone();
                let worktree_path = probe_clean_worktree_with_transient_retry(
                    kanban_card_id,
                    "canonical worktree fallback",
                    &wt_path,
                    &reviewed_commit,
                )
                .await;
                apply_review_target_context(
                    &DispatchExecutionTarget {
                        reviewed_commit: wt_commit,
                        branch: Some(wt_branch.clone()),
                        worktree_path,
                        target_repo: target_repo.clone(),
                    },
                    &mut obj,
                );
                if obj.get("worktree_path").is_some() {
                    tracing::info!(
                        "[dispatch] Review dispatch for card {}: using canonical worktree branch '{}' (commit {}, path: {})",
                        kanban_card_id,
                        wt_branch,
                        &reviewed_commit[..8.min(reviewed_commit.len())],
                        wt_path
                    );
                } else {
                    tracing::warn!(
                        "[dispatch] Review dispatch for card {}: canonical worktree '{}' failed stable-clean probe — emitting commit-only target for {}",
                        kanban_card_id,
                        wt_path,
                        &reviewed_commit[..8.min(reviewed_commit.len())]
                    );
                }
            } else if !sandbox_preflight_without_external_side_effects
                && let Some(target) =
                    resolve_card_issue_commit_target_pg(pool, kanban_card_id, Some(&ctx_snapshot))
                        .await?
            {
                apply_review_target_context(&target, &mut obj);
                tracing::info!(
                    "[dispatch] Review dispatch for card {}: recovered issue commit target ({})",
                    kanban_card_id,
                    &target.reviewed_commit[..8.min(target.reviewed_commit.len())]
                );
            } else if latest_work_target.is_some() && validated_work_target.is_none() {
                apply_review_target_warning(
                    &mut obj,
                    "latest_work_target_issue_mismatch",
                    "브랜치 정보 없음 — 직접 확인 필요. 최근 완료 작업 커밋이 현재 카드 이슈와 일치하지 않아 repo HEAD 폴백을 생략했습니다.",
                );
                tracing::warn!(
                    "[dispatch] Review dispatch for card {}: latest work target was rejected, downstream worktree recovery failed, and repo HEAD fallback is disabled",
                    kanban_card_id
                );
            } else if !sandbox_preflight_without_external_side_effects
                && let Some(target) =
                    resolve_repo_head_fallback_target_pg(pool, kanban_card_id, Some(&ctx_snapshot))
                        .await?
            {
                apply_review_target_context(&target, &mut obj);
                tracing::info!(
                    "[dispatch] Review dispatch for card {}: no worktree, using repo HEAD ({})",
                    kanban_card_id,
                    &target.reviewed_commit[..8.min(target.reviewed_commit.len())]
                );
            }
        }
    }

    inject_review_merge_base_context(&mut obj);
    inject_review_quality_context(&mut obj);
    inject_review_dispatch_identifiers(pool, kanban_card_id, "review", &mut obj).await;

    if let Ok(Some(bindings)) = load_agent_channel_bindings_pg(pool, to_agent_id).await {
        apply_review_counter_model_provider_context(&mut obj, &bindings);
    }

    Ok(serde_json::to_string(&serde_json::Value::Object(obj))?)
}

#[cfg(test)]
mod review_target_context_tests {
    use super::{DispatchExecutionTarget, apply_review_target_context};
    use serde_json::json;

    #[test]
    fn apply_review_target_context_removes_stale_worktree_path_when_target_has_none() {
        let target = DispatchExecutionTarget {
            reviewed_commit: "abc123".to_string(),
            branch: Some("main".to_string()),
            worktree_path: None,
            target_repo: Some("/repo".to_string()),
        };
        let mut obj = serde_json::Map::from_iter([
            ("reviewed_commit".to_string(), json!("old")),
            ("branch".to_string(), json!("old-branch")),
            ("worktree_path".to_string(), json!("/dirty-worktree")),
            ("target_repo".to_string(), json!("/old-repo")),
        ]);

        apply_review_target_context(&target, &mut obj);

        assert_eq!(obj["reviewed_commit"], json!("abc123"));
        assert_eq!(obj["branch"], json!("main"));
        assert_eq!(obj["target_repo"], json!("/repo"));
        assert!(obj.get("worktree_path").is_none());
    }

    /// #2237 item 2: when the resolver returns `None` for branch /
    /// worktree_path / target_repo, the persisted context must shed any
    /// stale values for ALL three fields — not just `worktree_path`. This
    /// guards against a prior failed rereview leaking its branch or
    /// target_repo into the next dispatch context.
    #[test]
    fn apply_review_target_context_symmetrically_clears_all_target_fields_when_none() {
        let target = DispatchExecutionTarget {
            reviewed_commit: "abc123".to_string(),
            branch: None,
            worktree_path: None,
            target_repo: None,
        };
        let mut obj = serde_json::Map::from_iter([
            ("reviewed_commit".to_string(), json!("old")),
            ("branch".to_string(), json!("stale-branch")),
            (
                "worktree_branch".to_string(),
                json!("stale-worktree-branch"),
            ),
            ("worktree_path".to_string(), json!("/stale-worktree")),
            ("target_repo".to_string(), json!("/stale-repo")),
            ("unrelated".to_string(), json!("keep-me")),
        ]);

        apply_review_target_context(&target, &mut obj);

        assert_eq!(obj["reviewed_commit"], json!("abc123"));
        assert!(
            obj.get("branch").is_none(),
            "stale branch must be cleared when target.branch is None"
        );
        assert!(
            obj.get("worktree_branch").is_none(),
            "stale worktree_branch (branch alias) must be cleared when target.branch is None"
        );
        assert!(
            obj.get("worktree_path").is_none(),
            "stale worktree_path must be cleared when target.worktree_path is None"
        );
        assert!(
            obj.get("target_repo").is_none(),
            "stale target_repo must be cleared when target.target_repo is None"
        );
        // Sanity: unrelated fields are not touched.
        assert_eq!(obj["unrelated"], json!("keep-me"));
    }
}

#[cfg(test)]
mod issue_reference_tests {
    use super::commit_subject_references_issue;

    #[test]
    fn commit_subject_references_issue_accepts_squash_and_adk_prefix_forms() {
        assert!(commit_subject_references_issue("#1765 some title", 1765));
        assert!(commit_subject_references_issue("feat: foo (#1765)", 1765));
        // #2372: a single-reference subject whose `#N` is immediately
        // preceded by a back-reference verb (`refs`) is rejected. The
        // earlier sub-fix accepted any single-reference subject regardless
        // of verb, which let `refs #N`-only subjects impersonate ownership.
        assert!(!commit_subject_references_issue(
            "fix bug — refs #1765",
            1765
        ));
        // …but a single-reference subject whose `#N` is just a trailing
        // description hash (no back-reference verb in front of it) is
        // still ownership and is accepted — locks in the repo-history
        // pattern Codex flagged on round-2.
        assert!(commit_subject_references_issue(
            "Harden auto-queue phase gate repair #1765",
            1765
        ));
    }

    #[test]
    fn commit_subject_references_issue_rejects_partial_and_empty_subjects() {
        assert!(!commit_subject_references_issue("#17650 unrelated", 1765));
        assert!(!commit_subject_references_issue("", 1765));
        assert!(!commit_subject_references_issue("ABC#1765 unrelated", 1765));
        assert!(!commit_subject_references_issue(
            "#1765suffix unrelated",
            1765
        ));
    }

    /// #2200 sub-fix 2 (`validator-mismatch`): the original friction case had a
    /// commit subject of the form `'#523 …'` while the validator was only
    /// recognising the parenthesised squash form `'… (#523)'`. Both shapes
    /// must be accepted; multi-reference subjects must still require N to sit
    /// in a canonical position so cross-references from unrelated commits
    /// can't impersonate ownership (Codex adversarial review feedback).
    #[test]
    fn commit_subject_references_issue_friction_2200_validator_mismatch() {
        // Exact friction symptom: hash-prefix subject must be accepted.
        assert!(commit_subject_references_issue("#523 add foo", 523));
        // Parenthesised squash form continues to be accepted.
        assert!(commit_subject_references_issue("feat: bar (#523)", 523));
        // Bracketed form (some templates use square brackets).
        assert!(commit_subject_references_issue("[#523] do thing", 523));
        // Multi-reference subject where N is in the canonical leading
        // position must be accepted.
        assert!(commit_subject_references_issue(
            "#523 fix bug; also refs #999",
            523
        ));
        // Multi-reference subject where N is the trailing-squash suffix.
        assert!(commit_subject_references_issue(
            "feat: cross-link refs #100 (#523)",
            523
        ));

        // Negative: alphanumeric/underscore boundary blocks false positives.
        assert!(!commit_subject_references_issue("#5230 unrelated", 523));
        assert!(!commit_subject_references_issue("foo#523 noboundary", 523));
        assert!(!commit_subject_references_issue("#523_unrelated body", 523));
        assert!(!commit_subject_references_issue(
            "issue_#523 still bad",
            523
        ));

        // Negative — Codex round-1 finding: a multi-reference subject where
        // 523 is NOT in a canonical position must NOT be treated as proof of
        // ownership. The leading `#999` makes it ambiguous; another issue's
        // commit could borrow this subject when fixing both.
        assert!(!commit_subject_references_issue("fix #999, refs #523", 523));
        assert!(!commit_subject_references_issue(
            "chore: #100 #200 #523 #999 done",
            523
        ));
        // Negative: trailing reference shape, but multiple references — N is
        // not in a canonical position, so reject as ambiguous.
        assert!(!commit_subject_references_issue(
            "fix #999 follow-up — refs #523",
            523
        ));

        // Codex round-2 finding (#2200 sub-fix 2): duplicate references to
        // *the same* issue remain admissible because at least one
        // occurrence sits in a canonical position (leading `#523` or
        // trailing squash `(#523)`). #2372 round-3 update: the new
        // back-reference verb test also accepts mid-subject `#523` when
        // it isn't preceded by a back-reference verb, so `feat: foo #523 …`
        // is now treated as ownership too.
        assert!(commit_subject_references_issue("fix #523, refs #523", 523));
        assert!(commit_subject_references_issue(
            "#523 part 1 — followup refs #523",
            523
        ));
        assert!(commit_subject_references_issue(
            "wip: #523 part 1 — refs #523",
            523
        ));
        assert!(commit_subject_references_issue(
            "feat: foo #523 #523 #523 done",
            523
        ));
    }

    /// #2372 (Codex follow-up to #2200 sub-fix 2): single-reference subjects
    /// whose `#N` is preceded by a back-reference verb (`refs`, `reverts`,
    /// `see`, …) must be rejected, but legitimate single-reference subjects
    /// where the description naturally ends with `#N` must continue to
    /// validate. Locks in both negative and positive history-derived cases.
    #[test]
    fn commit_subject_references_issue_friction_2372_back_reference_verbs() {
        // Negative — explicit back-reference verbs preceding `#523`.
        assert!(!commit_subject_references_issue("refs #523", 523));
        assert!(!commit_subject_references_issue("reverts #523", 523));
        assert!(!commit_subject_references_issue(
            "follow-up — refs #523",
            523
        ));
        assert!(!commit_subject_references_issue(
            "Revert \"feat: bar\" — reverts #523",
            523
        ));
        assert!(!commit_subject_references_issue(
            "chore: cleanup, see #523",
            523
        ));
        assert!(!commit_subject_references_issue("re #523", 523));

        // Positive — GitHub closing verbs (`fixes`, `closes`, `resolves`,
        // `fix`, …) are *ownership* claims, not back-references.
        assert!(commit_subject_references_issue("fixes #523", 523));
        assert!(commit_subject_references_issue("closes #523", 523));
        assert!(commit_subject_references_issue("resolves #523", 523));

        // Positive — repo-history pattern Codex flagged on round-2: a
        // single-reference subject where `#N` is a trailing description hash
        // (no back-reference verb in front of it) is ownership.
        assert!(commit_subject_references_issue(
            "Harden auto-queue phase gate repair #2211",
            2211
        ));
        assert!(commit_subject_references_issue(
            "Add auto-queue phase-gate repair endpoint #2192",
            2192
        ));
        assert!(commit_subject_references_issue(
            "Fix voice transcript nonce fencing #2167",
            2167
        ));
        // Emoji / Fix-leading subjects continue to be canonical leading
        // references.
        assert!(commit_subject_references_issue("#523 in path/to/file", 523));
        assert!(commit_subject_references_issue(
            "Fix #523 in path/file",
            523
        ));
        assert!(commit_subject_references_issue("#523 in the middle", 523));

        // Squash and bracketed canonical forms still work.
        assert!(commit_subject_references_issue("feat: bar (#523)", 523));
        assert!(commit_subject_references_issue("[#523] add foo", 523));
        assert!(commit_subject_references_issue("(#523) add foo", 523));

        // Boundary-rejection cases continue to hold.
        assert!(!commit_subject_references_issue("#5230 unrelated", 523));
        assert!(!commit_subject_references_issue("(#5230)", 523));
    }

    /// #2372 round-4 (Codex follow-up): the back-reference verb check must
    /// see *through* intervening whitespace and punctuation, and the
    /// trailing-squash canonical form must also be invalidated when the
    /// `(#N)` group is itself preceded by a back-reference verb.
    /// Regressions for the exact subjects Codex flagged.
    #[test]
    fn commit_subject_references_issue_friction_2372_punctuated_back_references() {
        // Punctuated back-references — the verb is followed by `.`, `:`,
        // `—`, etc. before the `#N` token. Must all reject.
        assert!(!commit_subject_references_issue("cf. #523", 523));
        assert!(!commit_subject_references_issue("refs: #523", 523));
        assert!(!commit_subject_references_issue("see — #523", 523));
        assert!(!commit_subject_references_issue("re, #523", 523));
        assert!(!commit_subject_references_issue("relates: #523", 523));

        // Parenthesised back-references — the trailing `(#N)` *looks* like
        // a squash suffix but the preceding word is a back-reference verb.
        // Must reject; previously these slipped through the trailing-squash
        // canonical branch.
        assert!(!commit_subject_references_issue("see (#523)", 523));
        assert!(!commit_subject_references_issue("refs (#523)", 523));
        assert!(!commit_subject_references_issue("cf (#523)", 523));
        assert!(!commit_subject_references_issue("cf. (#523)", 523));

        // …but a genuine squash suffix where the preceding word is just a
        // description noun (not a back-reference verb) continues to be
        // ownership. Confirms the new check doesn't over-reject.
        assert!(commit_subject_references_issue("feat: bar (#523)", 523));
        assert!(commit_subject_references_issue(
            "chore: cleanup foo (#523)",
            523
        ));

        // Positive — closing verbs followed by punctuation remain ownership.
        assert!(commit_subject_references_issue("fixes: #523", 523));
        assert!(commit_subject_references_issue("closes — #523", 523));
        assert!(commit_subject_references_issue("resolves (#523)", 523));
    }
}

#[cfg(test)]
mod pg_rereview_tests {
    use super::*;
    use crate::db::auto_queue::test_support::TestPostgresDb;
    use serde_json::json;

    fn run_git(dir: &str, args: &[&str]) {
        GitCommand::new()
            .repo(dir)
            .args(args)
            .run_output()
            .unwrap_or_else(|err| panic!("git {args:?} failed: {err}"));
    }

    fn init_test_repo() -> tempfile::TempDir {
        let repo = tempfile::tempdir().unwrap();
        let repo_dir = repo.path().to_str().unwrap();
        run_git(repo_dir, &["init", "-b", "main"]);
        run_git(repo_dir, &["config", "user.email", "test@test.com"]);
        run_git(repo_dir, &["config", "user.name", "Test"]);
        run_git(repo_dir, &["commit", "--allow-empty", "-m", "initial"]);
        repo
    }

    fn git_commit(dir: &str, message: &str) -> String {
        run_git(dir, &["commit", "--allow-empty", "-m", message]);
        crate::services::platform::git_head_commit(dir).unwrap()
    }

    async fn pg_seed_card(
        pool: &PgPool,
        card_id: &str,
        issue_number: Option<i64>,
        repo_id: Option<&str>,
    ) {
        sqlx::query(
            "INSERT INTO kanban_cards (id, title, status, github_issue_number, repo_id)
             VALUES ($1, 'Test Card', 'ready', $2, $3)
             ON CONFLICT (id) DO UPDATE
             SET github_issue_number = EXCLUDED.github_issue_number,
                 repo_id = EXCLUDED.repo_id",
        )
        .bind(card_id)
        .bind(issue_number)
        .bind(repo_id)
        .execute(pool)
        .await
        .expect("seed kanban_cards");
    }

    async fn pg_seed_agent(
        pool: &PgPool,
        agent_id: &str,
        discord_channel_id: Option<&str>,
        discord_channel_alt: Option<&str>,
    ) {
        sqlx::query(
            "INSERT INTO agents (id, name, discord_channel_id, discord_channel_alt)
             VALUES ($1, $2, $3, $4)
             ON CONFLICT (id) DO UPDATE
             SET discord_channel_id = EXCLUDED.discord_channel_id,
                 discord_channel_alt = EXCLUDED.discord_channel_alt",
        )
        .bind(agent_id)
        .bind(format!("Agent {agent_id}"))
        .bind(discord_channel_id)
        .bind(discord_channel_alt)
        .execute(pool)
        .await
        .expect("seed agents");
    }

    #[tokio::test]
    async fn pg_latest_completed_dispatch_commit_uses_postgres_json_extraction() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;

        let card_id = "card-pg-latest-completed-commit";
        pg_seed_card(&pool, card_id, Some(9_929), None).await;
        sqlx::query(
            "INSERT INTO task_dispatches
                (id, kanban_card_id, dispatch_type, status, title, context, result, updated_at)
             VALUES
                ($1, $2, 'implementation', 'completed', 'Done', $3, $4, NOW())",
        )
        .bind("dispatch-pg-head-sha")
        .bind(card_id)
        .bind(json!({}).to_string())
        .bind(json!({ "completed_commit": " ", "head_sha": "abc123" }).to_string())
        .execute(&pool)
        .await
        .expect("seed completed dispatch");

        let latest = latest_completed_dispatch_commit_for_card_pg(&pool, card_id)
            .await
            .expect("completed dispatch head_sha should be extracted");

        assert_eq!(latest, "abc123");

        pool.close().await;
        pg_db.drop().await;
    }

    /// #2237 item 5 regression: a single malformed row (truncated object,
    /// non-JSON string, or partially-written JSON beginning with `{`) must
    /// NOT abort the entire commit lookup. The query previously cast
    /// `result::TEXT::jsonb` for every row, so one bad row aborted the
    /// statement and the call silently returned `None`, sending review
    /// dispatch into the dirty-worktree failure path. Codex flagged the
    /// regex-prefix fix as still vulnerable to truncated objects; the
    /// Rust-side parsing approach handles every malformed shape.
    #[tokio::test]
    async fn pg_latest_completed_dispatch_commit_survives_malformed_rows() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;

        let card_id = "card-pg-latest-completed-commit-malformed";
        pg_seed_card(&pool, card_id, Some(9_930), None).await;

        // Row A (older): malformed object-shaped JSON that begins with `{`
        // but is truncated. This is the exact pattern the Codex review
        // called out — the regex prefix guard would have let it through
        // and then `BTRIM(...)::jsonb` would have aborted the query.
        sqlx::query(
            "INSERT INTO task_dispatches
                (id, kanban_card_id, dispatch_type, status, title, context, result, updated_at)
             VALUES
                ($1, $2, 'implementation', 'completed', 'Old', $3, $4, NOW() - INTERVAL '2 hours')",
        )
        .bind("dispatch-malformed-truncated")
        .bind(card_id)
        .bind("{".to_string()) // truncated JSON object literal
        .bind("{\"completed_commit\":".to_string()) // partially written
        .execute(&pool)
        .await
        .expect("seed malformed dispatch");

        // Row B (older still): non-JSON garbage that doesn't even start
        // like an object.
        sqlx::query(
            "INSERT INTO task_dispatches
                (id, kanban_card_id, dispatch_type, status, title, context, result, updated_at)
             VALUES
                ($1, $2, 'rework', 'completed', 'Garbage', $3, $4, NOW() - INTERVAL '3 hours')",
        )
        .bind("dispatch-malformed-string")
        .bind(card_id)
        .bind("internal error: connection reset".to_string())
        .bind("not json at all".to_string())
        .execute(&pool)
        .await
        .expect("seed garbage dispatch");

        // Row C (newest): a valid completed dispatch with a head_sha. The
        // resolver must surface this even though older rows are corrupt.
        sqlx::query(
            "INSERT INTO task_dispatches
                (id, kanban_card_id, dispatch_type, status, title, context, result, updated_at)
             VALUES
                ($1, $2, 'implementation', 'completed', 'Good', $3, $4, NOW())",
        )
        .bind("dispatch-valid-newest")
        .bind(card_id)
        .bind(json!({}).to_string())
        .bind(json!({ "head_sha": "valid-commit-sha" }).to_string())
        .execute(&pool)
        .await
        .expect("seed valid newest dispatch");

        let latest = latest_completed_dispatch_commit_for_card_pg(&pool, card_id).await;
        assert_eq!(
            latest.as_deref(),
            Some("valid-commit-sha"),
            "malformed older rows must not mask the valid newest commit"
        );

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn pg_build_review_context_rereview_uses_pr_tracking_head_when_repo_root_dirty() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;

        let card_id = "card-pg-rereview-pr-tracking-dirty";
        let repo = init_test_repo();
        let repo_dir = repo.path().to_str().unwrap();
        let tracked_file = repo.path().join("tracked.rs");
        std::fs::write(&tracked_file, "clean\n").unwrap();
        run_git(repo_dir, &["add", "tracked.rs"]);
        let reviewed_commit = git_commit(repo_dir, "fix: committed card target");
        std::fs::write(&tracked_file, "dirty\n").unwrap();

        pg_seed_card(&pool, card_id, Some(9_928), Some(repo_dir)).await;
        pg_seed_agent(&pool, "agent-1", Some("111"), Some("222")).await;
        sqlx::query(
            "INSERT INTO pr_tracking (card_id, repo_id, worktree_path, branch, head_sha, state)
             VALUES ($1, $2, $3, 'main', $4, 'wait-ci')",
        )
        .bind(card_id)
        .bind(repo_dir)
        .bind(repo_dir)
        .bind(&reviewed_commit)
        .execute(&pool)
        .await
        .expect("seed pr_tracking");

        let pg_context = build_review_context(
            &pool,
            card_id,
            "agent-1",
            &json!({ "rereview": true, "reason": "runtime failure regression" }),
            ReviewTargetTrust::Untrusted,
            TargetRepoSource::CardScopeDefault,
        )
        .await
        .expect("rereview should use pr_tracking head_sha instead of dirty repo HEAD fallback");
        let pg_parsed: serde_json::Value = serde_json::from_str(&pg_context).unwrap();

        assert_eq!(pg_parsed["reviewed_commit"], reviewed_commit);
        assert_eq!(pg_parsed["branch"], "main");
        assert_eq!(pg_parsed["target_repo"], repo_dir);
        assert!(
            pg_parsed.get("worktree_path").is_none(),
            "dirty repo-root must not be emitted as a review worktree path: {pg_parsed}"
        );

        pool.close().await;
        pg_db.drop().await;
    }

    // ──────────────────────────────────────────────────────────────────
    // #2254 item 4 — transient-dirty retry
    // ──────────────────────────────────────────────────────────────────

    /// Item 4 happy path: a tracked file is dirty on the first probe, then a
    /// background thread cleans it up before the retry fires. The resolver
    /// must surface `worktree_path` instead of dropping it.
    #[tokio::test]
    async fn pg_rereview_retries_once_when_dirty_settles_within_backoff() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;

        let card_id = "card-pg-rereview-transient-dirty";
        let repo = init_test_repo();
        let repo_dir = repo.path().to_str().unwrap().to_string();
        let tracked = repo.path().join("tracked.rs");
        std::fs::write(&tracked, "clean\n").unwrap();
        run_git(&repo_dir, &["add", "tracked.rs"]);
        let reviewed_commit = git_commit(&repo_dir, "fix: target");
        // Start dirty.
        std::fs::write(&tracked, "transient writer\n").unwrap();

        pg_seed_card(&pool, card_id, Some(11_111), Some(&repo_dir)).await;
        pg_seed_agent(&pool, "agent-r", Some("rd"), Some("ra")).await;
        sqlx::query(
            "INSERT INTO pr_tracking (card_id, repo_id, worktree_path, branch, head_sha, state)
             VALUES ($1, $2, $3, 'main', $4, 'wait-ci')",
        )
        .bind(card_id)
        .bind(&repo_dir)
        .bind(&repo_dir)
        .bind(&reviewed_commit)
        .execute(&pool)
        .await
        .expect("seed pr_tracking");

        // Settle the dirty file partway through the 500ms retry window.
        let cleanup_path = tracked.clone();
        let cleaner = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(150)).await;
            std::fs::write(&cleanup_path, "clean\n").unwrap();
        });

        let pg_context = build_review_context(
            &pool,
            card_id,
            "agent-r",
            &json!({ "rereview": true, "reason": "transient dirty" }),
            ReviewTargetTrust::Untrusted,
            TargetRepoSource::CardScopeDefault,
        )
        .await
        .expect("rereview should succeed after transient-dirty retry");
        cleaner.await.unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&pg_context).unwrap();
        assert_eq!(parsed["reviewed_commit"], reviewed_commit);
        assert_eq!(
            parsed["worktree_path"], repo_dir,
            "transient dirty should be retried and the now-clean worktree path emitted: {parsed}"
        );

        pool.close().await;
        pg_db.drop().await;
    }

    /// Item 4 negative path: a genuinely contaminated worktree stays dirty
    /// across the retry. `worktree_path` must remain absent (we fall back to
    /// commit-only review).
    #[tokio::test]
    async fn pg_rereview_drops_worktree_when_dirty_persists_across_retry() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;

        let card_id = "card-pg-rereview-persistent-dirty";
        let repo = init_test_repo();
        let repo_dir = repo.path().to_str().unwrap().to_string();
        let tracked = repo.path().join("tracked.rs");
        std::fs::write(&tracked, "clean\n").unwrap();
        run_git(&repo_dir, &["add", "tracked.rs"]);
        let reviewed_commit = git_commit(&repo_dir, "fix: target");
        std::fs::write(&tracked, "persistent contamination\n").unwrap();

        pg_seed_card(&pool, card_id, Some(11_222), Some(&repo_dir)).await;
        pg_seed_agent(&pool, "agent-rp", Some("rpd"), Some("rpa")).await;
        sqlx::query(
            "INSERT INTO pr_tracking (card_id, repo_id, worktree_path, branch, head_sha, state)
             VALUES ($1, $2, $3, 'main', $4, 'wait-ci')",
        )
        .bind(card_id)
        .bind(&repo_dir)
        .bind(&repo_dir)
        .bind(&reviewed_commit)
        .execute(&pool)
        .await
        .expect("seed pr_tracking");

        let pg_context = build_review_context(
            &pool,
            card_id,
            "agent-rp",
            &json!({ "rereview": true, "reason": "still dirty" }),
            ReviewTargetTrust::Untrusted,
            TargetRepoSource::CardScopeDefault,
        )
        .await
        .expect("rereview should still build a context (commit-only)");
        let parsed: serde_json::Value = serde_json::from_str(&pg_context).unwrap();
        assert_eq!(parsed["reviewed_commit"], reviewed_commit);
        assert!(
            parsed.get("worktree_path").is_none(),
            "persistent dirty must keep worktree_path stripped: {parsed}"
        );

        pool.close().await;
        pg_db.drop().await;
    }

    // ──────────────────────────────────────────────────────────────────
    // #2254 item 6 — repo cross-check on pr_tracking.head_sha
    // ──────────────────────────────────────────────────────────────────

    /// Item 6: pr_tracking row is bound to a different repo_id than the
    /// card. The resolver must refuse to emit that head_sha so a downstream
    /// fallback (worktree / issue-commit / repo HEAD) runs instead.
    #[tokio::test]
    async fn pg_rereview_rejects_pr_tracking_when_repo_id_mismatches_card() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;

        let card_id = "card-pg-rereview-repo-mismatch";
        // Card's canonical repo.
        let card_repo = init_test_repo();
        let card_repo_dir = card_repo.path().to_str().unwrap().to_string();
        // A *different* repo whose history we don't own — this simulates a
        // mis-routed integration write or a coincidental shared SHA.
        let other_repo = init_test_repo();
        let other_repo_dir = other_repo.path().to_str().unwrap().to_string();
        let foreign_commit = git_commit(&other_repo_dir, "fix: cherry-picked elsewhere");

        pg_seed_card(&pool, card_id, Some(11_333), Some(&card_repo_dir)).await;
        pg_seed_agent(&pool, "agent-x", Some("xd"), Some("xa")).await;
        sqlx::query(
            "INSERT INTO pr_tracking (card_id, repo_id, worktree_path, branch, head_sha, state)
             VALUES ($1, $2, $3, 'main', $4, 'wait-ci')",
        )
        .bind(card_id)
        .bind(&other_repo_dir) // pr_tracking bound to the OTHER repo
        .bind(&other_repo_dir)
        .bind(&foreign_commit)
        .execute(&pool)
        .await
        .expect("seed pr_tracking");

        let target = resolve_pr_tracking_review_target_pg(&pool, card_id, None)
            .await
            .expect("resolver must succeed");
        assert!(
            target.is_none(),
            "pr_tracking head_sha bound to a different repo_id must be refused: {target:?}"
        );

        pool.close().await;
        pg_db.drop().await;
    }

    /// Item 6 (Codex round-3): when `kanban_cards.repo_id` is NULL and
    /// pr_tracking.repo_id is non-empty and *cannot* be proven equivalent
    /// to any card-side anchor, the resolver must refuse the head_sha.
    /// Previously the `if let (Some, Some)` gate let this slip through.
    #[tokio::test]
    async fn pg_rereview_rejects_pr_tracking_when_card_repo_id_is_null_and_unresolvable() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;

        let card_id = "card-pg-rereview-null-card-repo";
        let other_repo = init_test_repo();
        let other_repo_dir = other_repo.path().to_str().unwrap().to_string();
        let foreign_commit = git_commit(&other_repo_dir, "fix: unrelated");

        // Card with NULL repo_id — the resolver has no anchor to validate
        // pr_tracking.repo_id against.
        pg_seed_card(&pool, card_id, Some(11_555), None).await;
        pg_seed_agent(&pool, "agent-null", Some("nd"), Some("na")).await;
        sqlx::query(
            "INSERT INTO pr_tracking (card_id, repo_id, worktree_path, branch, head_sha, state)
             VALUES ($1, $2, $3, 'main', $4, 'wait-ci')",
        )
        .bind(card_id)
        .bind(&other_repo_dir)
        .bind(&other_repo_dir)
        .bind(&foreign_commit)
        .execute(&pool)
        .await
        .expect("seed pr_tracking");

        let target = resolve_pr_tracking_review_target_pg(&pool, card_id, None)
            .await
            .expect("resolver must succeed");
        assert!(
            target.is_none(),
            "card.repo_id NULL + non-empty pr_tracking.repo_id must reject when equivalence cannot be proven: {target:?}"
        );

        pool.close().await;
        pg_db.drop().await;
    }

    /// Item 4 (Codex round-3): a concurrent writer can clean the tree just
    /// long enough for one retry probe to see it clean, then immediately
    /// re-dirty. The stability re-check must catch that and fall back to
    /// commit-only review.
    #[tokio::test]
    async fn pg_rereview_drops_worktree_when_dirty_returns_after_first_clean_probe() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;

        let card_id = "card-pg-rereview-flapping-dirty";
        let repo = init_test_repo();
        let repo_dir = repo.path().to_str().unwrap().to_string();
        let tracked = repo.path().join("tracked.rs");
        std::fs::write(&tracked, "clean\n").unwrap();
        run_git(&repo_dir, &["add", "tracked.rs"]);
        let reviewed_commit = git_commit(&repo_dir, "fix: target");
        // Start dirty.
        std::fs::write(&tracked, "writer burst 1\n").unwrap();

        pg_seed_card(&pool, card_id, Some(11_666), Some(&repo_dir)).await;
        pg_seed_agent(&pool, "agent-flap", Some("fd"), Some("fa")).await;
        sqlx::query(
            "INSERT INTO pr_tracking (card_id, repo_id, worktree_path, branch, head_sha, state)
             VALUES ($1, $2, $3, 'main', $4, 'wait-ci')",
        )
        .bind(card_id)
        .bind(&repo_dir)
        .bind(&repo_dir)
        .bind(&reviewed_commit)
        .execute(&pool)
        .await
        .expect("seed pr_tracking");

        // Background writer: clean the file just before the post-backoff
        // probe, then dirty it again during the stability gap.
        let cleanup_path = tracked.clone();
        let writer = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(450)).await;
            std::fs::write(&cleanup_path, "clean\n").unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(30)).await;
            std::fs::write(&cleanup_path, "writer burst 2\n").unwrap();
        });

        let pg_context = build_review_context(
            &pool,
            card_id,
            "agent-flap",
            &json!({ "rereview": true, "reason": "flapping" }),
            ReviewTargetTrust::Untrusted,
            TargetRepoSource::CardScopeDefault,
        )
        .await
        .expect("rereview should still build a commit-only context");
        writer.await.unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&pg_context).unwrap();
        assert_eq!(parsed["reviewed_commit"], reviewed_commit);
        assert!(
            parsed.get("worktree_path").is_none(),
            "flapping dirty must not be accepted by the retry — got {parsed}"
        );

        pool.close().await;
        pg_db.drop().await;
    }

    /// Codex round-4 #1: a pr_tracking row with the *correct* `repo_id` but
    /// a foreign `worktree_path` and head_sha must not silently use that
    /// foreign worktree as the commit-lookup source. The resolver must
    /// anchor against the card-scope `repo_dir` first.
    #[tokio::test]
    async fn pg_rereview_rejects_foreign_worktree_path_with_matching_repo_id() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;

        let card_id = "card-pg-rereview-foreign-worktree";
        let card_repo = init_test_repo();
        let card_repo_dir = card_repo.path().to_str().unwrap().to_string();
        // Card-scope repo has its OWN commit history. The foreign repo has
        // a head_sha that does not exist in the card-scope repo.
        let _card_commit = git_commit(&card_repo_dir, "fix: card-side commit");
        let foreign_repo = init_test_repo();
        let foreign_repo_dir = foreign_repo.path().to_str().unwrap().to_string();
        let foreign_commit = git_commit(&foreign_repo_dir, "fix: foreign commit not in card repo");

        pg_seed_card(&pool, card_id, Some(11_777), Some(&card_repo_dir)).await;
        pg_seed_agent(&pool, "agent-fw", Some("fwd"), Some("fwa")).await;
        // repo_id matches the card, but worktree_path + head_sha live in a
        // foreign repo (e.g. mis-routed integration write into the same
        // row, or a stale path left over from an old checkout).
        sqlx::query(
            "INSERT INTO pr_tracking (card_id, repo_id, worktree_path, branch, head_sha, state)
             VALUES ($1, $2, $3, 'main', $4, 'wait-ci')",
        )
        .bind(card_id)
        .bind(&card_repo_dir)
        .bind(&foreign_repo_dir)
        .bind(&foreign_commit)
        .execute(&pool)
        .await
        .expect("seed pr_tracking");

        let target = resolve_pr_tracking_review_target_pg(&pool, card_id, None)
            .await
            .expect("resolver must succeed");
        assert!(
            target.is_none(),
            "foreign head_sha not present in card-scope repo must reject: {target:?}"
        );

        pool.close().await;
        pg_db.drop().await;
    }

    /// Codex round-4 followup: `refresh_review_target_worktree_pg` must
    /// also gate every `worktree_path: Some(...)` emission behind the
    /// stable-clean probe. A latest-work target with HEAD-match but dirty
    /// tracked files used to be accepted as-is, exposing the reviewer to
    /// uncommitted state.
    #[tokio::test]
    async fn pg_refresh_review_target_omits_worktree_path_when_recorded_path_is_dirty() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;

        let card_id = "card-pg-refresh-dirty-recorded";
        let repo = init_test_repo();
        let repo_dir = repo.path().to_str().unwrap().to_string();
        let tracked = repo.path().join("tracked.rs");
        std::fs::write(&tracked, "clean\n").unwrap();
        run_git(&repo_dir, &["add", "tracked.rs"]);
        // Subject must reference the card issue so commit_belongs_to_card_issue_pg accepts it.
        let work_commit = git_commit(&repo_dir, "fix: latest work (#11999)");
        // Dirty AFTER the commit — HEAD still matches, tree contaminated.
        std::fs::write(&tracked, "post-commit drift\n").unwrap();

        pg_seed_card(&pool, card_id, Some(11_999), Some(&repo_dir)).await;
        pg_seed_agent(&pool, "agent-rd", Some("rdd"), Some("rda")).await;
        // Seed a completed implementation dispatch with worktree_path set
        // to the dirty repo.
        sqlx::query(
            "INSERT INTO task_dispatches
                (id, kanban_card_id, dispatch_type, status, title, context, result, updated_at)
             VALUES
                ($1, $2, 'implementation', 'completed', 'Done', $3, $4, NOW())",
        )
        .bind("dispatch-refresh-1")
        .bind(card_id)
        .bind(json!({}).to_string())
        .bind(
            json!({
                "completed_commit": work_commit,
                "worktree_path": repo_dir,
                "target_repo": repo_dir,
            })
            .to_string(),
        )
        .execute(&pool)
        .await
        .expect("seed completed dispatch");

        // Build review context with rereview=false so the latest-work
        // path (with `refresh_review_target_worktree_pg`) is the one
        // exercised.
        let pg_context = build_review_context(
            &pool,
            card_id,
            "agent-rd",
            &json!({}),
            ReviewTargetTrust::Untrusted,
            TargetRepoSource::CardScopeDefault,
        )
        .await
        .expect("review context build must succeed");
        let parsed: serde_json::Value = serde_json::from_str(&pg_context).unwrap();
        assert_eq!(parsed["reviewed_commit"], work_commit);
        assert!(
            parsed.get("worktree_path").is_none(),
            "dirty recorded worktree must not be re-emitted: {parsed}"
        );

        pool.close().await;
        pg_db.drop().await;
    }

    /// #2371 follow-up: the canonical worktree fallback in
    /// `build_review_context` must use the same stable-clean gate as
    /// pr_tracking / refresh. A HEAD-matching issue worktree with dirty
    /// tracked files is still a commit target, but not a safe filesystem
    /// target.
    #[tokio::test]
    async fn pg_canonical_worktree_fallback_omits_worktree_path_when_dirty() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;

        let card_id = "card-pg-canonical-dirty";
        let issue_number = 12_001;
        let repo = init_test_repo();
        let repo_dir = repo.path().to_str().unwrap().to_string();
        let wt_dir = repo.path().join("wt-12001");
        let wt_path = wt_dir.to_str().unwrap().to_string();
        run_git(
            &repo_dir,
            &["worktree", "add", "-b", "wt/12001-review", &wt_path],
        );
        let tracked = wt_dir.join("tracked.rs");
        std::fs::write(&tracked, "clean\n").unwrap();
        run_git(&wt_path, &["add", "tracked.rs"]);
        let reviewed_commit = git_commit(&wt_path, "fix: canonical review target (#12001)");
        std::fs::write(&tracked, "dirty canonical worktree\n").unwrap();

        pg_seed_card(&pool, card_id, Some(issue_number), Some(&repo_dir)).await;
        pg_seed_agent(&pool, "agent-canon", Some("cd"), Some("ca")).await;

        let pg_context = build_review_context(
            &pool,
            card_id,
            "agent-canon",
            &json!({}),
            ReviewTargetTrust::Untrusted,
            TargetRepoSource::CardScopeDefault,
        )
        .await
        .expect("review context should fall back to commit-only canonical target");
        let parsed: serde_json::Value = serde_json::from_str(&pg_context).unwrap();
        assert_eq!(parsed["reviewed_commit"], reviewed_commit);
        assert_eq!(parsed["branch"], "wt/12001-review");
        assert!(
            parsed.get("worktree_path").is_none(),
            "dirty canonical worktree must not be emitted: {parsed}"
        );

        pool.close().await;
        pg_db.drop().await;
    }

    /// #2371 follow-up: issue-commit recovery points at the repo root, so it
    /// also needs the stable-clean gate before surfacing `worktree_path`.
    #[tokio::test]
    async fn pg_issue_commit_fallback_omits_worktree_path_when_repo_is_dirty() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;

        let card_id = "card-pg-issue-commit-dirty";
        let issue_number = 12_002;
        let repo = init_test_repo();
        let repo_dir = repo.path().to_str().unwrap().to_string();
        let tracked = repo.path().join("tracked.rs");
        std::fs::write(&tracked, "clean\n").unwrap();
        run_git(&repo_dir, &["add", "tracked.rs"]);
        let reviewed_commit = git_commit(&repo_dir, "fix: issue commit target (#12002)");
        std::fs::write(&tracked, "dirty issue fallback\n").unwrap();

        pg_seed_card(&pool, card_id, Some(issue_number), Some(&repo_dir)).await;
        pg_seed_agent(&pool, "agent-issue", Some("id"), Some("ia")).await;

        let pg_context = build_review_context(
            &pool,
            card_id,
            "agent-issue",
            &json!({}),
            ReviewTargetTrust::Untrusted,
            TargetRepoSource::CardScopeDefault,
        )
        .await
        .expect("review context should recover issue commit without dirty worktree_path");
        let parsed: serde_json::Value = serde_json::from_str(&pg_context).unwrap();
        assert_eq!(parsed["reviewed_commit"], reviewed_commit);
        assert_eq!(parsed["branch"], "main");
        assert!(
            parsed.get("worktree_path").is_none(),
            "dirty issue-commit repo must not be emitted: {parsed}"
        );

        pool.close().await;
        pg_db.drop().await;
    }

    /// Codex round-4 #3: the `latest_completed_dispatch_commit` early return
    /// in `resolve_repo_head_fallback_target_pg` must not emit `repo_dir`
    /// as `worktree_path` when the repo is dirty / HEAD doesn't match.
    /// Falls back to commit-only review.
    #[tokio::test]
    async fn pg_repo_head_fallback_omits_worktree_path_when_completed_commit_repo_is_dirty() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;

        let card_id = "card-pg-fallback-completed-dirty";
        let repo = init_test_repo();
        let repo_dir = repo.path().to_str().unwrap().to_string();
        let tracked = repo.path().join("tracked.rs");
        std::fs::write(&tracked, "clean\n").unwrap();
        run_git(&repo_dir, &["add", "tracked.rs"]);
        let completed_commit = git_commit(&repo_dir, "fix: completed");
        // Dirty the worktree AFTER the commit. HEAD still matches, but the
        // worktree is contaminated. Pre-round-4 the resolver would emit
        // `worktree_path = repo_dir` here.
        std::fs::write(&tracked, "post-commit contamination\n").unwrap();

        pg_seed_card(&pool, card_id, Some(11_888), Some(&repo_dir)).await;
        pg_seed_agent(&pool, "agent-cd", Some("cdd"), Some("cda")).await;
        // Seed a completed implementation dispatch carrying the commit.
        sqlx::query(
            "INSERT INTO task_dispatches
                (id, kanban_card_id, dispatch_type, status, title, context, result, updated_at)
             VALUES
                ($1, $2, 'implementation', 'completed', 'Done', $3, $4, NOW())",
        )
        .bind("dispatch-cd-1")
        .bind(card_id)
        .bind(json!({}).to_string())
        .bind(json!({ "completed_commit": completed_commit }).to_string())
        .execute(&pool)
        .await
        .expect("seed completed dispatch");

        let target = resolve_repo_head_fallback_target_pg(&pool, card_id, None)
            .await
            .expect("resolver must succeed")
            .expect("completed_commit must still produce a commit-only target");
        assert_eq!(target.reviewed_commit, completed_commit);
        assert!(
            target.worktree_path.is_none(),
            "dirty repo must not be emitted as worktree_path: {target:?}"
        );

        pool.close().await;
        pg_db.drop().await;
    }

    /// Item 6 control: when pr_tracking.repo_id and card.repo_id agree (or
    /// resolve to the same path), the resolver behaves normally.
    #[tokio::test]
    async fn pg_rereview_accepts_pr_tracking_when_repo_id_matches_card() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;

        let card_id = "card-pg-rereview-repo-match";
        let repo = init_test_repo();
        let repo_dir = repo.path().to_str().unwrap().to_string();
        let reviewed_commit = git_commit(&repo_dir, "fix: matching repo");

        pg_seed_card(&pool, card_id, Some(11_444), Some(&repo_dir)).await;
        pg_seed_agent(&pool, "agent-m", Some("md"), Some("ma")).await;
        sqlx::query(
            "INSERT INTO pr_tracking (card_id, repo_id, worktree_path, branch, head_sha, state)
             VALUES ($1, $2, $3, 'main', $4, 'wait-ci')",
        )
        .bind(card_id)
        .bind(&repo_dir)
        .bind(&repo_dir)
        .bind(&reviewed_commit)
        .execute(&pool)
        .await
        .expect("seed pr_tracking");

        let target = resolve_pr_tracking_review_target_pg(&pool, card_id, None)
            .await
            .expect("resolver must succeed")
            .expect("matching repo should yield a target");
        assert_eq!(target.reviewed_commit, reviewed_commit);

        pool.close().await;
        pg_db.drop().await;
    }
}

// ────────────────────────────────────────────────────────────────────────────
// #2254 bonus — git_tracked_change_paths fail-closed at safety-critical sites
// ────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod git_failure_failclosed_tests {
    use super::*;

    /// The strict variant returns `Err` on a non-repo directory (git status
    /// exits non-zero). The wrapper `clean_exact_review_worktree_path` must
    /// return `None` rather than silently treating that as "clean".
    #[tokio::test]
    async fn clean_exact_review_worktree_path_returns_none_on_git_failure() {
        // Use a tempdir that is NOT a git repo. `git status` will fail.
        let not_a_repo = tempfile::tempdir().unwrap();
        let path = not_a_repo.path().to_str().unwrap();
        let result = clean_exact_review_worktree_path(
            "card-not-a-repo",
            "test",
            path,
            "deadbeefdeadbeefdeadbeefdeadbeefdeadbeef",
        )
        .await;
        assert!(
            result.is_none(),
            "non-repo path must be rejected even when worktree_head_matches_commit short-circuits — got {result:?}"
        );
    }

    /// `probe_clean_exact_review_worktree` distinguishes git-failure from
    /// clean and dirty. We can't easily force `git status` to fail on a real
    /// repo, but a missing directory deterministically routes to
    /// `NotMatching` (HEAD check fails first). Verify that contract.
    #[tokio::test]
    async fn probe_returns_not_matching_for_missing_path() {
        let probe = probe_clean_exact_review_worktree(
            "card-missing",
            "test",
            "/tmp/agentdesk-this-path-must-not-exist-2254",
            "deadbeefdeadbeefdeadbeefdeadbeefdeadbeef",
        )
        .await;
        assert!(matches!(probe, ReviewWorktreeProbe::NotMatching));
    }
}
