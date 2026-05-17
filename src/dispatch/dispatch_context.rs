use anyhow::Result;
use serde_json::json;
#[cfg(all(test, feature = "legacy-sqlite-tests"))]
use sqlite_test::OptionalExtension;
use sqlx::PgPool;

use crate::db::Db;
use crate::db::agents::AgentChannelBindings;
#[cfg(all(test, feature = "legacy-sqlite-tests"))]
use crate::db::agents::load_agent_channel_bindings;
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

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
#[derive(Debug, Clone, Default)]
struct CardDispatchInfo {
    issue_number: Option<i64>,
    repo_id: Option<String>,
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

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
pub(super) fn resolve_parent_dispatch_context_sqlite_test(
    conn: &sqlite_test::Connection,
    card_id: &str,
    context: &serde_json::Value,
) -> Result<(Option<String>, i64)> {
    let Some(parent_dispatch_id) =
        json_string_field(context, "parent_dispatch_id").filter(|value| !value.is_empty())
    else {
        return Ok((None, 0));
    };

    let Some((parent_card_id, parent_chain_depth)) = conn
        .query_row(
            "SELECT kanban_card_id, COALESCE(chain_depth, 0)
             FROM task_dispatches
             WHERE id = ?1",
            [parent_dispatch_id],
            |row| Ok((row.get::<_, Option<String>>(0)?, row.get::<_, i64>(1)?)),
        )
        .optional()?
    else {
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

    Ok((Some(parent_dispatch_id.to_string()), parent_chain_depth + 1))
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

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
fn load_card_dispatch_info(db: &Db, card_id: &str) -> Option<CardDispatchInfo> {
    db.separate_conn().ok().and_then(|conn| {
        conn.query_row(
            "SELECT github_issue_number, repo_id FROM kanban_cards WHERE id = ?1",
            [card_id],
            |row| {
                Ok(CardDispatchInfo {
                    issue_number: row.get(0)?,
                    repo_id: row.get(1)?,
                })
            },
        )
        .ok()
    })
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
fn load_card_issue_repo(db: &Db, card_id: &str) -> Option<(Option<i64>, Option<String>)> {
    load_card_dispatch_info(db, card_id).map(|info| (info.issue_number, info.repo_id))
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
fn load_card_pr_number(db: &Db, card_id: &str) -> Option<i64> {
    db.separate_conn().ok().and_then(|conn| {
        conn.query_row(
            "SELECT pr_number FROM pr_tracking WHERE card_id = ?1",
            [card_id],
            |row| row.get::<_, Option<i64>>(0),
        )
        .optional()
        .ok()
        .flatten()
        .flatten()
    })
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
pub(crate) fn inject_review_dispatch_identifiers_sqlite_test(
    db: &Db,
    card_id: &str,
    dispatch_type: &str,
    obj: &mut serde_json::Map<String, serde_json::Value>,
) {
    let repo = json_map_string_field(obj, "repo")
        .or_else(|| json_map_string_field(obj, "target_repo"))
        .map(str::to_string)
        .or_else(|| {
            resolve_card_target_repo_ref_sqlite_test(
                db,
                card_id,
                Some(&serde_json::Value::Object(obj.clone())),
            )
        });
    if let Some(repo) = repo {
        obj.entry("repo".to_string()).or_insert_with(|| json!(repo));
    }

    if let Some(issue_number) = load_card_issue_repo(db, card_id).and_then(|(issue, _)| issue) {
        obj.entry("issue_number".to_string())
            .or_insert_with(|| json!(issue_number));
    }

    if let Some(pr_number) = load_card_pr_number(db, card_id) {
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
#[cfg(all(test, feature = "legacy-sqlite-tests"))]
pub(super) fn resolve_card_target_repo_ref_sqlite_test(
    db: &Db,
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

    let info = load_card_dispatch_info(db, card_id)?;
    info.repo_id
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
fn resolve_card_repo_dir_with_context(
    db: &Db,
    card_id: &str,
    context: Option<&serde_json::Value>,
    purpose: &str,
) -> Result<Option<String>> {
    let target_repo = resolve_card_target_repo_ref_sqlite_test(db, card_id, context);
    crate::services::platform::shell::resolve_repo_dir_for_target(target_repo.as_deref())
        .map_err(|e| anyhow::anyhow!("Cannot {purpose} for card {}: {}", card_id, e))
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
fn resolve_card_repo_dir(db: &Db, card_id: &str, purpose: &str) -> Result<Option<String>> {
    resolve_card_repo_dir_with_context(db, card_id, None, purpose)
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
///   - `(#N)` at the end of subject — squash-merge suffix
///   - `[#N] <subject>`            — bracketed leading reference
///   - `<subject> ... #N` when N is the *only* `#<digits>` reference in the
///     subject — supports trailing "refs #N" / em-dash forms
///
/// Rejected — multi-issue subjects where N is not in a canonical position
/// (leading or trailing-squash) are treated as ambiguous because they could
/// originate from an unrelated commit that merely cross-references this card.
/// Sub-fix 2 of #2200 (validator-mismatch) tightened this: previously *any*
/// word-boundary occurrence of `#N` anywhere in the subject was accepted,
/// which let a `fix #999, refs #523`-style subject borrowed from another
/// issue pass through and poison the reviewed_commit fallback chain.
fn commit_subject_references_issue(subject: &str, issue_number: i64) -> bool {
    let needle = format!("#{issue_number}");
    // Collect all delimiter-bounded `#<digits>` references in the subject. We
    // need both the count of distinct numeric references (to detect ambiguous
    // multi-reference subjects) and the byte offsets of every match of our
    // needle so we can check whether at least one occurrence sits in a
    // canonical position.
    let mut all_refs: Vec<&str> = Vec::new();
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
                    all_refs.push(&subject[i..j]);
                }
            }
            i = j.max(i + 1);
        } else {
            i += 1;
        }
    }

    let mut found_needle = false;
    let mut competing_ref = false;
    for value in &all_refs {
        if *value == needle {
            found_needle = true;
        } else {
            competing_ref = true;
        }
    }
    if !found_needle {
        return false;
    }
    if !competing_ref {
        // Subject references only this issue (one or many times): a
        // delimiter-bounded `#N` is sufficient proof. Duplicate references
        // to the same issue (e.g. `fix #523, refs #523`) are explicitly not
        // ambiguous (Codex round-2 finding).
        return true;
    }

    // Multi-reference subject: require N to sit in a canonical position so
    // ambiguous cross-references can't impersonate ownership. Canonical = the
    // leading reference or the trailing-squash `(#N)` suffix.
    let trimmed = subject.trim();
    if let Some(rest) = trimmed.strip_prefix(&needle) {
        if rest
            .chars()
            .next()
            .map(|ch| !ch.is_ascii_alphanumeric() && ch != '_')
            .unwrap_or(true)
        {
            return true;
        }
    }
    if trimmed.starts_with(&format!("[{}]", needle))
        || trimmed.starts_with(&format!("({})", needle))
    {
        return true;
    }
    let squash_suffix = format!("({})", needle);
    if trimmed.ends_with(&squash_suffix) {
        return true;
    }
    false
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
pub(crate) fn commit_belongs_to_card_issue(
    db: &Db,
    card_id: &str,
    commit_sha: &str,
    target_repo: Option<&str>,
) -> bool {
    let issue_number = load_card_issue_repo(db, card_id).and_then(|(issue_number, _)| issue_number);
    let Some(issue_number) = issue_number else {
        return true;
    };
    let repo_dir = match crate::services::platform::shell::resolve_repo_dir_for_target(target_repo)
        .or_else(|_| {
            resolve_card_repo_dir(db, card_id, "validate reviewed commit")
                .map_err(|e| e.to_string())
        }) {
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

#[cfg(not(all(test, feature = "legacy-sqlite-tests")))]
pub(crate) fn commit_belongs_to_card_issue(
    _db: &Db,
    card_id: &str,
    commit_sha: &str,
    _target_repo: Option<&str>,
) -> bool {
    tracing::warn!(
        "[dispatch] sqlite commit/card validation disabled for card {} commit {}; postgres pool required",
        card_id,
        &commit_sha[..8.min(commit_sha.len())]
    );
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

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
pub(crate) fn commit_belongs_to_card_issue_tri(
    db: &Db,
    card_id: &str,
    commit_sha: &str,
    target_repo: Option<&str>,
) -> ScopeCheck {
    let issue_number = load_card_issue_repo(db, card_id).and_then(|(issue_number, _)| issue_number);
    let Some(issue_number) = issue_number else {
        return ScopeCheck::InScope;
    };
    let repo_dir = match crate::services::platform::shell::resolve_repo_dir_for_target(target_repo)
        .or_else(|_| {
            resolve_card_repo_dir(db, card_id, "validate reviewed commit")
                .map_err(|e| e.to_string())
        }) {
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

#[cfg(not(all(test, feature = "legacy-sqlite-tests")))]
pub(crate) fn commit_belongs_to_card_issue_tri(
    _db: &Db,
    card_id: &str,
    commit_sha: &str,
    _target_repo: Option<&str>,
) -> ScopeCheck {
    tracing::warn!(
        "[dispatch] sqlite tri-state scope check disabled for card {} commit {}; postgres pool required — Unknown",
        card_id,
        &commit_sha[..8.min(commit_sha.len())]
    );
    ScopeCheck::Unknown
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
    DirtyTransient,
    /// `git status` itself failed (index lock, permission denied, corrupt
    /// repo). Fail-closed at the caller — pre-#2254 this was silently
    /// treated as "clean" via `unwrap_or_default()`.
    GitFailure(String),
}

/// Async wrapper around the strict `git_tracked_change_paths_strict` shell
/// invocation: combines #2237 item 3 (run shell on blocking pool) with
/// #2254 fail-closed semantics (distinguish git-failure from clean).
async fn git_tracked_change_paths_strict_async(path: &str) -> Result<Vec<String>, String> {
    let path_owned = path.to_string();
    match tokio::task::spawn_blocking(move || {
        crate::services::platform::shell::git_tracked_change_paths_strict(&path_owned)
    })
    .await
    {
        Ok(result) => result,
        Err(join_err) => Err(format!("spawn_blocking join failed: {join_err}")),
    }
}

async fn probe_clean_exact_review_worktree(
    card_id: &str,
    source: &str,
    path: &str,
    commit: &str,
) -> ReviewWorktreeProbe {
    if !std::path::Path::new(path).is_dir() || !worktree_head_matches_commit(path, commit) {
        return ReviewWorktreeProbe::NotMatching;
    }

    match git_tracked_change_paths_strict_async(path).await {
        Ok(dirty_paths) if dirty_paths.is_empty() => ReviewWorktreeProbe::Clean(path.to_string()),
        Ok(dirty_paths) => {
            let sample = dirty_paths
                .iter()
                .take(3)
                .cloned()
                .collect::<Vec<_>>()
                .join(", ");
            tracing::warn!(
                "[dispatch] Review dispatch for card {}: {} worktree_path '{}' for commit {} has tracked changes{}",
                card_id,
                source,
                path,
                &commit[..8.min(commit.len())],
                if sample.is_empty() {
                    String::new()
                } else if dirty_paths.len() > 3 {
                    format!(" ({sample}, +{} more)", dirty_paths.len() - 3)
                } else {
                    format!(" ({sample})")
                }
            );
            ReviewWorktreeProbe::DirtyTransient
        }
        Err(err) => {
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
            ReviewWorktreeProbe::GitFailure(err)
        }
    }
}

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
        | ReviewWorktreeProbe::DirtyTransient
        | ReviewWorktreeProbe::GitFailure(_) => None,
    }
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
fn resolve_review_target_branch(
    db: &Db,
    card_id: &str,
    dir: &str,
    reviewed_commit: &str,
    preferred_branch: Option<&str>,
) -> Option<String> {
    let issue_branch_hint = load_card_issue_repo(db, card_id)
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

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
fn refresh_review_target_worktree(
    db: &Db,
    card_id: &str,
    context: &serde_json::Value,
    target: &DispatchExecutionTarget,
) -> Result<Option<DispatchExecutionTarget>> {
    // #682 (Codex review, [medium]): the recorded worktree_path may still
    // exist as a directory but point at a *different* checkout now (e.g. a
    // later session recycled the path for another issue). Accept the
    // recorded path only when the reviewed_commit is reachable from the
    // worktree's current HEAD; otherwise fall through to recovery.
    //
    // git_commit_exists is insufficient here — git's object store is
    // shared across worktrees of the same repo, so any commit anywhere
    // in the repo satisfies it. worktree_head_reaches_commit confirms
    // the reviewed state is actually what is checked out.
    if let Some(recorded) = target.worktree_path.as_deref() {
        if std::path::Path::new(recorded).is_dir()
            && worktree_head_matches_commit(recorded, &target.reviewed_commit)
        {
            return Ok(Some(target.clone()));
        }
    }

    if let Some(stale_path) = target.worktree_path.as_deref() {
        tracing::warn!(
            "[dispatch] Review dispatch for card {}: latest work target path '{}' no longer holds commit {} — attempting fallback",
            card_id,
            stale_path,
            &target.reviewed_commit[..8.min(target.reviewed_commit.len())]
        );
    }

    // #682 (Codex round 2, [high]): resolve_card_worktree picks the repo
    // from the current card/context, not the historical completion's
    // target_repo. For an external-repo completion whose card's canonical
    // repo differs, this would miss the right worktree. Inject target_repo
    // into a local context copy so resolve_card_worktree's repo lookup
    // honors the completion's recorded repo before falling back to the
    // card's default.
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
        resolve_card_worktree_sqlite_test(db, card_id, Some(resolve_context.as_ref()))?
    {
        // Use the exact-HEAD check here too — a worktree whose HEAD has
        // advanced past reviewed_commit still satisfies git_commit_exists
        // via the shared object store, but the files on disk are the
        // descendant state, not what was reviewed.
        if worktree_head_matches_commit(&wt_path, &target.reviewed_commit) {
            let branch = resolve_review_target_branch(
                db,
                card_id,
                &wt_path,
                &target.reviewed_commit,
                target.branch.as_deref().or(Some(wt_branch.as_str())),
            )
            .or(Some(wt_branch));
            tracing::info!(
                "[dispatch] Review dispatch for card {}: refreshed worktree path to active issue worktree '{}' for commit {}",
                card_id,
                wt_path,
                &target.reviewed_commit[..8.min(target.reviewed_commit.len())]
            );
            return Ok(Some(DispatchExecutionTarget {
                reviewed_commit: target.reviewed_commit.clone(),
                branch,
                worktree_path: Some(wt_path),
                target_repo: target.target_repo.clone(),
            }));
        }

        tracing::warn!(
            "[dispatch] Review dispatch for card {}: active issue worktree HEAD does not match reviewed commit {} — skipping path refresh",
            card_id,
            &target.reviewed_commit[..8.min(target.reviewed_commit.len())]
        );
    }

    // #682 (Codex review, [high]): prefer the explicit target_repo recorded
    // on the completion before falling back to card-scoped repo resolution.
    // Issue-less cards that ran against an external repo (recorded as
    // target_repo on the work dispatch) would otherwise lose the original
    // repo when their worktree was cleaned up.
    let fallback_repo_dir = target
        .target_repo
        .as_deref()
        .and_then(|value| {
            crate::services::platform::shell::resolve_repo_dir_for_target(Some(value))
                .ok()
                .flatten()
        })
        .or_else(|| {
            resolve_card_repo_dir_with_context(
                db,
                card_id,
                Some(context),
                "recover review target repo",
            )
            .ok()
            .flatten()
        });

    if let Some(repo_dir) = fallback_repo_dir {
        // #682 (Codex round 3, [high]): require the repo_dir's HEAD to be
        // exactly reviewed_commit before emitting it as worktree_path. The
        // shared git object store makes git_commit_exists trivially pass
        // for any commit anywhere in the repo — but if HEAD is checked out
        // at something else, the reviewer sees unrelated filesystem state.
        // Exact HEAD match guarantees the on-disk state is what was reviewed.
        if worktree_head_matches_commit(&repo_dir, &target.reviewed_commit) {
            let branch = resolve_review_target_branch(
                db,
                card_id,
                &repo_dir,
                &target.reviewed_commit,
                target.branch.as_deref(),
            );
            tracing::info!(
                "[dispatch] Review dispatch for card {}: falling back to repo dir '{}' for commit {} after stale worktree cleanup",
                card_id,
                repo_dir,
                &target.reviewed_commit[..8.min(target.reviewed_commit.len())]
            );
            return Ok(Some(DispatchExecutionTarget {
                reviewed_commit: target.reviewed_commit.clone(),
                branch,
                worktree_path: Some(repo_dir),
                target_repo: target.target_repo.clone(),
            }));
        }

        tracing::warn!(
            "[dispatch] Review dispatch for card {}: repo_dir '{}' HEAD does not match reviewed commit {} — emitting reviewed_commit without worktree_path",
            card_id,
            repo_dir,
            &target.reviewed_commit[..8.min(target.reviewed_commit.len())]
        );
        // We know the commit exists in this repo (cat-file via the earlier
        // branch); hand back reviewed_commit and let the reviewer inspect
        // it via git commands, without misleading worktree_path.
        if git_commit_exists(&repo_dir, &target.reviewed_commit) {
            let branch = resolve_review_target_branch(
                db,
                card_id,
                &repo_dir,
                &target.reviewed_commit,
                target.branch.as_deref(),
            );
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

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
fn latest_completed_work_dispatch_target(
    db: &Db,
    kanban_card_id: &str,
) -> Option<DispatchExecutionTarget> {
    let conn = db.separate_conn().ok()?;
    let (result_raw, context_raw): (Option<String>, Option<String>) = conn
        .query_row(
            "SELECT result, context
             FROM task_dispatches
             WHERE kanban_card_id = ?1
               AND dispatch_type IN ('implementation', 'rework')
               AND status = 'completed'
             ORDER BY updated_at DESC, rowid DESC
             LIMIT 1",
            [kanban_card_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .ok()?;

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

    // #800: Validate that the recorded worktree path still exists on disk
    // before propagating it into the new dispatch context. A done card that
    // gets reopened may reference a worktree that has since been removed
    // (`git worktree remove`, manual cleanup, branch deletion). Without this
    // check the new dispatch is silently re-pointed at the orphaned path,
    // which manifests as the agent restarting work in the same broken
    // location and reproducing the original conflict.
    //
    // When the path is stale we drop BOTH `worktree_path` and the associated
    // branch; downstream fallbacks (`resolve_card_worktree`,
    // `resolve_card_issue_commit_target`, repo-HEAD recovery) will then
    // resolve a fresh execution target from the card's current scope.
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
    let target_repo = context_json
        .as_ref()
        .and_then(|v| json_string_field(v, "target_repo"))
        .or_else(|| {
            result_json
                .as_ref()
                .and_then(|v| json_string_field(v, "target_repo"))
        })
        .map(str::to_string)
        .or_else(|| resolve_card_target_repo_ref_sqlite_test(db, kanban_card_id, None));

    if let Some(reviewed_commit) = reviewed_commit {
        let fallback_repo_dir = target_repo
            .as_deref()
            .and_then(|value| {
                crate::services::platform::shell::resolve_repo_dir_for_target(Some(value))
                    .ok()
                    .flatten()
            })
            .or_else(|| {
                resolve_card_repo_dir(db, kanban_card_id, "recover completed work repo")
                    .ok()
                    .flatten()
            });
        let worktree_path = path.map(str::to_string).or(fallback_repo_dir);
        let issue_branch_hint = load_card_issue_repo(db, kanban_card_id)
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

fn is_work_dispatch_type(dispatch_type: Option<&str>) -> bool {
    matches!(dispatch_type, Some("implementation") | Some("rework"))
}

fn result_has_work_completion_evidence(result: &serde_json::Value) -> bool {
    json_string_field(result, "completed_commit").is_some()
        || json_string_field(result, "assistant_message").is_some()
        || result
            .get("agent_response_present")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
        || json_string_field(result, "work_outcome").is_some()
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
pub(super) fn validate_dispatch_completion_evidence_on_conn(
    conn: &sqlite_test::Connection,
    db: &Db,
    pg_pool: Option<&sqlx::PgPool>,
    dispatch_id: &str,
    result: &serde_json::Value,
) -> Result<()> {
    let (dispatch_type, status): (Option<String>, String) = conn
        .query_row(
            "SELECT dispatch_type, status FROM task_dispatches WHERE id = ?1",
            [dispatch_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .map_err(|e| anyhow::anyhow!("Dispatch lookup error: {e}"))?;

    if !matches!(status.as_str(), "pending" | "dispatched")
        || !is_work_dispatch_type(dispatch_type.as_deref())
    {
        return Ok(());
    }

    if result_has_work_completion_evidence(result)
        || crate::db::session_transcripts::dispatch_has_assistant_response_db(
            Some(db),
            pg_pool,
            dispatch_id,
        )?
    {
        return Ok(());
    }

    let dispatch_label = dispatch_type.as_deref().unwrap_or("work");
    let completion_source = json_string_field(result, "completion_source").unwrap_or("unknown");
    tracing::warn!(
        "[dispatch] rejecting {} completion for {}: no agent execution evidence",
        dispatch_label,
        dispatch_id
    );
    Err(anyhow::anyhow!(
        "Cannot complete {dispatch_label} dispatch {dispatch_id} via {completion_source}: no agent execution evidence (expected assistant response, completed_commit, or explicit work_outcome)"
    ))
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

/// Resolve the canonical worktree for a card's GitHub issue.
///
/// Looks up the card's `github_issue_number`, then searches for an active
/// git worktree whose commits reference that issue.
/// Returns `(worktree_path, worktree_branch, head_commit)` if found.
///
/// Uses the card's `repo_id` + `github_issue_number` to identify the
/// canonical worktree. If the card points at a repo without a configured
/// local mapping, this fails instead of silently falling back to the default
/// repo.
#[cfg(all(test, feature = "legacy-sqlite-tests"))]
pub(crate) fn resolve_card_worktree_sqlite_test(
    db: &Db,
    card_id: &str,
    context: Option<&serde_json::Value>,
) -> Result<Option<(String, String, String)>> {
    let Some((issue_number, _repo_id)) = load_card_issue_repo(db, card_id) else {
        return Ok(None);
    };
    let Some(issue_number) = issue_number else {
        return Ok(None);
    };
    let Some(repo_dir) =
        resolve_card_repo_dir_with_context(db, card_id, context, "resolve worktree repo")?
    else {
        return Ok(None);
    };
    Ok(
        crate::services::platform::find_worktree_for_issue(&repo_dir, issue_number)
            .map(|wt| (wt.path, wt.branch, wt.commit)),
    )
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
fn resolve_card_issue_commit_target(
    db: &Db,
    card_id: &str,
    context: Option<&serde_json::Value>,
) -> Result<Option<DispatchExecutionTarget>> {
    let Some((issue_number, _repo_id)) = load_card_issue_repo(db, card_id) else {
        return Ok(None);
    };
    let Some(issue_number) = issue_number else {
        return Ok(None);
    };
    let Some(repo_dir) =
        resolve_card_repo_dir_with_context(db, card_id, context, "resolve commit target repo")?
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
    Ok(Some(DispatchExecutionTarget {
        reviewed_commit,
        branch,
        worktree_path: Some(repo_dir),
        target_repo: resolve_card_target_repo_ref_sqlite_test(db, card_id, context),
    }))
}

/// #2254 item 1 — SQLite codepath mirror decision.
///
/// `resolve_pr_tracking_review_target_pg` (PG) is the sole production
/// rereview-target resolver. The SQLite-shaped helpers below
/// (`resolve_card_issue_commit_target`, `resolve_repo_head_fallback_target`,
/// `refresh_review_target_worktree`, `build_review_context_sqlite_test`) are
/// all gated by `#[cfg(all(test, feature = "legacy-sqlite-tests"))]` and only
/// exist to keep the legacy rusqlite parity tests compiling. No production
/// deployment hits this branch — the dispatch service is PG-only — so we
/// intentionally do NOT mirror the round-1/2 improvements (symmetric clear,
/// spawn_blocking git check, Rust-side JSON parsing) here. If you ever
/// resurrect SQLite for production review dispatches, port
/// `resolve_pr_tracking_review_target_pg`, `clean_exact_review_worktree_path`,
/// and `probe_clean_worktree_with_transient_retry` over before re-enabling
/// the runtime path.
#[cfg(all(test, feature = "legacy-sqlite-tests"))]
fn resolve_repo_head_fallback_target(
    db: &Db,
    kanban_card_id: &str,
    context: Option<&serde_json::Value>,
) -> Result<Option<DispatchExecutionTarget>> {
    let Some(repo_dir) = resolve_card_repo_dir_with_context(
        db,
        kanban_card_id,
        context,
        "resolve repo-root HEAD fallback target",
    )?
    else {
        return Ok(None);
    };

    let dirty_paths =
        crate::services::platform::shell::git_tracked_change_paths(&repo_dir).unwrap_or_default();
    if !dirty_paths.is_empty() {
        let sample = dirty_paths
            .iter()
            .take(3)
            .cloned()
            .collect::<Vec<_>>()
            .join(", ");
        anyhow::bail!(
            "Cannot create review dispatch for card {}: repo-root HEAD fallback is unsafe while tracked changes exist{}",
            kanban_card_id,
            if sample.is_empty() {
                String::new()
            } else if dirty_paths.len() > 3 {
                format!(" ({sample}, +{} more)", dirty_paths.len() - 3)
            } else {
                format!(" ({sample})")
            }
        );
    }

    Ok(execution_target_from_dir(&repo_dir).map(|mut target| {
        target.target_repo = resolve_card_target_repo_ref_sqlite_test(db, kanban_card_id, context);
        target
    }))
}

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

/// Build the context JSON string for a review dispatch.
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
#[cfg(all(test, feature = "legacy-sqlite-tests"))]
pub(super) fn build_review_context_sqlite_test(
    db: &Db,
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
    let mut ctx_val = dispatch_context_with_session_strategy("review", context);

    // #761: Strip untrusted review-target fields before any downstream code
    // consumes them. The trust decision is out-of-band (the `trust` parameter
    // on this function's signature, not a JSON field), so a malicious or buggy
    // POST /api/dispatches body cannot opt out of stripping. Any legacy
    // `_trusted_review_target` key in the payload is also removed so it
    // cannot leak into the persisted dispatch context and mislead future
    // readers into thinking it carries meaning.
    if let Some(obj) = ctx_val.as_object_mut() {
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
    }

    let target_repo = resolve_card_target_repo_ref_sqlite_test(db, kanban_card_id, Some(&ctx_val));
    if let Some(obj) = ctx_val.as_object_mut() {
        if let Some(target_repo) = target_repo.as_deref() {
            obj.entry("target_repo".to_string())
                .or_insert_with(|| json!(target_repo));
        }
    }
    let ctx_snapshot = ctx_val.clone();
    // #655: Noop verification reviews don't need a commit target — they verify the
    // noop justification, not code changes. Skip the entire reviewed_commit resolution
    // to avoid repo-root dirty-check failures on noop completions.
    let is_noop_verification =
        ctx_val.get("review_mode").and_then(|v| v.as_str()) == Some("noop_verification");
    let card_issue_number =
        load_card_issue_repo(db, kanban_card_id).and_then(|(issue_number, _)| issue_number);
    if let Some(obj) = ctx_val.as_object_mut() {
        if !is_noop_verification && !obj.contains_key("reviewed_commit") {
            let latest_work_target = latest_completed_work_dispatch_target(db, kanban_card_id);
            let validated_work_target = if let Some(target) = latest_work_target.as_ref() {
                let valid = card_issue_number.is_none()
                    || commit_belongs_to_card_issue(
                        db,
                        kanban_card_id,
                        &target.reviewed_commit,
                        target.target_repo.as_deref().or(target_repo.as_deref()),
                    );
                if !valid {
                    tracing::warn!(
                        "[dispatch] Review dispatch for card {}: work target commit {} doesn't match card issue — skipping to next fallback",
                        kanban_card_id,
                        &target.reviewed_commit[..8.min(target.reviewed_commit.len())]
                    );
                }
                if valid {
                    // #682: Always refresh to catch stale worktree_path even for
                    // issue-less cards. refresh_review_target_worktree tries
                    // resolve_card_worktree first (needs issue_number — returns
                    // None here) and falls back to the card's repo_dir when the
                    // reviewed_commit still lives there. Prior code returned the
                    // recorded target unchanged when issue_number was None, which
                    // meant a stale worktree_path propagated into the dispatch
                    // context and failed `Path::exists()` at review execution.
                    refresh_review_target_worktree(db, kanban_card_id, &ctx_snapshot, target)?
                } else {
                    None
                }
            } else {
                None
            };
            if let Some(target) = validated_work_target {
                apply_review_target_context(&target, obj);
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

                // #762 (A): if the historical work target was recorded against
                // an EXTERNAL `target_repo` that differs from the card's
                // canonical repo, and refresh failed, the card-scoped
                // fallbacks below (`resolve_card_worktree`,
                // `resolve_card_issue_commit_target`, repo-HEAD fallback) will
                // silently redirect the reviewer to unrelated code in the
                // card's default repo. Fail closed instead.
                //
                // Exception: when the caller explicitly pinned `target_repo`
                // on the invocation context, `ctx_snapshot` already carries
                // the correct repo scope — `resolve_card_worktree` et al. will
                // honor it and no silent redirect can happen. We only fail
                // closed when the caller provided no override.
                let card_repo_id =
                    load_card_issue_repo(db, kanban_card_id).and_then(|(_, repo_id)| repo_id);
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

                if let Some(external_repo) = historical_external_repo_unrecoverable {
                    apply_review_target_warning(
                        obj,
                        "external_target_repo_unrecoverable",
                        "리뷰 대상 커밋을 원래 외부 target_repo에서 복구할 수 없습니다. 카드 기본 레포로 폴백하면 무관한 코드가 리뷰되므로 중단합니다.",
                    );
                    // Preserve the historical target_repo so downstream
                    // consumers (prompt builder, bootstrap) at least know
                    // which repo the reviewer should have been pointed at.
                    // Overwrite any card-scoped target_repo that may have
                    // been pre-injected by resolve_card_target_repo_ref —
                    // the failed external reference is the meaningful signal
                    // here, not the card's default repo.
                    obj.insert("target_repo".to_string(), json!(external_repo.clone()));
                    tracing::warn!(
                        "[dispatch] Review dispatch for card {}: historical external target_repo '{}' is unrecoverable — suppressing card-scoped fallback",
                        kanban_card_id,
                        external_repo
                    );
                } else if let Some((ref wt_path, ref wt_branch, ref wt_commit)) =
                    resolve_card_worktree_sqlite_test(db, kanban_card_id, Some(&ctx_snapshot))?
                {
                    apply_review_target_context(
                        &DispatchExecutionTarget {
                            reviewed_commit: wt_commit.clone(),
                            branch: Some(wt_branch.clone()),
                            worktree_path: Some(wt_path.clone()),
                            target_repo: target_repo.clone(),
                        },
                        obj,
                    );
                    tracing::info!(
                        "[dispatch] Review dispatch for card {}: using canonical worktree branch '{}' (commit {}, path: {})",
                        kanban_card_id,
                        wt_branch,
                        &wt_commit[..8.min(wt_commit.len())],
                        wt_path
                    );
                } else if let Some(target) =
                    resolve_card_issue_commit_target(db, kanban_card_id, Some(&ctx_snapshot))?
                {
                    apply_review_target_context(&target, obj);
                    tracing::info!(
                        "[dispatch] Review dispatch for card {}: recovered issue commit target ({})",
                        kanban_card_id,
                        &target.reviewed_commit[..8.min(target.reviewed_commit.len())]
                    );
                } else {
                    if latest_work_target.is_some() && validated_work_target.is_none() {
                        apply_review_target_warning(
                            obj,
                            "latest_work_target_issue_mismatch",
                            "브랜치 정보 없음 — 직접 확인 필요. 최근 완료 작업 커밋이 현재 카드 이슈와 일치하지 않아 repo HEAD 폴백을 생략했습니다.",
                        );
                        tracing::warn!(
                            "[dispatch] Review dispatch for card {}: latest work target was rejected, downstream worktree recovery failed, and repo HEAD fallback is disabled",
                            kanban_card_id
                        );
                    } else if let Some(target) =
                        resolve_repo_head_fallback_target(db, kanban_card_id, Some(&ctx_snapshot))?
                    {
                        apply_review_target_context(&target, obj);
                        tracing::info!(
                            "[dispatch] Review dispatch for card {}: no worktree, using repo HEAD ({})",
                            kanban_card_id,
                            &target.reviewed_commit[..8.min(target.reviewed_commit.len())]
                        );
                    }
                }
            }
        }

        inject_review_merge_base_context(obj);
        inject_review_quality_context(obj);
        inject_review_dispatch_identifiers_sqlite_test(db, kanban_card_id, "review", obj);

        if let Ok(conn) = db.separate_conn() {
            if let Ok(Some(bindings)) = load_agent_channel_bindings(&conn, to_agent_id) {
                apply_review_counter_model_provider_context(obj, &bindings);
            }
        }
    }
    Ok(serde_json::to_string(&ctx_val)?)
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
        if std::path::Path::new(recorded).is_dir()
            && worktree_head_matches_commit(recorded, &target.reviewed_commit)
        {
            if let Some(clean_path) = probe_clean_worktree_with_transient_retry(
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
            // Recorded path is HEAD-matching but unstable / dirty. Don't
            // accept it; let the issue-worktree / repo-dir fallbacks run
            // (they will themselves probe). If they also fail we emit a
            // commit-only target below.
        }
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
        if worktree_head_matches_commit(&wt_path, &target.reviewed_commit) {
            // Codex round-4 followup: gate this worktree_path emission
            // behind the same stable-clean probe used by the pr_tracking
            // resolver. HEAD-match without a cleanliness check would let
            // a parallel writer's dirty state leak into the review.
            let clean_wt = probe_clean_worktree_with_transient_retry(
                card_id,
                "refresh active worktree",
                &wt_path,
                &target.reviewed_commit,
            )
            .await;
            let branch = resolve_review_target_branch_pg(
                pool,
                card_id,
                &wt_path,
                &target.reviewed_commit,
                target.branch.as_deref().or(Some(wt_branch.as_str())),
            )
            .await
            .or(Some(wt_branch));
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
        if worktree_head_matches_commit(&repo_dir, &target.reviewed_commit) {
            // Codex round-4 followup: probe before emitting the repo dir
            // as worktree_path.
            let clean_repo = probe_clean_worktree_with_transient_retry(
                card_id,
                "refresh fallback repo_dir",
                &repo_dir,
                &target.reviewed_commit,
            )
            .await;
            let branch = resolve_review_target_branch_pg(
                pool,
                card_id,
                &repo_dir,
                &target.reviewed_commit,
                target.branch.as_deref(),
            )
            .await;
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
    Ok(Some(DispatchExecutionTarget {
        reviewed_commit,
        branch,
        worktree_path: Some(repo_dir),
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
    match stable_clean_probe(card_id, source, path, commit).await {
        ReviewWorktreeProbe::Clean(p) => Some(p),
        ReviewWorktreeProbe::DirtyTransient => {
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
                    Some(p)
                }
                _ => {
                    tracing::warn!(
                        "[dispatch] Review dispatch for card {}: {} worktree '{}' failed stability re-check after transient-dirty retry — falling back to commit-only review (commit {})",
                        card_id,
                        source,
                        path,
                        &commit[..8.min(commit.len())]
                    );
                    None
                }
            }
        }
        // NotMatching and GitFailure: fail-closed, no retry. A path that
        // doesn't hold the reviewed commit will not start holding it, and
        // a git-status failure (#2254 bonus) is an integrity signal we
        // must surface rather than mask.
        ReviewWorktreeProbe::NotMatching | ReviewWorktreeProbe::GitFailure(_) => None,
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

    // #2237 item 3 (extended per Codex review): the repo-HEAD fallback is
    // also reached from the async dispatch creation path and must not
    // stall the tokio worker on `git status` for large monorepos. Share
    // the helper used by clean_exact_review_worktree_path so both async
    // resolver branches behave identically.
    //
    // #2254 bonus: fail-closed on git status failure. Previously
    // `unwrap_or_default()` silently treated an index-lock or permission
    // error as a clean repo, which would then emit the repo root as the
    // review worktree even when the on-disk state was potentially dirty.
    let dirty_paths = git_tracked_change_paths_strict_async(&repo_dir)
        .await
        .map_err(|err| {
            anyhow::anyhow!(
                "Cannot create review dispatch for card {}: repo-root HEAD fallback is unsafe — git status check failed for '{}': {}",
                kanban_card_id,
                repo_dir,
                err
            )
        })?;
    if !dirty_paths.is_empty() {
        let sample = dirty_paths
            .iter()
            .take(3)
            .cloned()
            .collect::<Vec<_>>()
            .join(", ");
        anyhow::bail!(
            "Cannot create review dispatch for card {}: repo-root HEAD fallback is unsafe while tracked changes exist{}",
            kanban_card_id,
            if sample.is_empty() {
                String::new()
            } else if dirty_paths.len() > 3 {
                format!(" ({sample}, +{} more)", dirty_paths.len() - 3)
            } else {
                format!(" ({sample})")
            }
        );
    }

    let Some(mut target) = execution_target_from_dir(&repo_dir) else {
        return Ok(None);
    };
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
    let card_issue_repo = load_card_issue_repo_pg(pool, kanban_card_id).await;
    let card_issue_number = card_issue_repo
        .as_ref()
        .and_then(|(issue_number, _)| *issue_number);

    if !is_noop_verification && !obj.contains_key("reviewed_commit") {
        let latest_work_target =
            latest_completed_work_dispatch_target_pg(pool, kanban_card_id).await;
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

            let pr_tracking_target = if is_rereview_dispatch {
                resolve_pr_tracking_review_target_pg(pool, kanban_card_id, Some(&ctx_snapshot))
                    .await?
            } else {
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
            } else if let Some((wt_path, wt_branch, wt_commit)) =
                resolve_card_worktree(pool, kanban_card_id, Some(&ctx_snapshot)).await?
            {
                let reviewed_commit = wt_commit.clone();
                apply_review_target_context(
                    &DispatchExecutionTarget {
                        reviewed_commit: wt_commit,
                        branch: Some(wt_branch.clone()),
                        worktree_path: Some(wt_path.clone()),
                        target_repo: target_repo.clone(),
                    },
                    &mut obj,
                );
                tracing::info!(
                    "[dispatch] Review dispatch for card {}: using canonical worktree branch '{}' (commit {}, path: {})",
                    kanban_card_id,
                    wt_branch,
                    &reviewed_commit[..8.min(reviewed_commit.len())],
                    wt_path
                );
            } else if let Some(target) =
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
            } else if let Some(target) =
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
        // A single-reference subject where N is the only `#<digits>` token
        // is accepted regardless of position (e.g. trailing "refs #N").
        assert!(commit_subject_references_issue(
            "fix bug — refs #1765",
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

        // Codex round-2 finding: duplicate references to *the same* issue
        // are not ambiguous. The previous occurrence-count guard rejected
        // these and could fail a valid reviewed_commit lookup as stale.
        assert!(commit_subject_references_issue("fix #523, refs #523", 523));
        assert!(commit_subject_references_issue(
            "wip: #523 part 1 — followup refs #523",
            523
        ));
        assert!(commit_subject_references_issue(
            "feat: foo #523 #523 #523 done",
            523
        ));
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

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
mod tests {
    use super::*;
    use crate::db::Db;
    use crate::engine::PolicyEngine;
    use serde_json::json;
    use std::io::{self, Write};
    use std::sync::MutexGuard;
    use std::sync::{Arc, Mutex};

    struct RepoDirOverride {
        _lock: MutexGuard<'static, ()>,
        previous: Option<String>,
    }

    impl RepoDirOverride {
        fn new(path: &str) -> Self {
            let lock = crate::services::discord::runtime_store::lock_test_env();
            let previous = std::env::var("AGENTDESK_REPO_DIR").ok();
            unsafe { std::env::set_var("AGENTDESK_REPO_DIR", path) };
            Self {
                _lock: lock,
                previous,
            }
        }
    }

    impl Drop for RepoDirOverride {
        fn drop(&mut self) {
            if let Some(value) = self.previous.as_deref() {
                unsafe { std::env::set_var("AGENTDESK_REPO_DIR", value) };
            } else {
                unsafe { std::env::remove_var("AGENTDESK_REPO_DIR") };
            }
        }
    }

    fn test_db() -> Db {
        let db = crate::db::test_db();
        let conn = db.separate_conn().unwrap();
        conn.execute_batch(
            "INSERT OR IGNORE INTO agents (id, name, discord_channel_id, discord_channel_alt)
                 VALUES ('agent-1', 'Agent 1', '111', '222');",
        )
        .unwrap();
        drop(conn);
        db
    }

    fn test_engine(db: &Db) -> PolicyEngine {
        let config = crate::config::Config::default();
        PolicyEngine::new_with_legacy_db(&config, db.clone()).unwrap()
    }

    fn bindings_with_provider(provider: &str) -> AgentChannelBindings {
        AgentChannelBindings {
            provider: Some(provider.to_string()),
            discord_channel_id: Some("111".to_string()),
            discord_channel_alt: Some("222".to_string()),
            discord_channel_cc: Some("111".to_string()),
            discord_channel_cdx: Some("222".to_string()),
        }
    }

    #[test]
    fn review_counter_model_provider_uses_explicit_implementer_symmetrically() {
        let cases = [("codex", "claude", "codex"), ("claude", "codex", "claude")];

        for (main_provider, implementer_provider, expected_target) in cases {
            let bindings = bindings_with_provider(main_provider);
            let context = json!({ "implementer_provider": implementer_provider });

            let resolution =
                resolve_review_counter_model_provider(&bindings, context.as_object().unwrap())
                    .expect("review counter-model provider resolution");

            assert_eq!(resolution.source_provider.as_str(), implementer_provider);
            assert_eq!(resolution.target_provider.as_str(), expected_target);
            assert!(
                resolution
                    .reason
                    .starts_with("explicit_implementer_provider:")
            );
        }
    }

    #[test]
    fn review_counter_model_provider_uses_explicit_from_provider_symmetrically() {
        let cases = [("codex", "claude", "codex"), ("claude", "codex", "claude")];

        for (main_provider, from_provider, expected_target) in cases {
            let bindings = bindings_with_provider(main_provider);
            let context = json!({ "from_provider": from_provider });

            let resolution =
                resolve_review_counter_model_provider(&bindings, context.as_object().unwrap())
                    .expect("review counter-model provider resolution");

            assert_eq!(resolution.source_provider.as_str(), from_provider);
            assert_eq!(resolution.target_provider.as_str(), expected_target);
            assert!(resolution.reason.starts_with("explicit_from_provider:"));
        }
    }

    #[test]
    fn review_counter_model_provider_implementer_wins_over_stale_from_provider() {
        let bindings = bindings_with_provider("codex");
        let context = json!({
            "implementer_provider": "claude",
            "from_provider": "codex"
        });

        let resolution =
            resolve_review_counter_model_provider(&bindings, context.as_object().unwrap())
                .expect("review counter-model provider resolution");

        assert_eq!(resolution.source_provider.as_str(), "claude");
        assert_eq!(resolution.target_provider.as_str(), "codex");
        assert!(
            resolution
                .reason
                .starts_with("explicit_implementer_provider:")
        );
    }

    #[test]
    fn review_counter_model_provider_falls_back_to_agent_main_provider() {
        let cases = [("codex", "claude"), ("claude", "codex")];

        for (main_provider, expected_target) in cases {
            let bindings = bindings_with_provider(main_provider);
            let context = json!({});

            let resolution =
                resolve_review_counter_model_provider(&bindings, context.as_object().unwrap())
                    .expect("review counter-model provider resolution");

            assert_eq!(resolution.source_provider.as_str(), main_provider);
            assert_eq!(resolution.target_provider.as_str(), expected_target);
            assert!(resolution.reason.starts_with("agent_main_provider:"));
        }
    }

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

    fn setup_test_repo() -> (tempfile::TempDir, RepoDirOverride) {
        let repo = init_test_repo();
        let repo_dir = repo.path().to_str().unwrap();
        let repo_override = RepoDirOverride::new(repo_dir);
        (repo, repo_override)
    }

    fn git_commit(dir: &str, message: &str) -> String {
        run_git(dir, &["commit", "--allow-empty", "-m", message]);
        crate::services::platform::git_head_commit(dir).unwrap()
    }

    fn canonicalize_path(path: &str) -> String {
        std::fs::canonicalize(path)
            .unwrap()
            .to_string_lossy()
            .into_owned()
    }

    #[derive(Clone)]
    struct TestLogWriter {
        buffer: Arc<Mutex<Vec<u8>>>,
    }

    impl Write for TestLogWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.buffer.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn capture_logs<T>(run: impl FnOnce() -> T) -> (T, String) {
        let buffer = Arc::new(Mutex::new(Vec::new()));
        let log_buffer = buffer.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::WARN)
            .with_ansi(false)
            .without_time()
            .with_writer(move || TestLogWriter {
                buffer: log_buffer.clone(),
            })
            .finish();

        let result = tracing::subscriber::with_default(subscriber, run);
        let captured = buffer.lock().unwrap().clone();
        (result, String::from_utf8_lossy(&captured).to_string())
    }

    fn seed_card(db: &Db, card_id: &str, issue_number: i64, status: &str) {
        let conn = db.separate_conn().unwrap();
        conn.execute(
            "INSERT INTO kanban_cards (
                id, title, status, github_issue_number, created_at, updated_at
             ) VALUES (
                ?1, 'Test Card', ?2, ?3, datetime('now'), datetime('now')
             )",
            sqlite_test::params![card_id, status, issue_number],
        )
        .unwrap();
    }

    #[test]
    fn warn_and_flatten_pg_optional_logs_lookup_errors() {
        let (value, logs) = capture_logs(|| {
            warn_and_flatten_pg_optional::<(Option<i64>, Option<String>)>(
                Err(sqlx::Error::ColumnNotFound(
                    "github_issue_number".to_string(),
                )),
                "card-1",
                "failed to load postgres card dispatch info",
            )
        });

        assert!(value.is_none());
        assert!(logs.contains("failed to load postgres card dispatch info"));
        assert!(logs.contains("card_id=card-1"));
    }

    fn set_card_repo_id(db: &Db, card_id: &str, repo_id: &str) {
        let conn = db.separate_conn().unwrap();
        conn.execute(
            "UPDATE kanban_cards SET repo_id = ?1 WHERE id = ?2",
            sqlite_test::params![repo_id, card_id],
        )
        .unwrap();
    }

    #[test]
    fn dispatch_context_worktree_target_ignores_stale_explicit_path() {
        let context = json!({
            "worktree_path": "/tmp/agentdesk-stale-dispatch-context-worktree",
            "worktree_branch": "wt/stale-693"
        });

        let target = dispatch_context_worktree_target(&context).unwrap();

        assert!(
            target.is_none(),
            "stale explicit worktree_path must fall through to later recovery"
        );
    }

    #[test]
    fn create_dispatch_falls_back_after_stale_explicit_worktree_path() {
        let db = test_db();
        let engine = test_engine(&db);
        seed_card(&db, "card-dispatch-stale-explicit", 693, "ready");

        let (repo, _repo_override) = setup_test_repo();
        let repo_dir = repo.path().to_str().unwrap();
        let stale_wt_dir = repo.path().join("wt-693-stale");
        let stale_wt_path = stale_wt_dir.to_str().unwrap();
        run_git(
            repo_dir,
            &["worktree", "add", "-b", "wt/693-stale", stale_wt_path],
        );
        let reviewed_commit = git_commit(stale_wt_path, "fix: stale explicit worktree (#693)");
        run_git(repo_dir, &["worktree", "remove", "--force", stale_wt_path]);
        run_git(repo_dir, &["branch", "-D", "wt/693-stale"]);

        let live_wt_dir = repo.path().join("wt-693-live");
        let live_wt_path = live_wt_dir.to_str().unwrap();
        run_git(repo_dir, &["branch", "wt/693-live", &reviewed_commit]);
        run_git(repo_dir, &["worktree", "add", live_wt_path, "wt/693-live"]);

        let dispatch = crate::dispatch::create_dispatch(
            &db,
            &engine,
            "card-dispatch-stale-explicit",
            "agent-1",
            "implementation",
            "Recover stale explicit worktree",
            &json!({
                "worktree_path": stale_wt_path
            }),
        )
        .expect("stale explicit worktree_path should not block dispatch creation");

        let context = &dispatch["context"];
        assert_eq!(
            canonicalize_path(context["worktree_path"].as_str().unwrap()),
            canonicalize_path(live_wt_path)
        );
        assert_eq!(context["worktree_branch"], "wt/693-live");
    }

    #[test]
    fn refresh_review_target_worktree_recovers_active_issue_worktree() {
        let db = test_db();
        seed_card(&db, "card-review-active-refresh", 701, "review");

        let (repo, _repo_override) = setup_test_repo();
        let repo_dir = repo.path().to_str().unwrap();
        let stale_wt_dir = repo.path().join("wt-701-stale");
        let stale_wt_path = stale_wt_dir.to_str().unwrap();
        run_git(
            repo_dir,
            &["worktree", "add", "-b", "wt/701-stale", stale_wt_path],
        );
        let reviewed_commit = git_commit(stale_wt_path, "fix: review fallback target (#701)");
        run_git(repo_dir, &["worktree", "remove", "--force", stale_wt_path]);
        run_git(repo_dir, &["branch", "-D", "wt/701-stale"]);

        let live_wt_dir = repo.path().join("wt-701-live");
        let live_wt_path = live_wt_dir.to_str().unwrap();
        run_git(repo_dir, &["branch", "wt/701-live", &reviewed_commit]);
        run_git(repo_dir, &["worktree", "add", live_wt_path, "wt/701-live"]);

        let refreshed = refresh_review_target_worktree(
            &db,
            "card-review-active-refresh",
            &json!({}),
            &DispatchExecutionTarget {
                reviewed_commit: reviewed_commit.clone(),
                branch: Some("wt/701-stale".to_string()),
                worktree_path: Some(stale_wt_path.to_string()),
                target_repo: None,
            },
        )
        .unwrap()
        .expect("active issue worktree should replace stale path");

        assert_eq!(refreshed.reviewed_commit, reviewed_commit);
        assert_eq!(
            refreshed.worktree_path.as_deref().map(canonicalize_path),
            Some(canonicalize_path(live_wt_path))
        );
        assert_eq!(refreshed.branch.as_deref(), Some("wt/701-live"));
    }

    #[test]
    fn refresh_review_target_worktree_falls_back_to_repo_dir() {
        let db = test_db();
        seed_card(&db, "card-review-repo-refresh", 702, "review");

        let (repo, _repo_override) = setup_test_repo();
        let repo_dir = repo.path().to_str().unwrap();
        let reviewed_commit = git_commit(repo_dir, "fix: repo fallback target (#702)");
        let stale_wt_path = repo.path().join("wt-702-missing");

        let refreshed = refresh_review_target_worktree(
            &db,
            "card-review-repo-refresh",
            &json!({}),
            &DispatchExecutionTarget {
                reviewed_commit: reviewed_commit.clone(),
                branch: Some("wt/702-missing".to_string()),
                worktree_path: Some(stale_wt_path.to_string_lossy().into_owned()),
                target_repo: None,
            },
        )
        .unwrap()
        .expect("repo dir should be used when no active issue worktree exists");

        assert_eq!(refreshed.reviewed_commit, reviewed_commit);
        assert_eq!(
            refreshed.worktree_path.as_deref().map(canonicalize_path),
            Some(canonicalize_path(repo_dir))
        );
        assert_eq!(refreshed.branch.as_deref(), Some("main"));
    }

    #[test]
    fn refresh_review_target_worktree_returns_none_when_no_fallback_contains_commit() {
        let db = test_db();
        seed_card(&db, "card-review-refresh-miss", 703, "review");

        let (repo, _repo_override) = setup_test_repo();
        let stale_wt_path = repo.path().join("wt-703-missing");
        let missing_commit = "1111111111111111111111111111111111111111".to_string();

        let refreshed = refresh_review_target_worktree(
            &db,
            "card-review-refresh-miss",
            &json!({}),
            &DispatchExecutionTarget {
                reviewed_commit: missing_commit,
                branch: Some("wt/703-missing".to_string()),
                worktree_path: Some(stale_wt_path.to_string_lossy().into_owned()),
                target_repo: None,
            },
        )
        .unwrap();

        assert!(
            refreshed.is_none(),
            "refresh must report failure when neither worktree nor repo dir has the commit"
        );
    }

    #[test]
    fn review_context_injects_review_identifiers() {
        let db = test_db();
        seed_card(&db, "card-review-identifiers", 692, "review");

        let (repo, _repo_override) = setup_test_repo();
        let repo_dir = repo.path().to_str().unwrap();
        let reviewed_commit = git_commit(repo_dir, "fix: review identifiers (#692)");
        set_card_repo_id(&db, "card-review-identifiers", repo_dir);

        let conn = db.separate_conn().unwrap();
        conn.execute(
            "INSERT INTO pr_tracking (
                card_id, repo_id, worktree_path, branch, pr_number, head_sha, state, created_at, updated_at
             ) VALUES (
                'card-review-identifiers', ?1, ?2, 'wt/692-review', 901, ?3, 'review',
                datetime('now'), datetime('now')
             )",
            sqlite_test::params![repo_dir, repo_dir, reviewed_commit],
        )
        .unwrap();
        drop(conn);

        let context = build_review_context_sqlite_test(
            &db,
            "card-review-identifiers",
            "agent-1",
            &json!({
                "worktree_path": repo_dir,
                "branch": "wt/692-review",
                "reviewed_commit": reviewed_commit,
            }),
            ReviewTargetTrust::Untrusted,
            TargetRepoSource::CardScopeDefault,
        )
        .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&context).unwrap();

        assert_eq!(
            canonicalize_path(parsed["repo"].as_str().unwrap()),
            canonicalize_path(repo_dir)
        );
        assert_eq!(parsed["issue_number"], 692);
        assert_eq!(parsed["pr_number"], 901);
        assert_eq!(parsed["reviewed_commit"], reviewed_commit);
        assert_eq!(parsed["verdict_endpoint"], "POST /api/reviews/verdict");
    }

    /// #800: Verify that a recorded `worktree_path` pointing at a now-missing
    /// directory does NOT survive into the resolved
    /// `latest_completed_work_dispatch_target`. Without this guard a reopened
    /// done card silently re-points the new dispatch at the orphaned worktree
    /// and the agent restarts work in a broken location.
    #[test]
    fn latest_completed_work_dispatch_target_drops_stale_worktree_path() {
        let db = test_db();
        seed_card(&db, "card-stale-completed-wt", 800, "ready");

        // Materialize a real directory, capture its path, then remove the
        // directory so the recorded path becomes stale on disk while the
        // dispatch row keeps pointing at it.
        let stale_dir = tempfile::tempdir().unwrap();
        let stale_path = stale_dir.path().to_path_buf();
        let stale_path_str = stale_path.to_str().unwrap().to_string();
        let stale_branch = "wt/800-stale".to_string();
        drop(stale_dir);
        assert!(
            !stale_path.exists(),
            "tempdir must be removed before the test runs to simulate a stale wt"
        );

        let conn = db.separate_conn().unwrap();
        conn.execute(
            "INSERT INTO task_dispatches (
                id, kanban_card_id, to_agent_id, dispatch_type, status, title, context, result, created_at, updated_at
             ) VALUES (
                'dispatch-800-stale', 'card-stale-completed-wt', 'agent-1', 'implementation', 'completed',
                'Stale wt completion', ?1, ?2, datetime('now'), datetime('now')
             )",
            sqlite_test::params![
                serde_json::json!({
                    "worktree_path": stale_path_str,
                    "worktree_branch": stale_branch,
                })
                .to_string(),
                serde_json::json!({
                    "completed_worktree_path": stale_path_str,
                    "completed_branch": stale_branch,
                    // Intentionally no `completed_commit` so the function
                    // exits via the "trusted_path" branch (lines after the
                    // path-validation guard) and we can observe that the
                    // stale path was filtered out instead of returned.
                })
                .to_string(),
            ],
        )
        .unwrap();
        drop(conn);

        let target = latest_completed_work_dispatch_target(&db, "card-stale-completed-wt");
        assert!(
            target.is_none(),
            "stale worktree path with no recoverable commit must yield no dispatch target, got {:?}",
            target
        );
    }

    /// #800 companion: when a `completed_commit` IS recorded alongside a stale
    /// `worktree_path`, the function falls back through `fallback_repo_dir`.
    /// We assert that `worktree_path` on the returned target is NOT the stale
    /// recorded value — proving the path-validation guard fired.
    #[test]
    fn latest_completed_work_dispatch_target_does_not_reuse_stale_path_with_commit() {
        let db = test_db();
        seed_card(&db, "card-stale-completed-with-commit", 800, "ready");

        let stale_dir = tempfile::tempdir().unwrap();
        let stale_path_str = stale_dir.path().to_str().unwrap().to_string();
        drop(stale_dir);

        // Use a real repo for the fallback so resolve_card_target_repo_ref/
        // fallback_repo_dir yields *something*, otherwise the function returns
        // a target with worktree_path: None which still satisfies "no stale
        // reuse" but for the wrong reason.
        let (repo, _repo_override) = setup_test_repo();
        let repo_dir = repo.path().to_str().unwrap().to_string();
        let completed_commit = crate::services::platform::git_head_commit(&repo_dir).unwrap();
        set_card_repo_id(&db, "card-stale-completed-with-commit", &repo_dir);

        let conn = db.separate_conn().unwrap();
        conn.execute(
            "INSERT INTO task_dispatches (
                id, kanban_card_id, to_agent_id, dispatch_type, status, title, context, result, created_at, updated_at
             ) VALUES (
                'dispatch-800-stale-commit', 'card-stale-completed-with-commit', 'agent-1', 'implementation', 'completed',
                'Stale wt completion (with commit)', ?1, ?2, datetime('now'), datetime('now')
             )",
            sqlite_test::params![
                serde_json::json!({}).to_string(),
                serde_json::json!({
                    "completed_worktree_path": stale_path_str,
                    "completed_branch": "wt/800-stale-commit",
                    "completed_commit": completed_commit,
                })
                .to_string(),
            ],
        )
        .unwrap();
        drop(conn);

        let target = latest_completed_work_dispatch_target(
            &db,
            "card-stale-completed-with-commit",
        )
        .expect("target must exist when completed_commit is present and a fallback repo dir is resolvable");

        assert_eq!(target.reviewed_commit, completed_commit);
        assert_ne!(
            target.worktree_path.as_deref(),
            Some(stale_path_str.as_str()),
            "stale worktree_path must not be reused even when commit recovery succeeds"
        );
    }

    // ── #847 PG-native helper tests ─────────────────────────────────────────
    //
    // Each PG test no-ops when a local PostgreSQL is unavailable
    // (`TestPostgresDb::create` returns `None` on connect failure) so the
    // suite stays green on workstations without a running PG. Pattern mirrors
    // `src/db/session_transcripts.rs::tests`.

    struct TestPostgresDb {
        admin_url: String,
        database_name: String,
        database_url: String,
    }

    impl TestPostgresDb {
        async fn create() -> Option<Self> {
            let admin_url = postgres_admin_url();
            let database_name = format!("agentdesk_pg_{}", uuid::Uuid::new_v4().simple());
            let database_url = format!("{}/{}", postgres_base_database_url(), database_name);
            let admin_pool = match sqlx::PgPool::connect(&admin_url).await {
                Ok(pool) => pool,
                Err(error) => {
                    eprintln!(
                        "skipping postgres dispatch_context test: admin connect failed: {error}"
                    );
                    return None;
                }
            };
            if let Err(error) = sqlx::query(&format!("CREATE DATABASE \"{database_name}\""))
                .execute(&admin_pool)
                .await
            {
                eprintln!(
                    "skipping postgres dispatch_context test: create database failed: {error}"
                );
                admin_pool.close().await;
                return None;
            }
            admin_pool.close().await;
            Some(Self {
                admin_url,
                database_name,
                database_url,
            })
        }

        async fn migrate(&self) -> Option<sqlx::PgPool> {
            let pool = match sqlx::PgPool::connect(&self.database_url).await {
                Ok(pool) => pool,
                Err(error) => {
                    eprintln!(
                        "skipping postgres dispatch_context test: db connect failed: {error}"
                    );
                    return None;
                }
            };
            if let Err(error) = crate::db::postgres::migrate(&pool).await {
                eprintln!("skipping postgres dispatch_context test: migrate failed: {error}");
                pool.close().await;
                return None;
            }
            Some(pool)
        }

        async fn drop(self) {
            let Ok(admin_pool) = sqlx::PgPool::connect(&self.admin_url).await else {
                return;
            };
            let _ = sqlx::query(
                "SELECT pg_terminate_backend(pid)
                 FROM pg_stat_activity
                 WHERE datname = $1
                   AND pid <> pg_backend_pid()",
            )
            .bind(&self.database_name)
            .execute(&admin_pool)
            .await;
            let _ = sqlx::query(&format!(
                "DROP DATABASE IF EXISTS \"{}\"",
                self.database_name
            ))
            .execute(&admin_pool)
            .await;
            admin_pool.close().await;
        }
    }

    fn postgres_base_database_url() -> String {
        if let Ok(base) = std::env::var("POSTGRES_TEST_DATABASE_URL_BASE") {
            let trimmed = base.trim();
            if !trimmed.is_empty() {
                return trimmed.trim_end_matches('/').to_string();
            }
        }
        let user = std::env::var("PGUSER")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .or_else(|| {
                std::env::var("USER")
                    .ok()
                    .filter(|value| !value.trim().is_empty())
            })
            .unwrap_or_else(|| "postgres".to_string());
        let password = std::env::var("PGPASSWORD")
            .ok()
            .filter(|value| !value.trim().is_empty());
        let host = std::env::var("PGHOST")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "127.0.0.1".to_string());
        let port = std::env::var("PGPORT")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "5432".to_string());

        match password {
            Some(password) => format!("postgres://{user}:{password}@{host}:{port}"),
            None => format!("postgres://{user}@{host}:{port}"),
        }
    }

    fn postgres_admin_url() -> String {
        if let Ok(url) = std::env::var("POSTGRES_TEST_ADMIN_URL") {
            let trimmed = url.trim();
            if !trimmed.is_empty() {
                return trimmed.to_string();
            }
        }
        format!("{}/postgres", postgres_base_database_url())
    }

    async fn pg_seed_card(
        pool: &sqlx::PgPool,
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

    async fn pg_seed_dispatch(
        pool: &sqlx::PgPool,
        dispatch_id: &str,
        card_id: &str,
        chain_depth: i64,
    ) {
        sqlx::query(
            "INSERT INTO task_dispatches (
                id, kanban_card_id, to_agent_id, dispatch_type, status, title, chain_depth
             ) VALUES ($1, $2, NULL, 'implementation', 'completed', 'parent', $3)
             ON CONFLICT (id) DO UPDATE
             SET kanban_card_id = EXCLUDED.kanban_card_id,
                 chain_depth = EXCLUDED.chain_depth",
        )
        .bind(dispatch_id)
        .bind(card_id)
        .bind(chain_depth)
        .execute(pool)
        .await
        .expect("seed task_dispatches");
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

    // ── resolve_parent_dispatch_context_pg ────────────────────────────────

    #[tokio::test]
    async fn pg_resolve_parent_dispatch_context_returns_chain_depth_plus_one() {
        let Some(pg_db) = TestPostgresDb::create().await else {
            return;
        };
        let Some(pool) = pg_db.migrate().await else {
            pg_db.drop().await;
            return;
        };

        pg_seed_card(&pool, "card-pg-parent-happy", Some(847), None).await;
        pg_seed_dispatch(
            &pool,
            "dispatch-pg-parent-happy",
            "card-pg-parent-happy",
            3_000_000_200,
        )
        .await;

        let context = json!({ "parent_dispatch_id": "dispatch-pg-parent-happy" });
        let (parent_id, depth) =
            resolve_parent_dispatch_context(&pool, "card-pg-parent-happy", &context)
                .await
                .expect("happy-path parent context");

        assert_eq!(parent_id.as_deref(), Some("dispatch-pg-parent-happy"));
        assert_eq!(depth, 3_000_000_201);

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn pg_resolve_parent_dispatch_context_rejects_cross_card_parent() {
        let Some(pg_db) = TestPostgresDb::create().await else {
            return;
        };
        let Some(pool) = pg_db.migrate().await else {
            pg_db.drop().await;
            return;
        };

        pg_seed_card(&pool, "card-pg-parent-a", Some(8470), None).await;
        pg_seed_card(&pool, "card-pg-parent-b", Some(8471), None).await;
        pg_seed_dispatch(&pool, "dispatch-pg-parent-cross", "card-pg-parent-b", 0).await;

        let context = json!({ "parent_dispatch_id": "dispatch-pg-parent-cross" });
        let err = resolve_parent_dispatch_context(&pool, "card-pg-parent-a", &context)
            .await
            .expect_err("must reject parent that belongs to another card");
        assert!(
            err.to_string().contains("belongs to a different card"),
            "unexpected error message: {err}"
        );

        // missing parent → bail
        let context = json!({ "parent_dispatch_id": "dispatch-pg-parent-missing" });
        let err = resolve_parent_dispatch_context(&pool, "card-pg-parent-a", &context)
            .await
            .expect_err("must reject unknown parent_dispatch_id");
        assert!(
            err.to_string().contains("not found"),
            "unexpected error message: {err}"
        );

        // empty / missing parent → (None, 0)
        let (parent_id, depth) =
            resolve_parent_dispatch_context(&pool, "card-pg-parent-a", &json!({}))
                .await
                .unwrap();
        assert!(parent_id.is_none());
        assert_eq!(depth, 0);

        pool.close().await;
        pg_db.drop().await;
    }

    // ── resolve_card_target_repo_ref_pg ───────────────────────────────────

    #[tokio::test]
    async fn pg_resolve_card_target_repo_ref_prefers_caller_supplied_value() {
        let Some(pg_db) = TestPostgresDb::create().await else {
            return;
        };
        let Some(pool) = pg_db.migrate().await else {
            pg_db.drop().await;
            return;
        };

        pg_seed_card(
            &pool,
            "card-pg-tr-happy",
            Some(847),
            Some("itismyfield/AgentDesk"),
        )
        .await;

        let context = json!({ "target_repo": "external/repo" });
        let value = resolve_card_target_repo_ref(&pool, "card-pg-tr-happy", Some(&context)).await;
        assert_eq!(value.as_deref(), Some("external/repo"));

        let value = resolve_card_target_repo_ref(&pool, "card-pg-tr-happy", None).await;
        assert_eq!(value.as_deref(), Some("itismyfield/AgentDesk"));

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn pg_resolve_card_target_repo_ref_returns_none_for_unknown_card() {
        let Some(pg_db) = TestPostgresDb::create().await else {
            return;
        };
        let Some(pool) = pg_db.migrate().await else {
            pg_db.drop().await;
            return;
        };

        let value = resolve_card_target_repo_ref(&pool, "card-pg-tr-missing", None).await;
        assert!(value.is_none());

        pool.close().await;
        pg_db.drop().await;
    }

    /// #847: TargetRepoSource provenance regression.
    ///
    /// `create_dispatch_core_internal` derives `TargetRepoSource` from the
    /// raw caller context (`json_string_field(context, "target_repo")`)
    /// BEFORE either resolver runs. Both Caller-Supplied and
    /// Card-Scope-Default cases must round-trip identically through the PG
    /// helper so the downstream `external_target_repo_unrecoverable` filter
    /// in `build_review_context` keeps fail-closed semantics.
    #[tokio::test]
    async fn pg_resolve_card_target_repo_ref_preserves_provenance() {
        let Some(pg_db) = TestPostgresDb::create().await else {
            return;
        };
        let Some(pool) = pg_db.migrate().await else {
            pg_db.drop().await;
            return;
        };

        pg_seed_card(&pool, "card-pg-tr-prov", Some(847), Some("card/scope-repo")).await;

        // Case 1: Caller-Supplied. Provenance is computed from the raw
        // context BEFORE resolution; the helper returns the supplied value.
        let caller_ctx = json!({ "target_repo": "caller/explicit-repo" });
        let provenance_caller = if json_string_field(&caller_ctx, "target_repo").is_some() {
            TargetRepoSource::CallerSupplied
        } else {
            TargetRepoSource::CardScopeDefault
        };
        assert_eq!(provenance_caller, TargetRepoSource::CallerSupplied);
        let resolved =
            resolve_card_target_repo_ref(&pool, "card-pg-tr-prov", Some(&caller_ctx)).await;
        assert_eq!(resolved.as_deref(), Some("caller/explicit-repo"));

        // Case 2: Card-Scope-Default. No caller pin → CardScopeDefault and
        // helper falls back to card.repo_id.
        let card_ctx = json!({ "title": "no target_repo here" });
        let provenance_card = if json_string_field(&card_ctx, "target_repo").is_some() {
            TargetRepoSource::CallerSupplied
        } else {
            TargetRepoSource::CardScopeDefault
        };
        assert_eq!(provenance_card, TargetRepoSource::CardScopeDefault);
        let resolved =
            resolve_card_target_repo_ref(&pool, "card-pg-tr-prov", Some(&card_ctx)).await;
        assert_eq!(resolved.as_deref(), Some("card/scope-repo"));

        // Case 3: empty context → still CardScopeDefault, still card-scope value.
        let empty_ctx = json!({});
        let provenance_empty = if json_string_field(&empty_ctx, "target_repo").is_some() {
            TargetRepoSource::CallerSupplied
        } else {
            TargetRepoSource::CardScopeDefault
        };
        assert_eq!(provenance_empty, TargetRepoSource::CardScopeDefault);
        let resolved =
            resolve_card_target_repo_ref(&pool, "card-pg-tr-prov", Some(&empty_ctx)).await;
        assert_eq!(resolved.as_deref(), Some("card/scope-repo"));

        pool.close().await;
        pg_db.drop().await;
    }

    // ── resolve_card_worktree_pg ──────────────────────────────────────────

    #[tokio::test]
    async fn pg_resolve_card_worktree_returns_none_when_no_issue_number() {
        let Some(pg_db) = TestPostgresDb::create().await else {
            return;
        };
        let Some(pool) = pg_db.migrate().await else {
            pg_db.drop().await;
            return;
        };

        pg_seed_card(&pool, "card-pg-wt-no-issue", None, None).await;
        let resolved = resolve_card_worktree(&pool, "card-pg-wt-no-issue", None)
            .await
            .expect("no-issue card returns Ok(None)");
        assert!(resolved.is_none());

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn pg_resolve_card_worktree_happy_path_finds_active_worktree() {
        let Some(pg_db) = TestPostgresDb::create().await else {
            return;
        };
        let Some(pool) = pg_db.migrate().await else {
            pg_db.drop().await;
            return;
        };

        let _env_lock = crate::services::discord::runtime_store::lock_test_env();
        let repo = init_test_repo();
        let repo_dir = repo.path().to_str().unwrap();
        let wt_dir = repo.path().join("wt-pg-847");
        let wt_path = wt_dir.to_str().unwrap();
        run_git(repo_dir, &["worktree", "add", "-b", "wt/pg-847", wt_path]);
        let _wt_commit = git_commit(wt_path, "fix: pg helper worktree (#847)");

        pg_seed_card(&pool, "card-pg-wt-happy", Some(847), None).await;
        let context = json!({ "target_repo": repo_dir });
        let resolved = resolve_card_worktree(&pool, "card-pg-wt-happy", Some(&context))
            .await
            .expect("happy-path worktree resolution");
        let (path, branch, _commit) = resolved.expect("worktree should be discoverable for issue");
        assert_eq!(canonicalize_path(&path), canonicalize_path(wt_path));
        assert_eq!(branch, "wt/pg-847");

        pool.close().await;
        pg_db.drop().await;
    }

    // ── inject_review_dispatch_identifiers_pg ─────────────────────────────

    #[tokio::test]
    async fn pg_inject_review_dispatch_identifiers_happy_path() {
        let Some(pg_db) = TestPostgresDb::create().await else {
            return;
        };
        let Some(pool) = pg_db.migrate().await else {
            pg_db.drop().await;
            return;
        };

        pg_seed_card(
            &pool,
            "card-pg-ids-happy",
            Some(3_000_000_111),
            Some("itismyfield/AgentDesk"),
        )
        .await;
        sqlx::query(
            "INSERT INTO pr_tracking (card_id, repo_id, branch, pr_number, head_sha, state)
             VALUES ($1, 'itismyfield/AgentDesk', 'wt/pg-847', 3_000_000_222, 'deadbeef', 'review')",
        )
        .bind("card-pg-ids-happy")
        .execute(&pool)
        .await
        .expect("seed pr_tracking");

        let mut obj = serde_json::Map::new();
        inject_review_dispatch_identifiers(&pool, "card-pg-ids-happy", "review", &mut obj).await;

        assert_eq!(
            obj.get("repo").and_then(|v| v.as_str()),
            Some("itismyfield/AgentDesk")
        );
        assert_eq!(
            obj.get("issue_number").and_then(|v| v.as_i64()),
            Some(3_000_000_111)
        );
        assert_eq!(
            obj.get("pr_number").and_then(|v| v.as_i64()),
            Some(3_000_000_222)
        );
        assert_eq!(
            obj.get("verdict_endpoint").and_then(|v| v.as_str()),
            Some("POST /api/reviews/verdict")
        );

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn pg_inject_review_dispatch_identifiers_skips_unknown_pr_and_issue() {
        let Some(pg_db) = TestPostgresDb::create().await else {
            return;
        };
        let Some(pool) = pg_db.migrate().await else {
            pg_db.drop().await;
            return;
        };

        // Card without issue number, no PR row, no caller-supplied repo.
        pg_seed_card(&pool, "card-pg-ids-empty", None, None).await;

        let mut obj = serde_json::Map::new();
        inject_review_dispatch_identifiers(&pool, "card-pg-ids-empty", "review-decision", &mut obj)
            .await;

        assert!(obj.get("repo").is_none(), "repo must remain unset");
        assert!(obj.get("issue_number").is_none());
        assert!(obj.get("pr_number").is_none());
        assert_eq!(
            obj.get("decision_endpoint").and_then(|v| v.as_str()),
            Some("POST /api/reviews/decision")
        );

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn pg_build_review_context_untrusted_strips_bogus_branch_and_reresolves() {
        let Some(pg_db) = TestPostgresDb::create().await else {
            return;
        };
        let Some(pool) = pg_db.migrate().await else {
            pg_db.drop().await;
            return;
        };

        let db = test_db();
        let card_id = "card-pg-review-untrusted-branch";
        let issue_number = 8481;
        let (repo, _repo_override) = setup_test_repo();
        let repo_dir = repo.path().to_str().unwrap();
        let wt_dir = repo.path().join("wt-8481-live");
        let wt_path = wt_dir.to_str().unwrap();
        run_git(
            repo_dir,
            &["worktree", "add", "-b", "wt/8481-live", wt_path],
        );
        let reviewed_commit = git_commit(wt_path, "fix: pg review branch recovery (#8481)");

        seed_card(&db, card_id, issue_number, "review");
        set_card_repo_id(&db, card_id, repo_dir);
        pg_seed_card(&pool, card_id, Some(issue_number), Some(repo_dir)).await;
        pg_seed_agent(&pool, "agent-1", Some("111"), Some("222")).await;

        let input = json!({
            "branch": "bogus/pr-branch",
            "reviewed_commit": "1111111111111111111111111111111111111111",
            "worktree_path": "/tmp/agentdesk-bogus-review-path",
        });

        let sqlite_context = build_review_context_sqlite_test(
            &db,
            card_id,
            "agent-1",
            &input,
            ReviewTargetTrust::Untrusted,
            TargetRepoSource::CardScopeDefault,
        )
        .expect("sqlite review context");
        let pg_context = build_review_context(
            &pool,
            card_id,
            "agent-1",
            &input,
            ReviewTargetTrust::Untrusted,
            TargetRepoSource::CardScopeDefault,
        )
        .await
        .expect("pg review context");

        let sqlite_parsed: serde_json::Value = serde_json::from_str(&sqlite_context).unwrap();
        let pg_parsed: serde_json::Value = serde_json::from_str(&pg_context).unwrap();

        assert_eq!(pg_parsed, sqlite_parsed);
        assert_eq!(pg_parsed["branch"], "wt/8481-live");
        assert_eq!(pg_parsed["reviewed_commit"], reviewed_commit);
        assert_eq!(
            canonicalize_path(pg_parsed["worktree_path"].as_str().unwrap()),
            canonicalize_path(wt_path)
        );

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn pg_build_review_context_untrusted_caller_supplied_unrecoverable_target_repo_errors() {
        let Some(pg_db) = TestPostgresDb::create().await else {
            return;
        };
        let Some(pool) = pg_db.migrate().await else {
            pg_db.drop().await;
            return;
        };

        let card_id = "card-pg-review-untrusted-target-repo-error";
        let issue_number = 8482;
        let (repo, _repo_override) = setup_test_repo();
        let repo_dir = repo.path().to_str().unwrap();
        let wt_dir = repo.path().join("wt-8482-live");
        let wt_path = wt_dir.to_str().unwrap();
        run_git(
            repo_dir,
            &["worktree", "add", "-b", "wt/8482-live", wt_path],
        );
        let _reviewed_commit = git_commit(wt_path, "fix: pg target repo error guard (#8482)");

        pg_seed_card(&pool, card_id, Some(issue_number), Some(repo_dir)).await;
        pg_seed_agent(&pool, "agent-1", Some("111"), Some("222")).await;

        let err = build_review_context(
            &pool,
            card_id,
            "agent-1",
            &json!({
                "target_repo": "/tmp/agentdesk-pg-review-missing-target-repo-8482"
            }),
            ReviewTargetTrust::Untrusted,
            TargetRepoSource::CallerSupplied,
        )
        .await
        .expect_err("untrusted caller-supplied unrecoverable target_repo must fail closed");

        assert!(
            err.to_string()
                .contains("external_target_repo_unrecoverable"),
            "unexpected error: {err:#}"
        );

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn pg_build_review_context_untrusted_without_target_repo_falls_back_silently() {
        let Some(pg_db) = TestPostgresDb::create().await else {
            return;
        };
        let Some(pool) = pg_db.migrate().await else {
            pg_db.drop().await;
            return;
        };

        let db = test_db();
        let card_id = "card-pg-review-no-target-repo";
        let issue_number = 8483;
        let (repo, _repo_override) = setup_test_repo();
        let repo_dir = repo.path().to_str().unwrap();
        let wt_dir = repo.path().join("wt-8483-live");
        let wt_path = wt_dir.to_str().unwrap();
        run_git(
            repo_dir,
            &["worktree", "add", "-b", "wt/8483-live", wt_path],
        );
        let reviewed_commit = git_commit(wt_path, "fix: pg no target repo fallback (#8483)");

        seed_card(&db, card_id, issue_number, "review");
        set_card_repo_id(&db, card_id, repo_dir);
        pg_seed_card(&pool, card_id, Some(issue_number), Some(repo_dir)).await;
        pg_seed_agent(&pool, "agent-1", Some("111"), Some("222")).await;

        let sqlite_context = build_review_context_sqlite_test(
            &db,
            card_id,
            "agent-1",
            &json!({}),
            ReviewTargetTrust::Untrusted,
            TargetRepoSource::CardScopeDefault,
        )
        .expect("sqlite no-target_repo fallback");
        let pg_context = build_review_context(
            &pool,
            card_id,
            "agent-1",
            &json!({}),
            ReviewTargetTrust::Untrusted,
            TargetRepoSource::CardScopeDefault,
        )
        .await
        .expect("pg no-target_repo fallback");

        let sqlite_parsed: serde_json::Value = serde_json::from_str(&sqlite_context).unwrap();
        let pg_parsed: serde_json::Value = serde_json::from_str(&pg_context).unwrap();

        assert_eq!(pg_parsed, sqlite_parsed);
        assert_eq!(pg_parsed["target_repo"], repo_dir);
        assert_eq!(pg_parsed["branch"], "wt/8483-live");
        assert_eq!(pg_parsed["reviewed_commit"], reviewed_commit);
        assert_eq!(
            canonicalize_path(pg_parsed["worktree_path"].as_str().unwrap()),
            canonicalize_path(wt_path)
        );
        assert!(pg_parsed.get("review_target_reject_reason").is_none());

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn pg_build_review_context_trusted_preserves_preseeded_fields() {
        let Some(pg_db) = TestPostgresDb::create().await else {
            return;
        };
        let Some(pool) = pg_db.migrate().await else {
            pg_db.drop().await;
            return;
        };

        let db = test_db();
        let card_id = "card-pg-review-trusted-preserve";
        let issue_number = 8484;
        let (repo, _repo_override) = setup_test_repo();
        let repo_dir = repo.path().to_str().unwrap();
        let reviewed_commit = git_commit(repo_dir, "fix: pg trusted preserve (#8484)");

        seed_card(&db, card_id, issue_number, "review");
        set_card_repo_id(&db, card_id, repo_dir);
        let conn = db.separate_conn().unwrap();
        conn.execute(
            "INSERT INTO pr_tracking (
                card_id, repo_id, worktree_path, branch, pr_number, head_sha, state, created_at, updated_at
             ) VALUES (
                ?1, ?2, ?3, 'wt/sqlite-db-value', 7001, ?4, 'review', datetime('now'), datetime('now')
             )",
            sqlite_test::params![card_id, repo_dir, repo_dir, reviewed_commit],
        )
        .unwrap();
        drop(conn);

        pg_seed_card(&pool, card_id, Some(issue_number), Some(repo_dir)).await;
        pg_seed_agent(&pool, "agent-1", Some("111"), Some("222")).await;
        sqlx::query(
            "INSERT INTO pr_tracking (
                card_id, repo_id, worktree_path, branch, pr_number, head_sha, state
             ) VALUES (
                $1, $2, $3, 'wt/pg-db-value', 7001, $4, 'review'
             )",
        )
        .bind(card_id)
        .bind(repo_dir)
        .bind(repo_dir)
        .bind(&reviewed_commit)
        .execute(&pool)
        .await
        .expect("seed pr_tracking");

        let input = json!({
            "target_repo": "caller/preseeded-repo",
            "worktree_path": repo_dir,
            "branch": "trusted/preseeded-branch",
            "reviewed_commit": reviewed_commit,
            "pr_number": 4242,
        });

        let sqlite_context = build_review_context_sqlite_test(
            &db,
            card_id,
            "agent-1",
            &input,
            ReviewTargetTrust::Trusted,
            TargetRepoSource::CallerSupplied,
        )
        .expect("sqlite trusted preserve");
        let pg_context = build_review_context(
            &pool,
            card_id,
            "agent-1",
            &input,
            ReviewTargetTrust::Trusted,
            TargetRepoSource::CallerSupplied,
        )
        .await
        .expect("pg trusted preserve");

        let sqlite_parsed: serde_json::Value = serde_json::from_str(&sqlite_context).unwrap();
        let pg_parsed: serde_json::Value = serde_json::from_str(&pg_context).unwrap();

        assert_eq!(pg_parsed, sqlite_parsed);
        assert_eq!(pg_parsed["target_repo"], "caller/preseeded-repo");
        assert_eq!(pg_parsed["repo"], "caller/preseeded-repo");
        assert_eq!(pg_parsed["branch"], "trusted/preseeded-branch");
        assert_eq!(pg_parsed["reviewed_commit"], reviewed_commit);
        assert_eq!(pg_parsed["pr_number"], 4242);

        pool.close().await;
        pg_db.drop().await;
    }
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
#[path = "dispatch_context_relocated_tests.rs"]
mod relocated_tests;
