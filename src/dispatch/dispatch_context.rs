use anyhow::Result;
#[cfg(test)]
use rusqlite::OptionalExtension;
use serde_json::json;
use sqlx::PgPool;

use crate::db::Db;
#[cfg(test)]
use crate::db::agents::load_agent_channel_bindings;
use crate::db::agents::load_agent_channel_bindings_pg;
use crate::services::provider::ProviderKind;

use super::dispatch_channel::provider_from_channel_suffix;

#[derive(Debug, Clone, PartialEq, Eq)]
struct DispatchExecutionTarget {
    reviewed_commit: String,
    branch: Option<String>,
    worktree_path: Option<String>,
    target_repo: Option<String>,
}

#[cfg(test)]
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

pub(crate) fn dispatch_type_uses_thread_routing(dispatch_type: Option<&str>) -> bool {
    !matches!(dispatch_type, Some("phase-gate"))
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
/// `src/kanban.rs:strip_stale_worktree_metadata_from_dispatches_on_conn`.
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

#[cfg(test)]
pub(super) fn resolve_parent_dispatch_context_sqlite_test(
    conn: &rusqlite::Connection,
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

#[cfg(test)]
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

#[cfg(test)]
fn load_card_issue_repo(db: &Db, card_id: &str) -> Option<(Option<i64>, Option<String>)> {
    load_card_dispatch_info(db, card_id).map(|info| (info.issue_number, info.repo_id))
}

#[cfg(test)]
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

#[cfg(test)]
pub(crate) fn inject_review_dispatch_identifiers_sqlite_test(
    db: &Db,
    card_id: &str,
    dispatch_type: &str,
    obj: &mut serde_json::Map<String, serde_json::Value>,
) {
    let snapshot = serde_json::Value::Object(obj.clone());
    let repo = json_string_field(&snapshot, "repo")
        .or_else(|| json_string_field(&snapshot, "target_repo"))
        .map(str::to_string)
        .or_else(|| resolve_card_target_repo_ref_sqlite_test(db, card_id, Some(&snapshot)));
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
                .or_insert_with(|| json!("POST /api/review-verdict"));
        }
        "review-decision" => {
            obj.entry("decision_endpoint".to_string())
                .or_insert_with(|| json!("POST /api/review-decision"));
        }
        _ => {}
    }
}
#[cfg(test)]
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

#[cfg(test)]
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

#[cfg(test)]
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
#[cfg(test)]
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
    let Ok(output) = std::process::Command::new("git")
        .args(["log", "--format=%s", "-n", "1", commit_sha])
        .current_dir(&repo_dir)
        .output()
    else {
        tracing::warn!(
            "[dispatch] commit_belongs_to_card_issue: git log failed — rejecting to fallback"
        );
        return false;
    };
    if !output.status.success() {
        tracing::warn!(
            "[dispatch] commit_belongs_to_card_issue: commit {} not reachable — rejecting to fallback",
            &commit_sha[..8.min(commit_sha.len())]
        );
        return false;
    }
    let subject = String::from_utf8_lossy(&output.stdout);
    let pattern = format!("(#{})", issue_number);
    subject.contains(&pattern)
}

#[cfg(not(test))]
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

fn git_commit_exists(dir: &str, commit_sha: &str) -> bool {
    std::process::Command::new("git")
        .args(["cat-file", "-e", &format!("{commit_sha}^{{commit}}")])
        .current_dir(dir)
        .output()
        .ok()
        .is_some_and(|output| output.status.success())
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
    let Some(output) = std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(dir)
        .output()
        .ok()
    else {
        return false;
    };
    if !output.status.success() {
        return false;
    }
    let head = String::from_utf8_lossy(&output.stdout).trim().to_string();
    head == commit_sha
}

#[cfg(test)]
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

#[cfg(test)]
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

#[cfg(test)]
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

#[cfg(test)]
pub(super) fn validate_dispatch_completion_evidence_on_conn(
    conn: &rusqlite::Connection,
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
    if let Some(branch) = target.branch.as_deref() {
        obj.insert("branch".to_string(), json!(branch));
    }
    if let Some(path) = target.worktree_path.as_deref() {
        obj.insert("worktree_path".to_string(), json!(path));
    }
    if let Some(target_repo) = target.target_repo.as_deref() {
        obj.insert("target_repo".to_string(), json!(target_repo));
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
#[cfg(test)]
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

#[cfg(test)]
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

#[cfg(test)]
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
#[cfg(test)]
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

        if !obj.contains_key("from_provider") || !obj.contains_key("target_provider") {
            if let Ok(conn) = db.separate_conn() {
                if let Ok(Some(bindings)) = load_agent_channel_bindings(&conn, to_agent_id) {
                    let primary_provider = bindings
                        .provider
                        .as_deref()
                        .and_then(ProviderKind::from_str);
                    if !obj.contains_key("from_provider") {
                        if let Some(fp) = primary_provider.as_ref().map(ProviderKind::as_str) {
                            obj.insert("from_provider".to_string(), json!(fp));
                        } else if let Some(fp) = bindings
                            .primary_channel()
                            .as_deref()
                            .and_then(provider_from_channel_suffix)
                        {
                            obj.insert("from_provider".to_string(), json!(fp));
                        }
                    }
                    if !obj.contains_key("target_provider") {
                        if let Some(tp) = primary_provider.as_ref().map(|p| p.counterpart()) {
                            obj.insert("target_provider".to_string(), json!(tp.as_str()));
                        } else if let Some(tp) = bindings
                            .counter_model_channel()
                            .as_deref()
                            .and_then(provider_from_channel_suffix)
                        {
                            obj.insert("target_provider".to_string(), json!(tp));
                        }
                    }
                }
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
    let snapshot = serde_json::Value::Object(obj.clone());
    let repo = match json_string_field(&snapshot, "repo")
        .or_else(|| json_string_field(&snapshot, "target_repo"))
        .map(str::to_string)
    {
        Some(value) => Some(value),
        None => resolve_card_target_repo_ref(pool, card_id, Some(&snapshot)).await,
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
                .or_insert_with(|| json!("POST /api/review-verdict"));
        }
        "review-decision" => {
            obj.entry("decision_endpoint".to_string())
                .or_insert_with(|| json!("POST /api/review-decision"));
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
    let Ok(output) = std::process::Command::new("git")
        .args(["log", "--format=%s", "-n", "1", commit_sha])
        .current_dir(&repo_dir)
        .output()
    else {
        tracing::warn!(
            "[dispatch] commit_belongs_to_card_issue: git log failed — rejecting to fallback"
        );
        return false;
    };
    if !output.status.success() {
        tracing::warn!(
            "[dispatch] commit_belongs_to_card_issue: commit {} not reachable — rejecting to fallback",
            &commit_sha[..8.min(commit_sha.len())]
        );
        return false;
    }
    let subject = String::from_utf8_lossy(&output.stdout);
    let pattern = format!("(#{})", issue_number);
    subject.contains(&pattern)
}

async fn refresh_review_target_worktree_pg(
    pool: &PgPool,
    card_id: &str,
    context: &serde_json::Value,
    target: &DispatchExecutionTarget,
) -> Result<Option<DispatchExecutionTarget>> {
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
            let branch = resolve_review_target_branch_pg(
                pool,
                card_id,
                &wt_path,
                &target.reviewed_commit,
                target.branch.as_deref().or(Some(wt_branch.as_str())),
            )
            .await
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
            let branch = resolve_review_target_branch_pg(
                pool,
                card_id,
                &repo_dir,
                &target.reviewed_commit,
                target.branch.as_deref(),
            )
            .await;
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

    let Some(mut target) = execution_target_from_dir(&repo_dir) else {
        return Ok(None);
    };
    target.target_repo = resolve_card_target_repo_ref(pool, kanban_card_id, context).await;
    Ok(Some(target))
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

    if !obj.contains_key("from_provider") || !obj.contains_key("target_provider") {
        if let Ok(Some(bindings)) = load_agent_channel_bindings_pg(pool, to_agent_id).await {
            let primary_provider = bindings
                .provider
                .as_deref()
                .and_then(ProviderKind::from_str);
            if !obj.contains_key("from_provider") {
                if let Some(fp) = primary_provider.as_ref().map(ProviderKind::as_str) {
                    obj.insert("from_provider".to_string(), json!(fp));
                } else if let Some(fp) = bindings
                    .primary_channel()
                    .as_deref()
                    .and_then(provider_from_channel_suffix)
                {
                    obj.insert("from_provider".to_string(), json!(fp));
                }
            }
            if !obj.contains_key("target_provider") {
                if let Some(tp) = primary_provider.as_ref().map(|p| p.counterpart()) {
                    obj.insert("target_provider".to_string(), json!(tp.as_str()));
                } else if let Some(tp) = bindings
                    .counter_model_channel()
                    .as_deref()
                    .and_then(provider_from_channel_suffix)
                {
                    obj.insert("target_provider".to_string(), json!(tp));
                }
            }
        }
    }

    Ok(serde_json::to_string(&serde_json::Value::Object(obj))?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Db;
    use crate::engine::PolicyEngine;
    use serde_json::json;
    use std::io::{self, Write};
    use std::process::Command;
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

    fn run_git(dir: &str, args: &[&str]) {
        let output = Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&output.stderr)
        );
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
            rusqlite::params![card_id, status, issue_number],
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
            rusqlite::params![repo_id, card_id],
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
            rusqlite::params![repo_dir, repo_dir, reviewed_commit],
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
        assert_eq!(parsed["verdict_endpoint"], "POST /api/review-verdict");
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
            rusqlite::params![
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
            rusqlite::params![
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
            Some("POST /api/review-verdict")
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
            Some("POST /api/review-decision")
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
            rusqlite::params![card_id, repo_dir, repo_dir, reviewed_commit],
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
