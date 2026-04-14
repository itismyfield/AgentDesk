use anyhow::Result;
use rusqlite::OptionalExtension;
use serde_json::json;

use crate::db::Db;
use crate::db::agents::{load_agent_channel_bindings, resolve_agent_dispatch_channel_on_conn};
use crate::engine::PolicyEngine;
use crate::services::provider::ProviderKind;

#[derive(Debug, Clone, Copy, Default)]
pub struct DispatchCreateOptions {
    pub skip_outbox: bool,
    pub sidecar_dispatch: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DispatchExecutionTarget {
    reviewed_commit: String,
    branch: Option<String>,
    worktree_path: Option<String>,
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
    })
}

fn json_string_field<'a>(value: &'a serde_json::Value, key: &str) -> Option<&'a str> {
    value
        .get(key)
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
}

pub(crate) fn dispatch_type_force_new_session_default(dispatch_type: Option<&str>) -> Option<bool> {
    match dispatch_type {
        Some("implementation") | Some("review") | Some("rework") => Some(true),
        Some("review-decision") => Some(false),
        _ => None,
    }
}

fn dispatch_context_with_session_strategy(
    dispatch_type: &str,
    context: &serde_json::Value,
) -> serde_json::Value {
    let Some(default_force_new_session) =
        dispatch_type_force_new_session_default(Some(dispatch_type))
    else {
        return context.clone();
    };

    let mut context = if context.is_object() {
        context.clone()
    } else {
        json!({})
    };

    if let Some(obj) = context.as_object_mut() {
        obj.entry("force_new_session".to_string())
            .or_insert(json!(default_force_new_session));
    }

    context
}

fn dispatch_context_requests_sidecar(context: &serde_json::Value) -> bool {
    context
        .get("sidecar_dispatch")
        .and_then(|value| value.as_bool())
        .unwrap_or(false)
        || context
            .get("phase_gate")
            .and_then(|value| value.as_object())
            .is_some()
}

fn dispatch_context_worktree_target(
    context: &serde_json::Value,
) -> Result<Option<(String, Option<String>)>> {
    let Some(path) = json_string_field(context, "worktree_path") else {
        return Ok(None);
    };
    if !std::path::Path::new(path).is_dir() {
        anyhow::bail!(
            "Cannot create dispatch with explicit worktree_path '{}': path does not exist or is not a directory",
            path
        );
    }

    let branch = json_string_field(context, "worktree_branch")
        .or_else(|| json_string_field(context, "branch"))
        .map(str::to_string)
        .or_else(|| crate::services::platform::shell::git_branch_name(path));

    Ok(Some((path.to_string(), branch)))
}

fn resolve_parent_dispatch_context(
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

fn load_card_issue_repo(db: &Db, card_id: &str) -> Option<(Option<i64>, Option<String>)> {
    db.separate_conn().ok().and_then(|conn| {
        conn.query_row(
            "SELECT github_issue_number, repo_id FROM kanban_cards WHERE id = ?1",
            [card_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .ok()
    })
}

fn resolve_card_repo_dir(db: &Db, card_id: &str, purpose: &str) -> Result<Option<String>> {
    let repo_id = load_card_issue_repo(db, card_id).and_then(|(_, repo_id)| repo_id);
    crate::services::platform::resolve_repo_dir_for_id(repo_id.as_deref())
        .map_err(|e| anyhow::anyhow!("Cannot {purpose} for card {}: {}", card_id, e))
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
pub(crate) fn commit_belongs_to_card_issue(db: &Db, card_id: &str, commit_sha: &str) -> bool {
    let issue_number = load_card_issue_repo(db, card_id).and_then(|(issue_number, _)| issue_number);
    let Some(issue_number) = issue_number else {
        // No issue number on card — can't validate, assume OK
        return true;
    };
    let repo_dir = match resolve_card_repo_dir(db, card_id, "validate reviewed commit") {
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
    // Check commit subject for (#<issue_number>)
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

    let path = result_json
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
    let branch = result_json
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
    let reviewed_commit = result_json
        .as_ref()
        .and_then(|v| json_string_field(v, "completed_commit"))
        .or_else(|| {
            result_json
                .as_ref()
                .and_then(|v| json_string_field(v, "reviewed_commit"))
        })
        .map(str::to_string);

    if let Some(reviewed_commit) = reviewed_commit {
        let fallback_repo_dir =
            resolve_card_repo_dir(db, kanban_card_id, "recover completed work repo")
                .ok()
                .flatten();
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
        });
    }

    let trusted_path =
        path.filter(|candidate| is_card_scoped_worktree_path(candidate, branch.as_deref()));

    trusted_path.and_then(execution_target_from_dir)
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

fn dispatch_has_assistant_response(conn: &rusqlite::Connection, dispatch_id: &str) -> Result<bool> {
    conn.query_row(
        "SELECT COUNT(*) > 0
         FROM session_transcripts
         WHERE dispatch_id = ?1
           AND TRIM(assistant_message) <> ''",
        [dispatch_id],
        |row| row.get(0),
    )
    .map_err(|e| anyhow::anyhow!("session transcript lookup failed: {e}"))
}

fn validate_dispatch_completion_evidence_on_conn(
    conn: &rusqlite::Connection,
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
        || dispatch_has_assistant_response(conn, dispatch_id)?
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

#[allow(dead_code)]
pub(crate) fn validate_dispatch_completion_evidence(
    db: &Db,
    dispatch_id: &str,
    result: &serde_json::Value,
) -> Result<()> {
    let conn = db
        .separate_conn()
        .map_err(|e| anyhow::anyhow!("DB lock error: {e}"))?;
    validate_dispatch_completion_evidence_on_conn(&conn, dispatch_id, result)
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
}

fn latest_completed_work_dispatch_is_noop(db: &Db, kanban_card_id: &str) -> bool {
    let Ok(conn) = db.separate_conn() else {
        return false;
    };
    let result_raw: Option<String> = conn
        .query_row(
            "SELECT result
             FROM task_dispatches
             WHERE kanban_card_id = ?1
               AND dispatch_type IN ('implementation', 'rework')
               AND status = 'completed'
             ORDER BY updated_at DESC, rowid DESC
             LIMIT 1",
            [kanban_card_id],
            |row| row.get(0),
        )
        .ok()
        .flatten();
    let Some(result_json) = result_raw
        .as_deref()
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(raw).ok())
    else {
        return false;
    };

    json_string_field(&result_json, "work_outcome") == Some("noop")
        || result_json
            .get("completed_without_changes")
            .and_then(|value| value.as_bool())
            .unwrap_or(false)
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

fn inject_review_merge_base_context(obj: &mut serde_json::Map<String, serde_json::Value>) {
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
pub(crate) fn resolve_card_worktree(
    db: &Db,
    card_id: &str,
) -> Result<Option<(String, String, String)>> {
    let Some((issue_number, _repo_id)) = load_card_issue_repo(db, card_id) else {
        return Ok(None);
    };
    let Some(issue_number) = issue_number else {
        return Ok(None);
    };
    let Some(repo_dir) = resolve_card_repo_dir(db, card_id, "resolve worktree repo")? else {
        return Ok(None);
    };
    Ok(
        crate::services::platform::find_worktree_for_issue(&repo_dir, issue_number)
            .map(|wt| (wt.path, wt.branch, wt.commit)),
    )
}

fn resolve_card_issue_commit_target(
    db: &Db,
    card_id: &str,
) -> Result<Option<DispatchExecutionTarget>> {
    let Some((issue_number, _repo_id)) = load_card_issue_repo(db, card_id) else {
        return Ok(None);
    };
    let Some(issue_number) = issue_number else {
        return Ok(None);
    };
    let Some(repo_dir) = resolve_card_repo_dir(db, card_id, "resolve commit target repo")? else {
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
    }))
}

fn resolve_repo_head_fallback_target(
    db: &Db,
    kanban_card_id: &str,
) -> Result<Option<DispatchExecutionTarget>> {
    let Some(repo_dir) =
        resolve_card_repo_dir(db, kanban_card_id, "resolve repo-root HEAD fallback target")?
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

    Ok(execution_target_from_dir(&repo_dir))
}

/// Build the context JSON string for a review dispatch.
///
/// Injects `reviewed_commit`, `branch`, `worktree_path`, and provider info.
/// Prefers worktree branch (if found for this card's issue) over main HEAD.
fn build_review_context(
    db: &Db,
    kanban_card_id: &str,
    to_agent_id: &str,
    context: &serde_json::Value,
) -> Result<String> {
    let mut ctx_val = dispatch_context_with_session_strategy("review", context);
    if let Some(obj) = ctx_val.as_object_mut() {
        if !obj.contains_key("reviewed_commit") {
            if latest_completed_work_dispatch_is_noop(db, kanban_card_id) {
                tracing::warn!(
                    "[dispatch] Review dispatch for card {}: skipping review context because latest completed work dispatch is explicit noop",
                    kanban_card_id
                );
                anyhow::bail!(
                    "Cannot create review dispatch for card {}: latest completed work dispatch is noop",
                    kanban_card_id
                );
            }

            // Prefer the actual target used by the latest completed work dispatch
            // for this card. This keeps review aligned even when the card had no
            // dedicated worktree and the agent worked directly in the repo root.
            //
            // #269/#491: Cross-validate the commit against the card's issue
            // number. A poisoned reviewed_commit (from an unrelated issue) can
            // propagate through review→rework/dispute cycles if we blindly
            // trust dispatch history or revive the same worktree HEAD after
            // validation already failed.
            let latest_work_target = latest_completed_work_dispatch_target(db, kanban_card_id);
            let validated_work_target = latest_work_target.as_ref().filter(|t| {
                let valid =
                    commit_belongs_to_card_issue(db, kanban_card_id, &t.reviewed_commit);
                if !valid {
                    tracing::warn!(
                        "[dispatch] Review dispatch for card {}: work target commit {} doesn't match card issue — skipping to next fallback",
                        kanban_card_id,
                        &t.reviewed_commit[..8.min(t.reviewed_commit.len())]
                    );
                }
                valid
            });
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
                if let Some((ref wt_path, ref wt_branch, ref wt_commit)) =
                    resolve_card_worktree(db, kanban_card_id)?
                {
                    apply_review_target_context(
                        &DispatchExecutionTarget {
                            reviewed_commit: wt_commit.clone(),
                            branch: Some(wt_branch.clone()),
                            worktree_path: Some(wt_path.clone()),
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
                } else if let Some(target) = resolve_card_issue_commit_target(db, kanban_card_id)? {
                    apply_review_target_context(&target, obj);
                    tracing::info!(
                        "[dispatch] Review dispatch for card {}: recovered issue commit target ({})",
                        kanban_card_id,
                        &target.reviewed_commit[..8.min(target.reviewed_commit.len())]
                    );
                } else {
                    // Last fallback: review the current repo HEAD. This path is used
                    // only when we have neither an execution target nor a canonical
                    // issue worktree.
                    if let Some(target) = resolve_repo_head_fallback_target(db, kanban_card_id)? {
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

        // Inject from_provider/target_provider for cross-provider review validation
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
                        if let Some(tp) = bindings
                            .counter_model_channel()
                            .as_deref()
                            .and_then(provider_from_channel_suffix)
                        {
                            obj.insert("target_provider".to_string(), json!(tp));
                        } else if let Some(tp) = primary_provider.as_ref().map(|p| p.counterpart())
                        {
                            obj.insert("target_provider".to_string(), json!(tp.as_str()));
                        }
                    }
                }
            }
        }
    }
    Ok(serde_json::to_string(&ctx_val)?)
}

/// Cancel a live dispatch and reset any linked auto-queue entry back to pending.
///
/// The dispatch row remains the canonical source of truth. `auto_queue_entries`
/// is a derived projection that must be cleared whenever the linked dispatch is
/// cancelled so a stale `dispatched` entry cannot block or duplicate work.
pub fn cancel_dispatch_and_reset_auto_queue_on_conn(
    conn: &rusqlite::Connection,
    dispatch_id: &str,
    reason: Option<&str>,
) -> rusqlite::Result<usize> {
    let cancel_payload = reason.map(|reason| json!({ "reason": reason }));
    let cancelled = if let Some(payload) = cancel_payload.as_ref() {
        set_dispatch_status_on_conn(
            conn,
            dispatch_id,
            "cancelled",
            Some(payload),
            "cancel_dispatch",
            Some(&["pending", "dispatched"]),
            false,
        )
        .map_err(|e| {
            rusqlite::Error::ToSqlConversionFailure(Box::new(std::io::Error::other(e.to_string())))
        })?
    } else {
        set_dispatch_status_on_conn(
            conn,
            dispatch_id,
            "cancelled",
            None,
            "cancel_dispatch",
            Some(&["pending", "dispatched"]),
            false,
        )
        .map_err(|e| {
            rusqlite::Error::ToSqlConversionFailure(Box::new(std::io::Error::other(e.to_string())))
        })?
    };

    let dispatch_status: Option<String> = conn
        .query_row(
            "SELECT status FROM task_dispatches WHERE id = ?1",
            [dispatch_id],
            |row| row.get(0),
        )
        .ok();
    if matches!(
        dispatch_status.as_deref(),
        Some("cancelled") | Some("failed")
    ) {
        let mut stmt = conn.prepare(
            "SELECT id FROM auto_queue_entries
             WHERE dispatch_id = ?1 AND status IN ('pending', 'dispatched')",
        )?;
        let entry_ids: Vec<String> = stmt
            .query_map([dispatch_id], |row| row.get::<_, String>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        drop(stmt);

        for entry_id in entry_ids {
            crate::db::auto_queue::update_entry_status_on_conn(
                conn,
                &entry_id,
                crate::db::auto_queue::ENTRY_STATUS_PENDING,
                "dispatch_cancel",
                &crate::db::auto_queue::EntryStatusUpdateOptions::default(),
            )
            .map_err(|error| match error {
                crate::db::auto_queue::EntryStatusUpdateError::Sql(sql) => sql,
                other => rusqlite::Error::ToSqlConversionFailure(Box::new(std::io::Error::other(
                    other.to_string(),
                ))),
            })?;
        }
    }

    Ok(cancelled)
}

/// Cancel all live dispatches for a card without resetting auto-queue entries.
///
/// Used when PMD force-transitions a live card back to backlog/ready. In that
/// case the current work should be abandoned rather than re-queued into the
/// same active run.
pub fn cancel_active_dispatches_for_card_on_conn(
    conn: &rusqlite::Connection,
    card_id: &str,
    reason: Option<&str>,
) -> rusqlite::Result<usize> {
    let mut stmt = conn.prepare(
        "SELECT id FROM task_dispatches
         WHERE kanban_card_id = ?1 AND status IN ('pending', 'dispatched')",
    )?;
    let live_dispatch_ids: Vec<String> = stmt
        .query_map([card_id], |row| row.get(0))?
        .filter_map(|row| row.ok())
        .collect();
    drop(stmt);

    conn.execute(
        "UPDATE sessions \
         SET status = CASE WHEN status = 'working' THEN 'idle' ELSE status END, \
             active_dispatch_id = NULL \
         WHERE active_dispatch_id IN (
             SELECT id FROM task_dispatches
             WHERE kanban_card_id = ?1 AND status IN ('pending', 'dispatched')
        )",
        [card_id],
    )?;

    let cancel_payload =
        reason.map(|reason| json!({ "reason": reason, "completion_source": "force_transition" }));
    let mut cancelled = 0usize;
    for dispatch_id in live_dispatch_ids {
        cancelled += set_dispatch_status_on_conn(
            conn,
            &dispatch_id,
            "cancelled",
            cancel_payload.as_ref(),
            "force_transition",
            Some(&["pending", "dispatched"]),
            false,
        )
        .map_err(|e| {
            rusqlite::Error::ToSqlConversionFailure(Box::new(std::io::Error::other(e.to_string())))
        })?;
    }
    Ok(cancelled)
}

fn dispatch_uses_alt_channel(dispatch_type: &str) -> bool {
    matches!(dispatch_type, "review" | "e2e-test" | "consultation")
}

fn resolve_dispatch_channel_id(channel: &str) -> Option<u64> {
    channel
        .parse::<u64>()
        .ok()
        .or_else(|| crate::server::routes::dispatches::resolve_channel_alias_pub(channel))
}

fn load_existing_thread_for_channel(
    conn: &rusqlite::Connection,
    card_id: &str,
    channel_id: u64,
) -> Result<Option<String>> {
    let map_json: Option<String> = conn
        .query_row(
            "SELECT channel_thread_map FROM kanban_cards WHERE id = ?1",
            [card_id],
            |row| row.get(0),
        )
        .ok()
        .flatten();

    if let Some(json_str) = map_json.as_deref() {
        if !json_str.is_empty() && json_str != "{}" {
            let map: serde_json::Map<String, serde_json::Value> = serde_json::from_str(json_str)
                .map_err(|e| {
                    anyhow::anyhow!(
                        "Cannot create dispatch for card {}: invalid channel_thread_map JSON: {}",
                        card_id,
                        e
                    )
                })?;

            if let Some(value) = map.get(&channel_id.to_string()) {
                let thread_id = value.as_str().ok_or_else(|| {
                    anyhow::anyhow!(
                        "Cannot create dispatch for card {}: non-string thread mapping for channel {}",
                        card_id,
                        channel_id
                    )
                })?;
                return Ok(Some(thread_id.to_string()));
            }
            return Ok(None);
        }
    }

    Ok(conn
        .query_row(
            "SELECT active_thread_id FROM kanban_cards WHERE id = ?1 AND active_thread_id IS NOT NULL",
            [card_id],
            |row| row.get(0),
        )
        .ok())
}

fn validate_dispatch_target_on_conn(
    conn: &rusqlite::Connection,
    card_id: &str,
    to_agent_id: &str,
    dispatch_type: &str,
) -> Result<()> {
    let channel_role = if dispatch_uses_alt_channel(dispatch_type) {
        "counter-model"
    } else {
        "primary"
    };

    let channel_value: Option<String> =
        resolve_agent_dispatch_channel_on_conn(conn, to_agent_id, Some(dispatch_type))
            .ok()
            .flatten()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());

    let channel_value = channel_value.ok_or_else(|| {
        anyhow::anyhow!(
            "Cannot create {} dispatch: agent '{}' has no {} discord channel (card {})",
            dispatch_type,
            to_agent_id,
            channel_role,
            card_id
        )
    })?;

    let channel_id = resolve_dispatch_channel_id(&channel_value).ok_or_else(|| {
        anyhow::anyhow!(
            "Cannot create {} dispatch: agent '{}' has invalid {} discord channel '{}' (card {})",
            dispatch_type,
            to_agent_id,
            channel_role,
            channel_value,
            card_id
        )
    })?;

    if let Some(thread_id) = load_existing_thread_for_channel(conn, card_id, channel_id)? {
        if thread_id.parse::<u64>().is_err() {
            return Err(anyhow::anyhow!(
                "Cannot create {} dispatch: card '{}' has invalid thread '{}' for channel {}",
                dispatch_type,
                card_id,
                thread_id,
                channel_id
            ));
        }
    }

    Ok(())
}

fn create_dispatch_core_internal(
    db: &Db,
    dispatch_id: &str,
    kanban_card_id: &str,
    to_agent_id: &str,
    dispatch_type: &str,
    title: &str,
    context: &serde_json::Value,
    mut options: DispatchCreateOptions,
) -> Result<(String, String, bool)> {
    // Use separate_conn to avoid blocking request handlers while
    // engine/onTick holds the main DB Mutex via QuickJS.
    let conn = db
        .separate_conn()
        .map_err(|e| anyhow::anyhow!("DB conn error: {e}"))?;

    // Get current card status + repo/agent IDs for effective pipeline resolution
    let (old_status, card_repo_id, card_agent_id): (String, Option<String>, Option<String>) = conn
        .query_row(
            "SELECT status, repo_id, assigned_agent_id FROM kanban_cards WHERE id = ?1",
            [kanban_card_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .map_err(|e| anyhow::anyhow!("Card not found: {e}"))?;

    // Guard: reject dispatches to non-existent agents or invalid Discord routing
    // before any row is created.
    let agent_exists: bool = conn
        .query_row("SELECT 1 FROM agents WHERE id = ?1", [to_agent_id], |_| {
            Ok(())
        })
        .is_ok();
    if !agent_exists {
        return Err(anyhow::anyhow!(
            "Cannot create {} dispatch: agent '{}' not found (card {})",
            dispatch_type,
            to_agent_id,
            kanban_card_id
        ));
    }
    validate_dispatch_target_on_conn(&conn, kanban_card_id, to_agent_id, dispatch_type)?;
    if dispatch_context_requests_sidecar(context) {
        options.sidecar_dispatch = true;
    }

    // Guard: prevent ALL dispatches for terminal cards (pipeline-driven).
    crate::pipeline::ensure_loaded();
    let effective =
        crate::pipeline::resolve_for_card(&conn, card_repo_id.as_deref(), card_agent_id.as_deref());
    let is_terminal = effective.is_terminal(&old_status);
    if is_terminal && !options.sidecar_dispatch {
        return Err(anyhow::anyhow!(
            "Cannot create {} dispatch for terminal card {} (status: {}) — cannot revert terminal card",
            dispatch_type,
            kanban_card_id,
            old_status
        ));
    }

    // Dedup on the canonical path after validation so malformed targets do not
    // silently reuse an existing dispatch.
    if dispatch_type != "review-decision" {
        let existing_id: Option<String> = conn
            .query_row(
                "SELECT id FROM task_dispatches \
                 WHERE kanban_card_id = ?1 AND dispatch_type = ?2 \
                 AND status IN ('pending', 'dispatched') LIMIT 1",
                rusqlite::params![kanban_card_id, dispatch_type],
                |row| row.get(0),
            )
            .ok();
        if let Some(eid) = existing_id {
            tracing::info!(
                "DEDUP: reusing existing dispatch {} for card {} type {}",
                eid,
                kanban_card_id,
                dispatch_type
            );
            return Ok((eid, old_status, true));
        }
    }

    let (parent_dispatch_id, chain_depth) =
        resolve_parent_dispatch_context(&conn, kanban_card_id, context)?;

    let context_with_session_strategy =
        dispatch_context_with_session_strategy(dispatch_type, context);
    let context_str = if dispatch_type == "review" {
        build_review_context(
            db,
            kanban_card_id,
            to_agent_id,
            &context_with_session_strategy,
        )?
    } else {
        // #259: For ALL non-review dispatch types, prefer explicit worktree
        // context from the caller; otherwise inject the canonical issue
        // worktree so the session uses the right CWD instead of the repo root.
        let mut base = serde_json::to_string(&context_with_session_strategy)?;
        let worktree_target = if let Some((wt_path, wt_branch)) =
            dispatch_context_worktree_target(&context_with_session_strategy)?
        {
            Some((wt_path, wt_branch))
        } else {
            resolve_card_worktree(db, kanban_card_id)?
                .map(|(wt_path, wt_branch, _)| (wt_path, Some(wt_branch)))
        };

        if let Some((wt_path, wt_branch)) = worktree_target {
            if let Ok(mut obj) =
                serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(&base)
            {
                obj.entry("worktree_path".to_string())
                    .or_insert(json!(wt_path.clone()));
                if let Some(wt_branch) = wt_branch {
                    obj.entry("worktree_branch".to_string())
                        .or_insert(json!(wt_branch));
                }
                tracing::info!(
                    "[dispatch] {} dispatch for card {}: injecting worktree_path={}",
                    dispatch_type,
                    kanban_card_id,
                    wt_path
                );
                base = serde_json::to_string(&serde_json::Value::Object(obj)).unwrap_or(base);
            }
        }
        base
    };
    let is_review_type = dispatch_type == "review"
        || dispatch_type == "review-decision"
        || dispatch_type == "rework"
        || dispatch_type == "consultation";

    if dispatch_type == "review-decision" {
        let mut stmt = conn.prepare(
            "SELECT id FROM task_dispatches \
             WHERE kanban_card_id = ?1 AND dispatch_type = 'review-decision' \
             AND status IN ('pending', 'dispatched')",
        )?;
        let stale_ids: Vec<String> = stmt
            .query_map([kanban_card_id], |row| row.get::<_, String>(0))?
            .filter_map(|r| r.ok())
            .collect();
        drop(stmt);
        let mut cancelled = 0;
        for stale_id in &stale_ids {
            cancelled += cancel_dispatch_and_reset_auto_queue_on_conn(
                &conn,
                stale_id,
                Some("superseded_by_new_review_decision"),
            )?;
        }
        if cancelled > 0 {
            tracing::info!(
                "[dispatch] Cancelled {} stale review-decision(s) for card {} before creating new one",
                cancelled,
                kanban_card_id
            );
        }
    }

    apply_dispatch_attached_intents(
        &conn,
        kanban_card_id,
        to_agent_id,
        dispatch_id,
        dispatch_type,
        is_review_type,
        &old_status,
        &effective,
        title,
        &context_str,
        parent_dispatch_id.as_deref(),
        chain_depth,
        options,
    )?;

    Ok((dispatch_id.to_string(), old_status, false))
}

/// Core dispatch creation: DB operations only, no hooks fired.
///
/// - Inserts a record into `task_dispatches`
/// - Updates `kanban_cards.latest_dispatch_id` and sets status to "requested" (non-review)
/// - Returns `(dispatch_id, old_card_status)`
///
/// Caller is responsible for firing hooks after this returns.
///
/// Returns `(dispatch_id, old_card_status, reused)`.
/// When `reused` is true the returned ID belongs to an existing pending/dispatched
/// dispatch of the same type — no new row was inserted (#173 dedup).
pub fn create_dispatch_core(
    db: &Db,
    kanban_card_id: &str,
    to_agent_id: &str,
    dispatch_type: &str,
    title: &str,
    context: &serde_json::Value,
) -> Result<(String, String, bool)> {
    create_dispatch_core_with_options(
        db,
        kanban_card_id,
        to_agent_id,
        dispatch_type,
        title,
        context,
        DispatchCreateOptions::default(),
    )
}

pub fn create_dispatch_core_with_options(
    db: &Db,
    kanban_card_id: &str,
    to_agent_id: &str,
    dispatch_type: &str,
    title: &str,
    context: &serde_json::Value,
    options: DispatchCreateOptions,
) -> Result<(String, String, bool)> {
    let dispatch_id = uuid::Uuid::new_v4().to_string();
    create_dispatch_core_internal(
        db,
        &dispatch_id,
        kanban_card_id,
        to_agent_id,
        dispatch_type,
        title,
        context,
        options,
    )
}

/// Like `create_dispatch_core` but uses a pre-assigned dispatch ID (#121 intent model).
/// Called by the intent executor when processing CreateDispatch intents.
///
/// Returns `(dispatch_id, old_card_status, reused)` — see `create_dispatch_core` docs.
pub fn create_dispatch_core_with_id(
    db: &Db,
    dispatch_id: &str,
    kanban_card_id: &str,
    to_agent_id: &str,
    dispatch_type: &str,
    title: &str,
    context: &serde_json::Value,
) -> Result<(String, String, bool)> {
    create_dispatch_core_with_id_and_options(
        db,
        dispatch_id,
        kanban_card_id,
        to_agent_id,
        dispatch_type,
        title,
        context,
        DispatchCreateOptions::default(),
    )
}

pub fn create_dispatch_core_with_id_and_options(
    db: &Db,
    dispatch_id: &str,
    kanban_card_id: &str,
    to_agent_id: &str,
    dispatch_type: &str,
    title: &str,
    context: &serde_json::Value,
    options: DispatchCreateOptions,
) -> Result<(String, String, bool)> {
    create_dispatch_core_internal(
        db,
        dispatch_id,
        kanban_card_id,
        to_agent_id,
        dispatch_type,
        title,
        context,
        options,
    )
}

/// Create a new dispatch for a kanban card.
///
/// - Delegates DB work to `create_dispatch_core`
/// - Fires `OnCardTransition` hook (old_status -> requested)
///
/// Returns the full dispatch row as JSON.
pub fn create_dispatch(
    db: &Db,
    engine: &PolicyEngine,
    kanban_card_id: &str,
    to_agent_id: &str,
    dispatch_type: &str,
    title: &str,
    context: &serde_json::Value,
) -> Result<serde_json::Value> {
    create_dispatch_with_options(
        db,
        engine,
        kanban_card_id,
        to_agent_id,
        dispatch_type,
        title,
        context,
        DispatchCreateOptions::default(),
    )
}

pub fn create_dispatch_with_options(
    db: &Db,
    engine: &PolicyEngine,
    kanban_card_id: &str,
    to_agent_id: &str,
    dispatch_type: &str,
    title: &str,
    context: &serde_json::Value,
    options: DispatchCreateOptions,
) -> Result<serde_json::Value> {
    let options = DispatchCreateOptions {
        sidecar_dispatch: options.sidecar_dispatch || dispatch_context_requests_sidecar(context),
        ..options
    };
    let (dispatch_id, old_status, reused) = create_dispatch_core_with_options(
        db,
        kanban_card_id,
        to_agent_id,
        dispatch_type,
        title,
        context,
        options,
    )?;

    // Read back the dispatch
    let conn = db
        .separate_conn()
        .map_err(|e| anyhow::anyhow!("DB lock error: {e}"))?;
    let dispatch = query_dispatch_row(&conn, &dispatch_id)?;

    // #173: If dedup'd, skip hook firing — no new dispatch was created.
    if reused {
        let mut d = dispatch;
        // Signal to HTTP handler that this was a dedup'd response
        d["__reused"] = json!(true);
        return Ok(d);
    }

    if options.sidecar_dispatch {
        drop(conn);
        return Ok(dispatch);
    }

    // Fire pipeline-defined on_enter hooks for the kickoff state (#134).
    // Resolve kickoff state from card's effective pipeline (repo/agent overrides).
    crate::pipeline::ensure_loaded();
    let (card_repo_id, card_agent_id): (Option<String>, Option<String>) = conn
        .query_row(
            "SELECT repo_id, assigned_agent_id FROM kanban_cards WHERE id = ?1",
            [kanban_card_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap_or((None, None));
    let effective =
        crate::pipeline::resolve_for_card(&conn, card_repo_id.as_deref(), card_agent_id.as_deref());
    drop(conn);
    let kickoff_owned = effective.kickoff_for(&old_status).unwrap_or_else(|| {
        tracing::error!("Pipeline has no kickoff state for hook firing");
        effective.initial_state().to_string()
    });
    crate::kanban::fire_state_hooks(db, engine, kanban_card_id, &old_status, &kickoff_owned);

    Ok(dispatch)
}

/// Ensure a durable notify outbox row exists for a dispatch.
///
/// Used both by the authoritative dispatch creation transaction and by
/// fallback/backfill paths that must avoid duplicate notify entries.
pub(crate) fn ensure_dispatch_notify_outbox_on_conn(
    conn: &rusqlite::Connection,
    dispatch_id: &str,
    agent_id: &str,
    card_id: &str,
    title: &str,
) -> rusqlite::Result<bool> {
    let exists: bool = conn.query_row(
        "SELECT COUNT(*) > 0 FROM dispatch_outbox WHERE dispatch_id = ?1 AND action = 'notify'",
        [dispatch_id],
        |row| row.get(0),
    )?;
    if exists {
        return Ok(false);
    }

    conn.execute(
        "INSERT INTO dispatch_outbox (dispatch_id, action, agent_id, card_id, title) \
         VALUES (?1, 'notify', ?2, ?3, ?4)",
        rusqlite::params![dispatch_id, agent_id, card_id, title],
    )?;
    Ok(true)
}

/// Ensure a pending status-reaction outbox row exists for a dispatch.
///
/// At most one in-flight status sync is needed: when the worker drains it, the
/// Discord side-effect reads the latest dispatch status from `task_dispatches`.
/// Once an older row is already `done` or `failed`, a later transition should
/// enqueue a fresh row.
pub(crate) fn ensure_dispatch_status_reaction_outbox_on_conn(
    conn: &rusqlite::Connection,
    dispatch_id: &str,
) -> rusqlite::Result<bool> {
    let exists: bool = conn.query_row(
        "SELECT COUNT(*) > 0
         FROM dispatch_outbox
         WHERE dispatch_id = ?1
           AND action = 'status_reaction'
           AND status IN ('pending', 'processing')",
        [dispatch_id],
        |row| row.get(0),
    )?;
    if exists {
        return Ok(false);
    }

    conn.execute(
        "INSERT INTO dispatch_outbox (dispatch_id, action) VALUES (?1, 'status_reaction')",
        [dispatch_id],
    )?;
    Ok(true)
}

pub(crate) fn record_dispatch_status_event_on_conn(
    conn: &rusqlite::Connection,
    dispatch_id: &str,
    from_status: Option<&str>,
    to_status: &str,
    transition_source: &str,
    payload: Option<&serde_json::Value>,
) -> rusqlite::Result<()> {
    let (kanban_card_id, dispatch_type): (Option<String>, Option<String>) = conn
        .query_row(
            "SELECT kanban_card_id, dispatch_type FROM task_dispatches WHERE id = ?1",
            [dispatch_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?
        .unwrap_or((None, None));

    conn.execute(
        "INSERT INTO dispatch_events (
            dispatch_id,
            kanban_card_id,
            dispatch_type,
            from_status,
            to_status,
            transition_source,
            payload_json
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        rusqlite::params![
            dispatch_id,
            kanban_card_id,
            dispatch_type,
            from_status,
            to_status,
            transition_source,
            payload.map(|value| value.to_string()),
        ],
    )?;
    Ok(())
}

pub(crate) fn set_dispatch_status_on_conn(
    conn: &rusqlite::Connection,
    dispatch_id: &str,
    to_status: &str,
    result: Option<&serde_json::Value>,
    transition_source: &str,
    allowed_from: Option<&[&str]>,
    touch_completed_at: bool,
) -> Result<usize> {
    let current_status: Option<String> = conn
        .query_row(
            "SELECT status FROM task_dispatches WHERE id = ?1",
            [dispatch_id],
            |row| row.get(0),
        )
        .optional()?;
    let Some(current_status) = current_status else {
        return Ok(0);
    };

    if let Some(allowed_from) = allowed_from {
        if !allowed_from
            .iter()
            .any(|status| *status == current_status.as_str())
        {
            return Ok(0);
        }
    }

    conn.execute_batch("SAVEPOINT dispatch_status_transition")?;
    let update_result = (|| -> Result<usize> {
        let changed = match (result, touch_completed_at) {
            (Some(result), true) => conn.execute(
                "UPDATE task_dispatches
                 SET status = ?1,
                     result = ?2,
                     updated_at = datetime('now'),
                     completed_at = CASE
                         WHEN ?1 = 'completed' THEN COALESCE(completed_at, datetime('now'))
                         ELSE completed_at
                     END
                 WHERE id = ?3 AND status = ?4",
                rusqlite::params![to_status, result.to_string(), dispatch_id, current_status],
            )?,
            (Some(result), false) => conn.execute(
                "UPDATE task_dispatches
                 SET status = ?1,
                     result = ?2,
                     updated_at = datetime('now')
                 WHERE id = ?3 AND status = ?4",
                rusqlite::params![to_status, result.to_string(), dispatch_id, current_status],
            )?,
            (None, true) => conn.execute(
                "UPDATE task_dispatches
                 SET status = ?1,
                     updated_at = datetime('now'),
                     completed_at = CASE
                         WHEN ?1 = 'completed' THEN COALESCE(completed_at, datetime('now'))
                         ELSE completed_at
                     END
                 WHERE id = ?2 AND status = ?3",
                rusqlite::params![to_status, dispatch_id, current_status],
            )?,
            (None, false) => conn.execute(
                "UPDATE task_dispatches
                 SET status = ?1,
                     updated_at = datetime('now')
                 WHERE id = ?2 AND status = ?3",
                rusqlite::params![to_status, dispatch_id, current_status],
            )?,
        };

        if changed > 0 && current_status != to_status {
            record_dispatch_status_event_on_conn(
                conn,
                dispatch_id,
                Some(current_status.as_str()),
                to_status,
                transition_source,
                result,
            )?;

            if matches!(
                to_status,
                "dispatched" | "completed" | "failed" | "cancelled"
            ) {
                ensure_dispatch_status_reaction_outbox_on_conn(conn, dispatch_id)?;
            }
        }
        Ok(changed)
    })();

    match update_result {
        Ok(changed) => {
            conn.execute_batch("RELEASE dispatch_status_transition")?;
            Ok(changed)
        }
        Err(err) => {
            let _ = conn.execute_batch(
                "ROLLBACK TO dispatch_status_transition;
                 RELEASE dispatch_status_transition;",
            );
            Err(err)
        }
    }
}

/// #155: Insert dispatch row + apply DispatchAttached transition intents atomically.
///
/// Both the `task_dispatches` INSERT and the card-state intents execute inside
/// a single transaction so that reducer failure rolls back the dispatch row too.
/// #249 also inserts the notify outbox row inside the same transaction.
fn apply_dispatch_attached_intents(
    conn: &rusqlite::Connection,
    card_id: &str,
    to_agent_id: &str,
    dispatch_id: &str,
    dispatch_type: &str,
    is_review_type: bool,
    old_status: &str,
    effective: &crate::pipeline::PipelineConfig,
    title: &str,
    context_str: &str,
    parent_dispatch_id: Option<&str>,
    chain_depth: i64,
    options: DispatchCreateOptions,
) -> Result<()> {
    use crate::engine::transition::{
        self, CardState, GateSnapshot, TransitionContext, TransitionEvent, TransitionOutcome,
    };

    let kickoff_state = if !is_review_type && !options.sidecar_dispatch {
        Some(effective.kickoff_for(old_status).unwrap_or_else(|| {
            tracing::error!("Pipeline has no kickoff state — check pipeline configuration");
            effective.initial_state().to_string()
        }))
    } else {
        None
    };

    let ctx = TransitionContext {
        card: CardState {
            id: card_id.to_string(),
            status: old_status.to_string(),
            review_status: None,
            latest_dispatch_id: None,
        },
        pipeline: effective.clone(),
        gates: GateSnapshot::default(),
    };

    let decision = if options.sidecar_dispatch {
        transition::TransitionDecision {
            outcome: transition::TransitionOutcome::Allowed,
            intents: Vec::new(),
        }
    } else {
        transition::decide_transition(
            &ctx,
            &TransitionEvent::DispatchAttached {
                dispatch_id: dispatch_id.to_string(),
                dispatch_type: dispatch_type.to_string(),
                kickoff_state,
            },
        )
    };

    if let TransitionOutcome::Blocked(reason) = &decision.outcome {
        return Err(anyhow::anyhow!("{}", reason));
    }

    conn.execute_batch("BEGIN")?;
    let exec_result = (|| -> anyhow::Result<()> {
        // Insert dispatch row inside the transaction (#155 review fix)
        if let Err(e) = conn.execute(
            "INSERT INTO task_dispatches (
                id, kanban_card_id, to_agent_id, dispatch_type, status, title, context,
                parent_dispatch_id, chain_depth, created_at, updated_at
            )
             VALUES (?1, ?2, ?3, ?4, 'pending', ?5, ?6, ?7, ?8, datetime('now'), datetime('now'))",
            rusqlite::params![
                dispatch_id,
                card_id,
                to_agent_id,
                dispatch_type,
                title,
                context_str,
                parent_dispatch_id,
                chain_depth
            ],
        ) {
            if dispatch_type == "review-decision"
                && e.to_string().contains("UNIQUE constraint failed")
            {
                return Err(anyhow::anyhow!(
                    "review-decision already exists for card {} (concurrent race prevented by DB constraint)",
                    card_id
                ));
            }
            return Err(e.into());
        }
        record_dispatch_status_event_on_conn(
            conn,
            dispatch_id,
            None,
            "pending",
            "create_dispatch",
            None,
        )?;
        if !options.skip_outbox {
            ensure_dispatch_notify_outbox_on_conn(conn, dispatch_id, to_agent_id, card_id, title)?;
        }
        for intent in &decision.intents {
            transition::execute_intent_on_conn(conn, intent)?;
        }
        Ok(())
    })();
    if let Err(e) = exec_result {
        conn.execute_batch("ROLLBACK").ok();
        return Err(e);
    }
    conn.execute_batch("COMMIT")?;

    Ok(())
}

/// Single authority for dispatch completion.
///
/// All dispatch completion paths — turn_bridge explicit, recovery, API PATCH,
/// session idle — MUST route through this function.  It performs:
///   1. DB status update  (task_dispatches → completed)
///   2. OnDispatchCompleted hook firing  (pipeline event hooks)
///   3. Side-effect draining  (intents, transitions, follow-up dispatches)
///   4. Safety-net re-fire of OnReviewEnter (#139)
pub fn finalize_dispatch(
    db: &Db,
    engine: &PolicyEngine,
    dispatch_id: &str,
    completion_source: &str,
    context: Option<&serde_json::Value>,
) -> Result<serde_json::Value> {
    let result = match context {
        Some(ctx) => {
            let mut merged = ctx.clone();
            if let Some(obj) = merged.as_object_mut() {
                obj.insert(
                    "completion_source".to_string(),
                    serde_json::Value::String(completion_source.to_string()),
                );
            }
            merged
        }
        None => json!({ "completion_source": completion_source }),
    };
    complete_dispatch_inner(db, engine, dispatch_id, &result)
}

/// #143: DB-only dispatch completion — marks status='completed' without firing hooks.
///
/// Used by specialized paths (review_verdict, pm-decision) that fire their own
/// domain-specific hooks instead of the generic OnDispatchCompleted.
/// Returns the number of rows updated (0 = already completed/cancelled/not found).
pub fn mark_dispatch_completed(
    db: &Db,
    dispatch_id: &str,
    result: &serde_json::Value,
) -> Result<usize> {
    let conn = db
        .separate_conn()
        .map_err(|e| anyhow::anyhow!("DB conn error: {e}"))?;
    let changed = set_dispatch_status_on_conn(
        &conn,
        dispatch_id,
        "completed",
        Some(result),
        "mark_dispatch_completed",
        Some(&["pending", "dispatched"]),
        true,
    )?;
    Ok(changed)
}

/// Legacy wrapper — delegates to [`finalize_dispatch`] for callers that already
/// have a fully-formed result JSON (e.g. API PATCH handler).
#[cfg_attr(not(test), allow(dead_code))]
pub fn complete_dispatch(
    db: &Db,
    engine: &PolicyEngine,
    dispatch_id: &str,
    result: &serde_json::Value,
) -> Result<serde_json::Value> {
    complete_dispatch_inner(db, engine, dispatch_id, result)
}

fn complete_dispatch_inner(
    db: &Db,
    engine: &PolicyEngine,
    dispatch_id: &str,
    result: &serde_json::Value,
) -> Result<serde_json::Value> {
    let dispatch_span =
        crate::logging::dispatch_span("complete_dispatch", Some(dispatch_id), None, None);
    let _guard = dispatch_span.enter();
    let conn = db
        .separate_conn()
        .map_err(|e| anyhow::anyhow!("DB lock error: {e}"))?;

    validate_dispatch_completion_evidence_on_conn(&conn, dispatch_id, result)?;

    let changed = set_dispatch_status_on_conn(
        &conn,
        dispatch_id,
        "completed",
        Some(result),
        result
            .get("completion_source")
            .and_then(|value| value.as_str())
            .unwrap_or("complete_dispatch"),
        Some(&["pending", "dispatched"]),
        true,
    )?;

    if changed == 0 {
        // Either not found, already completed, or cancelled — skip hook firing
        let exists: bool = conn
            .query_row(
                "SELECT COUNT(*) > 0 FROM task_dispatches WHERE id = ?1",
                [dispatch_id],
                |row| row.get(0),
            )
            .unwrap_or(false);
        if exists {
            tracing::info!("skipping completion hooks because dispatch is already finalized");
            let dispatch = query_dispatch_row(&conn, dispatch_id)?;
            drop(conn);
            return Ok(dispatch);
        }
        return Err(anyhow::anyhow!("Dispatch not found: {dispatch_id}"));
    }

    let dispatch = query_dispatch_row(&conn, dispatch_id)?;

    let kanban_card_id: Option<String> = conn
        .query_row(
            "SELECT kanban_card_id FROM task_dispatches WHERE id = ?1",
            [dispatch_id],
            |row| row.get(0),
        )
        .ok();

    // Capture card status BEFORE hooks fire (used for audit/logging if needed)
    let _old_status: String = kanban_card_id
        .as_ref()
        .and_then(|cid| {
            conn.query_row(
                "SELECT status FROM kanban_cards WHERE id = ?1",
                [cid],
                |row| row.get(0),
            )
            .ok()
        })
        .unwrap_or_default();

    drop(conn);

    // Fire event hooks for dispatch completion (#134 — pipeline-defined events)
    crate::kanban::fire_event_hooks(
        db,
        engine,
        "on_dispatch_completed",
        "OnDispatchCompleted",
        json!({
            "dispatch_id": dispatch_id,
            "kanban_card_id": kanban_card_id,
            "result": result,
        }),
    );

    // After OnDispatchCompleted, policies may have queued follow-up transitions
    // and dispatch intents (OnReviewEnter, retry dispatches, etc.).
    crate::kanban::drain_hook_side_effects(db, engine);

    // #139/#220: Safety net — if card transitioned to review but OnReviewEnter
    // failed to create a review dispatch (engine lock contention causing
    // try_lock WouldBlock → hook deferred, JS error, etc.), re-fire
    // OnReviewEnter with a blocking lock to guarantee execution.
    // Uses fire_hook_by_name_blocking (lock() not try_lock()) so the hook
    // always runs — preserving all JS policy guards and state updates
    // (review_round, review_status, counter-model checks, etc.).
    {
        let needs_review_dispatch = db
            .lock()
            .ok()
            .map(|conn| {
                let (card_status, repo_id, agent_id): (Option<String>, Option<String>, Option<String>) = conn
                    .query_row(
                        "SELECT status, repo_id, assigned_agent_id FROM kanban_cards WHERE id = ?1",
                        [&kanban_card_id],
                        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                    )
                    .unwrap_or((None, None, None));
                let has_review_dispatch: bool = conn
                    .query_row(
                        "SELECT COUNT(*) > 0 FROM task_dispatches \
                         WHERE kanban_card_id = ?1 AND dispatch_type IN ('review', 'review-decision') \
                         AND status IN ('pending', 'dispatched')",
                        [&kanban_card_id],
                        |row| row.get(0),
                    )
                    .unwrap_or(false);
                let is_review_state = card_status.as_deref().map_or(false, |s| {
                    let eff = crate::pipeline::resolve_for_card(&conn, repo_id.as_deref(), agent_id.as_deref());
                    eff.hooks_for_state(s)
                        .map_or(false, |h| h.on_enter.iter().any(|n| n == "OnReviewEnter"))
                });
                is_review_state && !has_review_dispatch
            })
            .unwrap_or(false);

        if needs_review_dispatch {
            let cid = kanban_card_id.as_deref().unwrap_or("unknown");
            tracing::warn!(
                "[dispatch] Card {} in review-like state but no review dispatch — re-firing OnReviewEnter with blocking lock (#220)",
                cid
            );
            let _ = engine.fire_hook_by_name_blocking("OnReviewEnter", json!({ "card_id": cid }));
            crate::kanban::drain_hook_side_effects(db, engine);
        }
    }

    Ok(dispatch)
}

/// Read a single dispatch row as JSON.
pub fn query_dispatch_row(
    conn: &rusqlite::Connection,
    dispatch_id: &str,
) -> Result<serde_json::Value> {
    conn.query_row(
        "SELECT id, kanban_card_id, from_agent_id, to_agent_id, dispatch_type, status, title, context, result, parent_dispatch_id, chain_depth, created_at, updated_at, completed_at, COALESCE(retry_count, 0)
         FROM task_dispatches WHERE id = ?1",
        [dispatch_id],
        |row| {
            let status: String = row.get(5)?;
            let updated_at: String = row.get(12)?;
            let completed_at: Option<String> = row
                .get::<_, Option<String>>(13)?
                .or_else(|| (status == "completed").then(|| updated_at.clone()));
            Ok(json!({
                "id": row.get::<_, String>(0)?,
                "kanban_card_id": row.get::<_, Option<String>>(1)?,
                "from_agent_id": row.get::<_, Option<String>>(2)?,
                "to_agent_id": row.get::<_, Option<String>>(3)?,
                "dispatch_type": row.get::<_, Option<String>>(4)?,
                "status": status,
                "title": row.get::<_, Option<String>>(6)?,
                "context": row.get::<_, Option<String>>(7)?.and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok()),
                "result": row.get::<_, Option<String>>(8)?.and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok()),
                "parent_dispatch_id": row.get::<_, Option<String>>(9)?,
                "chain_depth": row.get::<_, i64>(10)?,
                "created_at": row.get::<_, String>(11)?,
                "updated_at": updated_at,
                "completed_at": completed_at,
                "retry_count": row.get::<_, i64>(14)?,
            }))
        },
    )
    .map_err(|e| anyhow::anyhow!("Dispatch query error: {e}"))
}

pub fn is_unified_thread_channel_active(channel_id: u64) -> bool {
    let _ = channel_id;
    false
}

/// Extract thread channel ID from a channel name's `-t{15+digit}` suffix.
/// Pure parsing — no DB access. Used by both production guards and tests.
#[cfg_attr(not(test), allow(dead_code))]
pub fn extract_thread_channel_id(channel_name: &str) -> Option<u64> {
    let pos = channel_name.rfind("-t")?;
    let suffix = &channel_name[pos + 2..];
    if suffix.len() >= 15 && suffix.chars().all(|c| c.is_ascii_digit()) {
        let id: u64 = suffix.parse().ok()?;
        if id == 0 { None } else { Some(id) }
    } else {
        None
    }
}

/// Check whether a channel name (from tmux session parsing) belongs to an active
/// unified-thread auto-queue run. Extracts the thread channel ID from the
/// `-t{15+digit}` suffix in the channel name.
pub fn is_unified_thread_channel_name_active(channel_name: &str) -> bool {
    let _ = channel_name;
    false
}

pub fn drain_unified_thread_kill_signals() -> Vec<String> {
    Vec::new()
}

/// Determine provider from a Discord channel name suffix.
fn provider_from_channel_suffix(channel: &str) -> Option<&'static str> {
    ProviderKind::from_channel_suffix(channel).and_then(|provider| match provider {
        ProviderKind::Claude => Some("claude"),
        ProviderKind::Codex => Some("codex"),
        ProviderKind::Gemini => Some("gemini"),
        ProviderKind::Qwen => Some("qwen"),
        ProviderKind::Unsupported(_) => None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
    use std::sync::MutexGuard;

    struct DispatchEnvOverride {
        _lock: MutexGuard<'static, ()>,
        previous_repo_dir: Option<String>,
        previous_config: Option<String>,
    }

    impl DispatchEnvOverride {
        fn new(repo_dir: Option<&str>, config_path: Option<&str>) -> Self {
            let lock = crate::services::discord::runtime_store::lock_test_env();
            let previous_repo_dir = std::env::var("AGENTDESK_REPO_DIR").ok();
            let previous_config = std::env::var("AGENTDESK_CONFIG").ok();

            match repo_dir {
                Some(path) => unsafe { std::env::set_var("AGENTDESK_REPO_DIR", path) },
                None => unsafe { std::env::remove_var("AGENTDESK_REPO_DIR") },
            }
            match config_path {
                Some(path) => unsafe { std::env::set_var("AGENTDESK_CONFIG", path) },
                None => unsafe { std::env::remove_var("AGENTDESK_CONFIG") },
            }

            Self {
                _lock: lock,
                previous_repo_dir,
                previous_config,
            }
        }
    }

    impl Drop for DispatchEnvOverride {
        fn drop(&mut self) {
            if let Some(value) = self.previous_repo_dir.as_deref() {
                unsafe { std::env::set_var("AGENTDESK_REPO_DIR", value) };
            } else {
                unsafe { std::env::remove_var("AGENTDESK_REPO_DIR") };
            }

            if let Some(value) = self.previous_config.as_deref() {
                unsafe { std::env::set_var("AGENTDESK_CONFIG", value) };
            } else {
                unsafe { std::env::remove_var("AGENTDESK_CONFIG") };
            }
        }
    }

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
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
        crate::db::schema::migrate(&conn).unwrap();
        let db = crate::db::wrap_conn(conn);
        // Seed common test agents with valid primary/alternate channels so the
        // canonical dispatch target validation can run in unit tests.
        {
            let c = db.separate_conn().unwrap();
            c.execute_batch(
                "INSERT OR IGNORE INTO agents (id, name, discord_channel_id, discord_channel_alt) VALUES ('agent-1', 'Agent 1', '111', '222');
                 INSERT OR IGNORE INTO agents (id, name, discord_channel_id, discord_channel_alt) VALUES ('agent-2', 'Agent 2', '333', '444');"
            ).unwrap();
        }
        db
    }

    fn test_engine(db: &Db) -> PolicyEngine {
        let config = crate::config::Config::default();
        PolicyEngine::new(&config, db.clone()).unwrap()
    }

    fn run_git(repo_dir: &str, args: &[&str]) -> std::process::Output {
        let output = Command::new("git")
            .args(args)
            .current_dir(repo_dir)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&output.stderr)
        );
        output
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
        let override_guard = RepoDirOverride::new(repo_dir);
        (repo, override_guard)
    }

    fn git_commit(repo_dir: &str, message: &str) -> String {
        run_git(repo_dir, &["commit", "--allow-empty", "-m", message]);
        crate::services::platform::git_head_commit(repo_dir).unwrap()
    }

    fn seed_card(db: &Db, card_id: &str, status: &str) {
        let conn = db.separate_conn().unwrap();
        conn.execute(
            "INSERT INTO kanban_cards (id, title, status, created_at, updated_at) VALUES (?1, 'Test Card', ?2, datetime('now'), datetime('now'))",
            rusqlite::params![card_id, status],
        )
        .unwrap();
    }

    fn set_card_issue_number(db: &Db, card_id: &str, issue_number: i64) {
        let conn = db.separate_conn().unwrap();
        conn.execute(
            "UPDATE kanban_cards SET github_issue_number = ?1 WHERE id = ?2",
            rusqlite::params![issue_number, card_id],
        )
        .unwrap();
    }

    fn set_card_repo_id(db: &Db, card_id: &str, repo_id: &str) {
        let conn = db.separate_conn().unwrap();
        conn.execute(
            "UPDATE kanban_cards SET repo_id = ?1 WHERE id = ?2",
            rusqlite::params![repo_id, card_id],
        )
        .unwrap();
    }

    fn write_repo_mapping_config(entries: &[(&str, &str)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let mut config = crate::config::Config::default();
        for (repo_id, repo_dir) in entries {
            config
                .github
                .repo_dirs
                .insert((*repo_id).to_string(), (*repo_dir).to_string());
        }
        crate::config::save_to_path(&dir.path().join("agentdesk.yaml"), &config).unwrap();
        dir
    }

    fn count_notify_outbox(conn: &rusqlite::Connection, dispatch_id: &str) -> i64 {
        conn.query_row(
            "SELECT COUNT(*) FROM dispatch_outbox WHERE dispatch_id = ?1 AND action = 'notify'",
            [dispatch_id],
            |row| row.get(0),
        )
        .unwrap()
    }

    fn count_status_reaction_outbox(conn: &rusqlite::Connection, dispatch_id: &str) -> i64 {
        conn.query_row(
            "SELECT COUNT(*) FROM dispatch_outbox WHERE dispatch_id = ?1 AND action = 'status_reaction'",
            [dispatch_id],
            |row| row.get(0),
        )
        .unwrap()
    }

    fn load_dispatch_events(
        conn: &rusqlite::Connection,
        dispatch_id: &str,
    ) -> Vec<(Option<String>, String, String)> {
        let mut stmt = conn
            .prepare(
                "SELECT from_status, to_status, transition_source
                 FROM dispatch_events
                 WHERE dispatch_id = ?1
                 ORDER BY id ASC",
            )
            .unwrap();
        stmt.query_map([dispatch_id], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })
        .unwrap()
        .filter_map(|row| row.ok())
        .collect()
    }

    fn seed_assistant_response_for_dispatch(db: &Db, dispatch_id: &str, message: &str) {
        crate::db::session_transcripts::persist_turn(
            db,
            crate::db::session_transcripts::PersistSessionTranscript {
                turn_id: &format!("dispatch-test:{dispatch_id}"),
                session_key: Some("dispatch-test-session"),
                channel_id: Some("123"),
                agent_id: Some("agent-1"),
                provider: Some("codex"),
                dispatch_id: Some(dispatch_id),
                user_message: "Implement the task",
                assistant_message: message,
                events: &[],
                duration_ms: None,
            },
        )
        .unwrap();
    }

    #[test]
    fn create_dispatch_inserts_and_updates_card() {
        let db = test_db();
        let engine = test_engine(&db);
        seed_card(&db, "card-1", "ready");

        let dispatch = create_dispatch(
            &db,
            &engine,
            "card-1",
            "agent-1",
            "implementation",
            "Do the thing",
            &json!({"key": "value"}),
        )
        .unwrap();

        assert_eq!(dispatch["status"], "pending");
        assert_eq!(dispatch["kanban_card_id"], "card-1");
        assert_eq!(dispatch["to_agent_id"], "agent-1");
        assert_eq!(dispatch["dispatch_type"], "implementation");
        assert_eq!(dispatch["title"], "Do the thing");

        // Card should be updated — #255: ready→requested is free, so kickoff_for("ready")
        // falls back to first dispatchable state target = "in_progress"
        let conn = db.separate_conn().unwrap();
        let (card_status, latest_dispatch_id): (String, String) = conn
            .query_row(
                "SELECT status, latest_dispatch_id FROM kanban_cards WHERE id = 'card-1'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(card_status, "in_progress");
        assert_eq!(latest_dispatch_id, dispatch["id"].as_str().unwrap());
    }

    #[test]
    fn create_dispatch_for_nonexistent_card_fails() {
        let db = test_db();
        let engine = test_engine(&db);

        let result = create_dispatch(
            &db,
            &engine,
            "nonexistent",
            "agent-1",
            "implementation",
            "title",
            &json!({}),
        );
        assert!(result.is_err());
    }

    #[test]
    fn complete_dispatch_updates_status() {
        let db = test_db();
        let engine = test_engine(&db);
        seed_card(&db, "card-2", "ready");

        let dispatch = create_dispatch(
            &db,
            &engine,
            "card-2",
            "agent-1",
            "implementation",
            "title",
            &json!({}),
        )
        .unwrap();
        let dispatch_id = dispatch["id"].as_str().unwrap().to_string();
        seed_assistant_response_for_dispatch(&db, &dispatch_id, "implemented");

        let completed =
            complete_dispatch(&db, &engine, &dispatch_id, &json!({"output": "done"})).unwrap();

        assert_eq!(completed["status"], "completed");
    }

    #[test]
    fn complete_dispatch_records_completed_at() {
        let db = test_db();
        let engine = test_engine(&db);
        seed_card(&db, "card-2-ts", "ready");

        let dispatch = create_dispatch(
            &db,
            &engine,
            "card-2-ts",
            "agent-1",
            "implementation",
            "title",
            &json!({}),
        )
        .unwrap();
        let dispatch_id = dispatch["id"].as_str().unwrap().to_string();
        seed_assistant_response_for_dispatch(&db, &dispatch_id, "implemented");

        let completed =
            complete_dispatch(&db, &engine, &dispatch_id, &json!({"output": "done"})).unwrap();

        assert!(
            completed["completed_at"].as_str().is_some(),
            "completion result must expose completed_at"
        );

        let conn = db.separate_conn().unwrap();
        let stored_completed_at: Option<String> = conn
            .query_row(
                "SELECT completed_at FROM task_dispatches WHERE id = ?1",
                [&dispatch_id],
                |row| row.get(0),
            )
            .unwrap();
        assert!(
            stored_completed_at.is_some(),
            "task_dispatches.completed_at must be stored for completed rows"
        );
    }

    #[test]
    fn complete_dispatch_nonexistent_fails() {
        let db = test_db();
        let engine = test_engine(&db);

        let result = complete_dispatch(&db, &engine, "nonexistent", &json!({}));
        assert!(result.is_err());
    }

    #[test]
    fn complete_dispatch_rejects_work_without_execution_evidence() {
        let db = test_db();
        let engine = test_engine(&db);
        seed_card(&db, "card-no-evidence", "ready");

        let dispatch = create_dispatch(
            &db,
            &engine,
            "card-no-evidence",
            "agent-1",
            "implementation",
            "title",
            &json!({}),
        )
        .unwrap();
        let dispatch_id = dispatch["id"].as_str().unwrap().to_string();

        let result = complete_dispatch(
            &db,
            &engine,
            &dispatch_id,
            &json!({"completion_source": "test_harness"}),
        );
        let err = format!("{}", result.unwrap_err());
        assert!(err.contains("no agent execution evidence"));
        assert_eq!(dispatch["status"], "pending");

        let conn = db.separate_conn().unwrap();
        let status: String = conn
            .query_row(
                "SELECT status FROM task_dispatches WHERE id = ?1",
                [&dispatch_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(status, "pending");
    }

    #[test]
    fn complete_dispatch_skips_cancelled() {
        let db = test_db();
        let engine = test_engine(&db);
        seed_card(&db, "card-cancel", "review");

        let dispatch = create_dispatch(
            &db,
            &engine,
            "card-cancel",
            "agent-1",
            "review-decision",
            "Decision",
            &json!({}),
        )
        .unwrap();
        let dispatch_id = dispatch["id"].as_str().unwrap().to_string();

        // Simulate dismiss: cancel the dispatch
        {
            let conn = db.separate_conn().unwrap();
            conn.execute(
                "UPDATE task_dispatches SET status = 'cancelled' WHERE id = ?1",
                [&dispatch_id],
            )
            .unwrap();
        }

        // Delayed completion attempt should NOT re-complete the cancelled dispatch
        let result = complete_dispatch(&db, &engine, &dispatch_id, &json!({"verdict": "pass"}));
        // Should return Ok (dispatch found) but status should remain cancelled
        assert!(result.is_ok());
        let returned = result.unwrap();
        assert_eq!(
            returned["status"], "cancelled",
            "cancelled dispatch must not be re-completed"
        );
    }

    #[test]
    fn cancel_dispatch_resets_linked_auto_queue_entry() {
        let db = test_db();
        let conn = db.separate_conn().unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO agents (id, name, discord_channel_id, discord_channel_alt) \
             VALUES ('agent-1', 'Agent 1', '123', '456')",
            [],
        )
        .unwrap();
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS auto_queue_runs (
                id TEXT PRIMARY KEY,
                repo TEXT,
                agent_id TEXT,
                status TEXT DEFAULT 'active'
            );
            CREATE TABLE IF NOT EXISTS auto_queue_entries (
                id TEXT PRIMARY KEY,
                run_id TEXT REFERENCES auto_queue_runs(id),
                kanban_card_id TEXT REFERENCES kanban_cards(id),
                agent_id TEXT,
                status TEXT DEFAULT 'pending',
                dispatch_id TEXT,
                dispatched_at DATETIME,
                completed_at DATETIME
            );
            CREATE TABLE IF NOT EXISTS auto_queue_entry_dispatch_history (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                entry_id TEXT NOT NULL,
                dispatch_id TEXT NOT NULL,
                trigger_source TEXT,
                created_at DATETIME DEFAULT CURRENT_TIMESTAMP,
                UNIQUE(entry_id, dispatch_id)
            );",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO kanban_cards (id, title, status, assigned_agent_id, created_at, updated_at) \
             VALUES ('card-aq', 'AQ Card', 'requested', 'agent-1', datetime('now'), datetime('now'))",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO task_dispatches (id, kanban_card_id, to_agent_id, dispatch_type, status, title, created_at, updated_at) \
             VALUES ('dispatch-aq', 'card-aq', 'agent-1', 'implementation', 'dispatched', 'AQ', datetime('now'), datetime('now'))",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_runs (id, repo, agent_id, status) VALUES ('run-aq', 'repo', 'agent-1', 'active')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (id, run_id, kanban_card_id, agent_id, status, dispatch_id, dispatched_at) \
             VALUES ('entry-aq', 'run-aq', 'card-aq', 'agent-1', 'dispatched', 'dispatch-aq', datetime('now'))",
            [],
        )
        .unwrap();

        let cancelled =
            cancel_dispatch_and_reset_auto_queue_on_conn(&conn, "dispatch-aq", Some("test"))
                .unwrap();
        assert_eq!(cancelled, 1);

        let dispatch_status: String = conn
            .query_row(
                "SELECT status FROM task_dispatches WHERE id = 'dispatch-aq'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(dispatch_status, "cancelled");

        let (entry_status, entry_dispatch_id): (String, Option<String>) = conn
            .query_row(
                "SELECT status, dispatch_id FROM auto_queue_entries WHERE id = 'entry-aq'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(entry_status, "pending");
        assert!(entry_dispatch_id.is_none());
        assert_eq!(
            load_dispatch_events(&conn, "dispatch-aq"),
            vec![(
                Some("dispatched".to_string()),
                "cancelled".to_string(),
                "cancel_dispatch".to_string()
            )],
            "dispatch cancellation must be audited"
        );
    }

    #[test]
    fn provider_from_channel_suffix_supports_gemini() {
        assert_eq!(provider_from_channel_suffix("agent-cc"), Some("claude"));
        assert_eq!(provider_from_channel_suffix("agent-cdx"), Some("codex"));
        assert_eq!(provider_from_channel_suffix("agent-gm"), Some("gemini"));
        assert_eq!(provider_from_channel_suffix("agent-qw"), Some("qwen"));
        assert_eq!(provider_from_channel_suffix("agent"), None);
    }

    #[test]
    fn review_context_uses_actual_review_channel_provider() {
        let db = test_db();
        seed_card(&db, "card-review-provider", "review");

        let conn = db.separate_conn().unwrap();
        conn.execute(
            "UPDATE agents
             SET provider = 'claude',
                 discord_channel_id = 'agent-cc',
                 discord_channel_alt = 'agent-qw',
                 discord_channel_cc = 'agent-cc',
                 discord_channel_cdx = 'agent-qw'
             WHERE id = 'agent-1'",
            [],
        )
        .unwrap();
        drop(conn);

        let context = build_review_context(
            &db,
            "card-review-provider",
            "agent-1",
            &json!({"reviewed_commit": "provider-fixture"}),
        )
        .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&context).unwrap();

        assert_eq!(parsed["reviewed_commit"], "provider-fixture");
        assert_eq!(parsed["from_provider"], "claude");
        assert_eq!(parsed["target_provider"], "qwen");
    }

    #[test]
    fn create_review_dispatch_for_done_card_rejected() {
        let db = test_db();
        let engine = test_engine(&db);
        seed_card(&db, "card-done", "done");

        for dispatch_type in &["review", "review-decision", "rework"] {
            let result = create_dispatch(
                &db,
                &engine,
                "card-done",
                "agent-1",
                dispatch_type,
                "Should fail",
                &json!({}),
            );
            assert!(
                result.is_err(),
                "{} dispatch should not be created for done card",
                dispatch_type
            );
        }

        // All dispatch types for done cards should be rejected
        let result = create_dispatch(
            &db,
            &engine,
            "card-done",
            "agent-1",
            "implementation",
            "Reopen work",
            &json!({}),
        );
        assert!(
            result.is_err(),
            "implementation dispatch should be rejected for done card"
        );
    }

    #[test]
    fn create_sidecar_phase_gate_for_terminal_card_preserves_card_state() {
        let db = test_db();
        let engine = test_engine(&db);
        seed_card(&db, "card-phase-gate", "done");

        let dispatch = create_dispatch(
            &db,
            &engine,
            "card-phase-gate",
            "agent-1",
            "phase-gate",
            "Phase Gate",
            &json!({
                "phase_gate": {
                    "run_id": "run-sidecar",
                    "batch_phase": 2,
                    "pass_verdict": "phase_gate_passed",
                }
            }),
        )
        .expect("phase gate sidecar dispatch should be allowed for terminal cards");

        let dispatch_id = dispatch["id"].as_str().unwrap().to_string();
        let conn = db.separate_conn().unwrap();
        let (card_status, latest_dispatch_id): (String, Option<String>) = conn
            .query_row(
                "SELECT status, latest_dispatch_id FROM kanban_cards WHERE id = 'card-phase-gate'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(card_status, "done");
        assert!(
            latest_dispatch_id.is_none(),
            "sidecar phase gate must not replace latest_dispatch_id"
        );
        assert_eq!(
            count_notify_outbox(&conn, &dispatch_id),
            1,
            "sidecar phase gate must still enqueue a notify outbox row"
        );
        assert_eq!(
            load_dispatch_events(&conn, &dispatch_id),
            vec![(None, "pending".to_string(), "create_dispatch".to_string())],
            "sidecar dispatch creation should still be audited"
        );
    }

    #[test]
    fn create_dispatch_core_shares_invariants_with_create_dispatch() {
        let db = test_db();
        let engine = test_engine(&db);
        seed_card(&db, "card-core", "ready");

        // create_dispatch_core returns (dispatch_id, old_status, reused)
        let (dispatch_id, old_status, _reused) = create_dispatch_core(
            &db,
            "card-core",
            "agent-1",
            "implementation",
            "Core dispatch",
            &json!({"key": "value"}),
        )
        .unwrap();

        assert_eq!(old_status, "ready");

        // #255: ready→requested is free, so kickoff_for("ready") returns "in_progress"
        let conn = db.separate_conn().unwrap();
        let (card_status, latest_dispatch_id): (String, String) = conn
            .query_row(
                "SELECT status, latest_dispatch_id FROM kanban_cards WHERE id = 'card-core'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(card_status, "in_progress");
        assert_eq!(latest_dispatch_id, dispatch_id);

        // Dispatch row exists
        let dispatch = query_dispatch_row(&conn, &dispatch_id).unwrap();
        assert_eq!(dispatch["status"], "pending");
        assert_eq!(dispatch["kanban_card_id"], "card-core");
        assert_eq!(
            count_notify_outbox(&conn, &dispatch_id),
            1,
            "core creation must atomically enqueue exactly one notify outbox row"
        );
        assert_eq!(
            load_dispatch_events(&conn, &dispatch_id),
            vec![(None, "pending".to_string(), "create_dispatch".to_string())],
            "dispatch creation must record the initial pending event"
        );
        drop(conn);

        // create_dispatch delegates to core — verify same invariants
        seed_card(&db, "card-full", "ready");
        let full_dispatch = create_dispatch(
            &db,
            &engine,
            "card-full",
            "agent-1",
            "implementation",
            "Full dispatch",
            &json!({}),
        )
        .unwrap();
        assert_eq!(full_dispatch["status"], "pending");
    }

    #[test]
    fn dispatch_type_force_new_session_defaults_split_by_dispatch_type() {
        assert_eq!(
            dispatch_type_force_new_session_default(Some("implementation")),
            Some(true)
        );
        assert_eq!(
            dispatch_type_force_new_session_default(Some("review")),
            Some(true)
        );
        assert_eq!(
            dispatch_type_force_new_session_default(Some("rework")),
            Some(true)
        );
        assert_eq!(
            dispatch_type_force_new_session_default(Some("review-decision")),
            Some(false)
        );
        assert_eq!(
            dispatch_type_force_new_session_default(Some("consultation")),
            None
        );
        assert_eq!(dispatch_type_force_new_session_default(None), None);
    }

    #[test]
    fn create_dispatch_core_injects_fresh_session_default_for_implementation() {
        let db = test_db();
        seed_card(&db, "card-session-default", "ready");

        let (dispatch_id, _, _) = create_dispatch_core(
            &db,
            "card-session-default",
            "agent-1",
            "implementation",
            "Fresh implementation",
            &json!({"key": "value"}),
        )
        .unwrap();

        let conn = db.separate_conn().unwrap();
        let context: String = conn
            .query_row(
                "SELECT context FROM task_dispatches WHERE id = ?1",
                [&dispatch_id],
                |row| row.get(0),
            )
            .unwrap();
        let context_json: serde_json::Value = serde_json::from_str(&context).unwrap();
        assert_eq!(context_json["force_new_session"], true);
        assert_eq!(context_json["key"], "value");
    }

    #[test]
    fn create_dispatch_core_keeps_explicit_session_override() {
        let db = test_db();
        seed_card(&db, "card-session-override", "ready");

        let (dispatch_id, _, _) = create_dispatch_core(
            &db,
            "card-session-override",
            "agent-1",
            "implementation",
            "Warm override",
            &json!({"force_new_session": false}),
        )
        .unwrap();

        let conn = db.separate_conn().unwrap();
        let context: String = conn
            .query_row(
                "SELECT context FROM task_dispatches WHERE id = ?1",
                [&dispatch_id],
                |row| row.get(0),
            )
            .unwrap();
        let context_json: serde_json::Value = serde_json::from_str(&context).unwrap();
        assert_eq!(context_json["force_new_session"], false);
    }

    #[test]
    fn create_dispatch_core_injects_warm_resume_default_for_review_decision() {
        let db = test_db();
        seed_card(&db, "card-session-review-decision", "review");

        let (dispatch_id, _, _) = create_dispatch_core(
            &db,
            "card-session-review-decision",
            "agent-1",
            "review-decision",
            "Warm review decision",
            &json!({"verdict": "improve"}),
        )
        .unwrap();

        let conn = db.separate_conn().unwrap();
        let context: String = conn
            .query_row(
                "SELECT context FROM task_dispatches WHERE id = ?1",
                [&dispatch_id],
                |row| row.get(0),
            )
            .unwrap();
        let context_json: serde_json::Value = serde_json::from_str(&context).unwrap();
        assert_eq!(context_json["force_new_session"], false);
        assert_eq!(context_json["verdict"], "improve");
    }

    #[test]
    fn create_dispatch_core_with_id_atomically_inserts_notify_outbox() {
        let db = test_db();
        seed_card(&db, "card-core-id", "ready");

        let (dispatch_id, old_status, reused) = create_dispatch_core_with_id(
            &db,
            "dispatch-core-id",
            "card-core-id",
            "agent-1",
            "implementation",
            "Core with id",
            &json!({}),
        )
        .unwrap();

        assert_eq!(dispatch_id, "dispatch-core-id");
        assert_eq!(old_status, "ready");
        assert!(!reused);

        let conn = db.separate_conn().unwrap();
        assert_eq!(
            count_notify_outbox(&conn, "dispatch-core-id"),
            1,
            "pre-assigned dispatch creation must also enqueue notify outbox inside the transaction"
        );
    }

    #[test]
    fn create_dispatch_core_with_id_and_skip_outbox_omits_notify_row() {
        let db = test_db();
        seed_card(&db, "card-core-id-skip", "ready");

        let (dispatch_id, old_status, reused) = create_dispatch_core_with_id_and_options(
            &db,
            "dispatch-core-id-skip",
            "card-core-id-skip",
            "agent-1",
            "implementation",
            "Core with id skip outbox",
            &json!({}),
            DispatchCreateOptions {
                skip_outbox: true,
                ..Default::default()
            },
        )
        .unwrap();

        assert_eq!(dispatch_id, "dispatch-core-id-skip");
        assert_eq!(old_status, "ready");
        assert!(!reused);

        let conn = db.separate_conn().unwrap();
        assert_eq!(
            count_notify_outbox(&conn, "dispatch-core-id-skip"),
            0,
            "skip_outbox must suppress notify outbox insertion inside the transaction"
        );
    }

    #[test]
    fn create_dispatch_core_rejects_done_card() {
        let db = test_db();
        seed_card(&db, "card-done-core", "done");

        let result = create_dispatch_core(
            &db,
            "card-done-core",
            "agent-1",
            "implementation",
            "Should fail",
            &json!({}),
        );
        assert!(result.is_err(), "core should reject done card dispatch");
    }

    #[test]
    fn create_dispatch_core_rejects_missing_agent_before_insert() {
        let db = test_db();
        seed_card(&db, "card-missing-agent", "ready");

        let result = create_dispatch_core(
            &db,
            "card-missing-agent",
            "agent-missing",
            "implementation",
            "Should fail",
            &json!({}),
        );
        let err = format!("{}", result.unwrap_err());
        assert!(err.contains("agent 'agent-missing' not found"));

        let conn = db.separate_conn().unwrap();
        let dispatch_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM task_dispatches WHERE kanban_card_id = 'card-missing-agent'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(dispatch_count, 0, "missing agent must not persist rows");
    }

    #[test]
    fn create_dispatch_core_rejects_missing_primary_channel_before_insert() {
        let db = test_db();
        seed_card(&db, "card-no-channel", "ready");
        let conn = db.separate_conn().unwrap();
        conn.execute(
            "UPDATE agents
             SET discord_channel_id = NULL,
                 discord_channel_alt = NULL,
                 discord_channel_cc = NULL,
                 discord_channel_cdx = NULL
             WHERE id = 'agent-1'",
            [],
        )
        .unwrap();
        drop(conn);

        let result = create_dispatch_core(
            &db,
            "card-no-channel",
            "agent-1",
            "implementation",
            "Should fail",
            &json!({}),
        );
        let err = format!("{}", result.unwrap_err());
        assert!(err.contains("no primary discord channel"));

        let conn = db.separate_conn().unwrap();
        let dispatch_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM task_dispatches WHERE kanban_card_id = 'card-no-channel'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(dispatch_count, 0, "failed validation must not persist rows");
    }

    #[test]
    fn create_dispatch_core_with_id_rejects_invalid_channel_alias_before_insert() {
        let db = test_db();
        seed_card(&db, "card-bad-channel", "ready");
        let conn = db.separate_conn().unwrap();
        conn.execute(
            "UPDATE agents SET discord_channel_id = 'not-a-channel' WHERE id = 'agent-1'",
            [],
        )
        .unwrap();
        drop(conn);

        let result = create_dispatch_core_with_id(
            &db,
            "dispatch-bad-channel",
            "card-bad-channel",
            "agent-1",
            "implementation",
            "Should fail",
            &json!({}),
        );
        let err = format!("{}", result.unwrap_err());
        assert!(err.contains("invalid primary discord channel"));

        let conn = db.separate_conn().unwrap();
        let dispatch_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM task_dispatches WHERE id = 'dispatch-bad-channel'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            dispatch_count, 0,
            "invalid channels must fail before INSERT"
        );
    }

    #[test]
    fn create_dispatch_core_rejects_invalid_existing_thread_before_insert() {
        let db = test_db();
        seed_card(&db, "card-bad-thread", "ready");
        let conn = db.separate_conn().unwrap();
        conn.execute(
            "UPDATE kanban_cards SET active_thread_id = 'thread-not-numeric' WHERE id = 'card-bad-thread'",
            [],
        )
        .unwrap();
        drop(conn);

        let result = create_dispatch_core(
            &db,
            "card-bad-thread",
            "agent-1",
            "implementation",
            "Should fail",
            &json!({}),
        );
        let err = format!("{}", result.unwrap_err());
        assert!(err.contains("invalid thread"));

        let conn = db.separate_conn().unwrap();
        let dispatch_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM task_dispatches WHERE kanban_card_id = 'card-bad-thread'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(dispatch_count, 0, "invalid thread must fail before INSERT");
    }

    #[test]
    fn concurrent_dispatches_for_different_cards_have_distinct_ids() {
        // Regression: concurrent dispatches from different cards must not share
        // dispatch IDs or card state — each must be independently routable.
        let db = test_db();
        let engine = test_engine(&db);
        seed_card(&db, "card-a", "ready");
        seed_card(&db, "card-b", "ready");

        let dispatch_a = create_dispatch(
            &db,
            &engine,
            "card-a",
            "agent-1",
            "implementation",
            "Task A",
            &json!({}),
        )
        .unwrap();

        let dispatch_b = create_dispatch(
            &db,
            &engine,
            "card-b",
            "agent-2",
            "implementation",
            "Task B",
            &json!({}),
        )
        .unwrap();

        let id_a = dispatch_a["id"].as_str().unwrap();
        let id_b = dispatch_b["id"].as_str().unwrap();
        assert_ne!(id_a, id_b, "dispatch IDs must be unique");
        assert_eq!(dispatch_a["kanban_card_id"], "card-a");
        assert_eq!(dispatch_b["kanban_card_id"], "card-b");

        // Each card's latest_dispatch_id points to its own dispatch
        let conn = db.separate_conn().unwrap();
        let latest_a: String = conn
            .query_row(
                "SELECT latest_dispatch_id FROM kanban_cards WHERE id = 'card-a'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let latest_b: String = conn
            .query_row(
                "SELECT latest_dispatch_id FROM kanban_cards WHERE id = 'card-b'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(latest_a, id_a);
        assert_eq!(latest_b, id_b);
        assert_ne!(latest_a, latest_b, "card dispatch IDs must not cross");
    }

    #[test]
    fn finalize_dispatch_sets_completion_source() {
        let db = test_db();
        let engine = test_engine(&db);
        seed_card(&db, "card-fin", "ready");

        let dispatch = create_dispatch(
            &db,
            &engine,
            "card-fin",
            "agent-1",
            "implementation",
            "Finalize test",
            &json!({}),
        )
        .unwrap();
        let dispatch_id = dispatch["id"].as_str().unwrap().to_string();
        seed_assistant_response_for_dispatch(&db, &dispatch_id, "implemented");

        let completed =
            finalize_dispatch(&db, &engine, &dispatch_id, "turn_bridge_explicit", None).unwrap();

        assert_eq!(completed["status"], "completed");
        // result is parsed JSON (query_dispatch_row parses it)
        assert_eq!(
            completed["result"]["completion_source"],
            "turn_bridge_explicit"
        );
    }

    #[test]
    fn finalize_dispatch_merges_context() {
        let db = test_db();
        let engine = test_engine(&db);
        seed_card(&db, "card-ctx", "ready");

        let dispatch = create_dispatch(
            &db,
            &engine,
            "card-ctx",
            "agent-1",
            "implementation",
            "Context test",
            &json!({}),
        )
        .unwrap();
        let dispatch_id = dispatch["id"].as_str().unwrap().to_string();

        let completed = finalize_dispatch(
            &db,
            &engine,
            &dispatch_id,
            "session_idle",
            Some(&json!({ "auto_completed": true, "agent_response_present": true })),
        )
        .unwrap();

        assert_eq!(completed["status"], "completed");
        assert_eq!(completed["result"]["completion_source"], "session_idle");
        assert_eq!(completed["result"]["auto_completed"], true);
    }

    #[test]
    fn dispatch_events_capture_dispatched_and_completed_transitions() {
        let db = test_db();
        let engine = test_engine(&db);
        seed_card(&db, "card-events", "ready");

        let dispatch = create_dispatch(
            &db,
            &engine,
            "card-events",
            "agent-1",
            "implementation",
            "Event trail",
            &json!({}),
        )
        .unwrap();
        let dispatch_id = dispatch["id"].as_str().unwrap().to_string();

        {
            let conn = db.separate_conn().unwrap();
            set_dispatch_status_on_conn(
                &conn,
                &dispatch_id,
                "dispatched",
                None,
                "test_dispatch_outbox",
                Some(&["pending"]),
                false,
            )
            .unwrap();
        }
        seed_assistant_response_for_dispatch(&db, &dispatch_id, "implemented");

        finalize_dispatch(&db, &engine, &dispatch_id, "test_complete", None).unwrap();

        let conn = db.separate_conn().unwrap();
        assert_eq!(
            load_dispatch_events(&conn, &dispatch_id),
            vec![
                (None, "pending".to_string(), "create_dispatch".to_string()),
                (
                    Some("pending".to_string()),
                    "dispatched".to_string(),
                    "test_dispatch_outbox".to_string()
                ),
                (
                    Some("dispatched".to_string()),
                    "completed".to_string(),
                    "test_complete".to_string()
                ),
            ],
            "dispatch event log must preserve ordered status transitions"
        );
    }

    #[test]
    fn dispatch_status_transitions_enqueue_status_reaction_outbox_entries() {
        let db = test_db();
        let engine = test_engine(&db);
        seed_card(&db, "card-reaction-outbox", "ready");

        let dispatch = create_dispatch(
            &db,
            &engine,
            "card-reaction-outbox",
            "agent-1",
            "implementation",
            "Reaction trail",
            &json!({}),
        )
        .unwrap();
        let dispatch_id = dispatch["id"].as_str().unwrap().to_string();

        {
            let conn = db.separate_conn().unwrap();
            set_dispatch_status_on_conn(
                &conn,
                &dispatch_id,
                "dispatched",
                None,
                "test_dispatch_outbox",
                Some(&["pending"]),
                false,
            )
            .unwrap();
        }

        let conn = db.separate_conn().unwrap();
        assert_eq!(count_status_reaction_outbox(&conn, &dispatch_id), 1);

        conn.execute(
            "UPDATE dispatch_outbox
             SET status = 'done', processed_at = datetime('now')
             WHERE dispatch_id = ?1 AND action = 'status_reaction'",
            [&dispatch_id],
        )
        .unwrap();

        set_dispatch_status_on_conn(
            &conn,
            &dispatch_id,
            "completed",
            Some(&json!({"completion_source":"test_complete"})),
            "test_complete",
            Some(&["dispatched"]),
            true,
        )
        .unwrap();

        assert_eq!(count_status_reaction_outbox(&conn, &dispatch_id), 2);

        set_dispatch_status_on_conn(
            &conn,
            &dispatch_id,
            "completed",
            Some(&json!({"completion_source":"test_complete"})),
            "test_complete_duplicate",
            Some(&["completed"]),
            true,
        )
        .unwrap();

        assert_eq!(
            count_status_reaction_outbox(&conn, &dispatch_id),
            2,
            "duplicate terminal transition must not enqueue extra status sync work"
        );
    }

    // ── #173 Dedup tests ─────────────────────────────────────────────

    #[test]
    fn dedup_same_card_same_type_returns_existing_dispatch() {
        let db = test_db();
        let engine = test_engine(&db);
        seed_card(&db, "card-dup", "ready");

        let d1 = create_dispatch(
            &db,
            &engine,
            "card-dup",
            "agent-1",
            "implementation",
            "First",
            &json!({}),
        )
        .unwrap();
        let id1 = d1["id"].as_str().unwrap();

        // Second call with same card + same type → should return existing
        let d2 = create_dispatch(
            &db,
            &engine,
            "card-dup",
            "agent-1",
            "implementation",
            "Second",
            &json!({}),
        )
        .unwrap();
        let id2 = d2["id"].as_str().unwrap();

        assert_eq!(id1, id2, "dedup must return existing dispatch_id");
        assert_eq!(d2["status"], "pending");

        // Only 1 row in DB
        let conn = db.separate_conn().unwrap();
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM task_dispatches \
                 WHERE kanban_card_id = 'card-dup' AND dispatch_type = 'implementation' \
                 AND status IN ('pending', 'dispatched')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1, "only one pending dispatch must exist");
    }

    #[test]
    fn dedup_same_card_different_type_allows_creation() {
        let (_repo, _override_guard) = setup_test_repo();
        let db = test_db();
        let engine = test_engine(&db);
        seed_card(&db, "card-diff", "review");

        // Create review dispatch
        let d1 = create_dispatch(
            &db,
            &engine,
            "card-diff",
            "agent-1",
            "review",
            "Review",
            &json!({}),
        )
        .unwrap();

        // Create review-decision for same card → different type, should succeed
        let d2 = create_dispatch(
            &db,
            &engine,
            "card-diff",
            "agent-1",
            "review-decision",
            "Decision",
            &json!({}),
        )
        .unwrap();

        assert_ne!(
            d1["id"].as_str().unwrap(),
            d2["id"].as_str().unwrap(),
            "different types must create distinct dispatches"
        );
    }

    #[test]
    fn dedup_completed_dispatch_allows_new_creation() {
        let db = test_db();
        let engine = test_engine(&db);
        seed_card(&db, "card-reopen", "ready");

        let d1 = create_dispatch(
            &db,
            &engine,
            "card-reopen",
            "agent-1",
            "implementation",
            "First",
            &json!({}),
        )
        .unwrap();
        let id1 = d1["id"].as_str().unwrap().to_string();

        // Complete the first dispatch
        seed_assistant_response_for_dispatch(&db, &id1, "implemented first attempt");
        complete_dispatch(&db, &engine, &id1, &json!({"output": "done"})).unwrap();

        // New dispatch of same type → should succeed (old one is completed)
        let d2 = create_dispatch(
            &db,
            &engine,
            "card-reopen",
            "agent-1",
            "implementation",
            "Second",
            &json!({}),
        )
        .unwrap();

        assert_ne!(
            id1,
            d2["id"].as_str().unwrap(),
            "completed dispatch must not block new creation"
        );
    }

    #[test]
    fn dedup_core_returns_reused_flag() {
        let db = test_db();
        seed_card(&db, "card-flag", "ready");

        let (id1, _, reused1) = create_dispatch_core(
            &db,
            "card-flag",
            "agent-1",
            "implementation",
            "First",
            &json!({}),
        )
        .unwrap();
        assert!(!reused1, "first creation must not be reused");

        let (id2, _, reused2) = create_dispatch_core(
            &db,
            "card-flag",
            "agent-1",
            "implementation",
            "Second",
            &json!({}),
        )
        .unwrap();
        assert!(reused2, "duplicate must be flagged as reused");
        assert_eq!(id1, id2);

        let conn = db.separate_conn().unwrap();
        assert_eq!(
            count_notify_outbox(&conn, &id1),
            1,
            "reused dispatch must not create a second notify outbox row"
        );
    }

    #[test]
    fn resolve_card_worktree_returns_none_without_issue_number() {
        let db = test_db();
        seed_card(&db, "card-no-issue", "ready");
        // Card has no github_issue_number → should return None
        let result = resolve_card_worktree(&db, "card-no-issue").unwrap();
        assert!(
            result.is_none(),
            "card without issue number should return None"
        );
    }

    #[test]
    fn non_review_dispatch_uses_card_repo_mapping_instead_of_default_repo() {
        let default_repo = init_test_repo();
        let default_repo_dir = default_repo.path().to_str().unwrap();
        let default_wt_dir = default_repo.path().join("wt-wrong-414");
        let default_wt_path = default_wt_dir.to_str().unwrap();
        run_git(
            default_repo_dir,
            &["worktree", "add", default_wt_path, "-b", "wt/wrong-414"],
        );
        std::fs::write(default_wt_dir.join("wrong.txt"), "wrong repo\n").unwrap();
        run_git(default_wt_path, &["add", "wrong.txt"]);
        run_git(default_wt_path, &["commit", "-m", "fix: wrong repo (#414)"]);

        let mapped_repo = init_test_repo();
        let mapped_repo_dir = mapped_repo.path().to_str().unwrap();
        let mapped_wt_dir = mapped_repo.path().join("wt-right-414");
        let mapped_wt_path = mapped_wt_dir.to_str().unwrap();
        run_git(
            mapped_repo_dir,
            &["worktree", "add", mapped_wt_path, "-b", "wt/right-414"],
        );
        std::fs::write(mapped_wt_dir.join("right.txt"), "right repo\n").unwrap();
        run_git(mapped_wt_path, &["add", "right.txt"]);
        run_git(mapped_wt_path, &["commit", "-m", "fix: mapped repo (#414)"]);

        let config_dir = write_repo_mapping_config(&[("owner/repo-b", mapped_repo_dir)]);
        let config_path = config_dir.path().join("agentdesk.yaml");
        let _env =
            DispatchEnvOverride::new(Some(default_repo_dir), Some(config_path.to_str().unwrap()));

        let db = test_db();
        let engine = test_engine(&db);
        seed_card(&db, "card-mapped", "ready");
        set_card_issue_number(&db, "card-mapped", 414);
        set_card_repo_id(&db, "card-mapped", "owner/repo-b");

        let dispatch = create_dispatch(
            &db,
            &engine,
            "card-mapped",
            "agent-1",
            "implementation",
            "Impl task",
            &json!({}),
        )
        .unwrap();

        let ctx = &dispatch["context"];
        let actual_wt_path = ctx["worktree_path"].as_str().unwrap();
        let canonical_actual = std::fs::canonicalize(actual_wt_path)
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let canonical_expected = std::fs::canonicalize(mapped_wt_path)
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let canonical_default = std::fs::canonicalize(default_wt_path)
            .unwrap()
            .to_string_lossy()
            .into_owned();

        assert_eq!(canonical_actual, canonical_expected);
        assert_eq!(ctx["worktree_branch"], "wt/right-414");
        assert_ne!(canonical_actual, canonical_default);
    }

    #[test]
    fn create_dispatch_rejects_missing_card_repo_mapping() {
        let default_repo = init_test_repo();
        let default_repo_dir = default_repo.path().to_str().unwrap();
        let _env = DispatchEnvOverride::new(Some(default_repo_dir), None);

        let db = test_db();
        seed_card(&db, "card-missing-mapping", "ready");
        set_card_issue_number(&db, "card-missing-mapping", 515);
        set_card_repo_id(&db, "card-missing-mapping", "owner/missing");

        let err = create_dispatch_core(
            &db,
            "card-missing-mapping",
            "agent-1",
            "implementation",
            "Should fail",
            &json!({}),
        )
        .expect_err("dispatch should fail when repo mapping is missing");

        assert!(
            err.to_string()
                .contains("No local repo mapping for 'owner/missing'"),
            "unexpected error: {err:#}"
        );

        let conn = db.separate_conn().unwrap();
        let dispatch_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM task_dispatches WHERE kanban_card_id = 'card-missing-mapping'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            dispatch_count, 0,
            "missing repo mapping must fail before INSERT"
        );
    }

    #[test]
    fn create_dispatch_uses_explicit_worktree_context_without_repo_mapping() {
        let (repo, _repo_override) = setup_test_repo();
        let repo_dir = repo.path().to_str().unwrap();
        run_git(repo_dir, &["checkout", "-b", "wt/explicit-515"]);

        let db = test_db();
        let engine = test_engine(&db);
        seed_card(&db, "card-explicit-worktree", "review");
        set_card_issue_number(&db, "card-explicit-worktree", 515);
        set_card_repo_id(&db, "card-explicit-worktree", "owner/missing");

        let dispatch = create_dispatch(
            &db,
            &engine,
            "card-explicit-worktree",
            "agent-1",
            "create-pr",
            "Create PR",
            &json!({
                "worktree_path": repo_dir,
                "worktree_branch": "wt/explicit-515",
                "branch": "wt/explicit-515",
            }),
        )
        .expect("explicit worktree context should bypass repo mapping lookup");

        let ctx = &dispatch["context"];
        assert_eq!(ctx["worktree_path"], repo_dir);
        assert_eq!(ctx["worktree_branch"], "wt/explicit-515");
        assert_eq!(ctx["branch"], "wt/explicit-515");
    }

    #[test]
    fn non_review_dispatch_injects_worktree_context() {
        // When resolve_card_worktree returns None (no issue), the context
        // should pass through unchanged (no worktree_path/worktree_branch).
        let db = test_db();
        seed_card(&db, "card-ctx", "ready");
        let engine = test_engine(&db);

        let dispatch = create_dispatch(
            &db,
            &engine,
            "card-ctx",
            "agent-1",
            "implementation",
            "Impl task",
            &json!({"custom_key": "custom_value"}),
        )
        .unwrap();

        // context is returned as parsed JSON by query_dispatch_row
        let ctx = &dispatch["context"];
        assert_eq!(ctx["custom_key"], "custom_value");
        // No issue number → no worktree injection
        assert!(
            ctx.get("worktree_path").is_none(),
            "no worktree_path without issue"
        );
        assert!(
            ctx.get("worktree_branch").is_none(),
            "no worktree_branch without issue"
        );
    }

    #[test]
    fn review_context_reuses_latest_completed_work_dispatch_target() {
        let db = test_db();
        seed_card(&db, "card-review-target", "review");

        let repo_dir = crate::services::platform::resolve_repo_dir()
            .or_else(|| {
                std::env::current_dir()
                    .ok()
                    .map(|path| path.display().to_string())
            })
            .unwrap();
        let completed_commit = crate::services::platform::git_head_commit(&repo_dir)
            .unwrap_or_else(|| "ci-detached-head".to_string());
        let completed_branch = crate::services::platform::shell::git_branch_name(&repo_dir);

        let conn = db.separate_conn().unwrap();
        conn.execute(
            "INSERT INTO task_dispatches (
                id, kanban_card_id, to_agent_id, dispatch_type, status, title, context, result, created_at, updated_at
             ) VALUES (
                'dispatch-review-target', 'card-review-target', 'agent-1', 'implementation', 'completed',
                'Done', ?1, ?2, datetime('now'), datetime('now')
             )",
            rusqlite::params![
                serde_json::json!({}).to_string(),
                serde_json::json!({
                    "completed_worktree_path": repo_dir.clone(),
                    "completed_branch": completed_branch.clone(),
                    "completed_commit": completed_commit.clone(),
                })
                .to_string(),
            ],
        )
        .unwrap();
        drop(conn);

        let context =
            build_review_context(&db, "card-review-target", "agent-1", &json!({})).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&context).unwrap();

        assert_eq!(parsed["reviewed_commit"], completed_commit);
        assert_eq!(parsed["worktree_path"], repo_dir);
        if let Some(branch) = completed_branch {
            assert_eq!(parsed["branch"], branch);
        }
    }

    #[test]
    fn review_context_includes_merge_base_for_branch_review() {
        let db = test_db();
        seed_card(&db, "card-review-merge-base", "review");

        let (repo, _repo_override) = setup_test_repo();
        let repo_dir = repo.path().to_str().unwrap();
        let fork_point = crate::services::platform::git_head_commit(repo_dir).unwrap();
        let wt_dir = repo.path().join("wt-542");
        let wt_path = wt_dir.to_str().unwrap();

        run_git(repo_dir, &["worktree", "add", wt_path, "-b", "wt/fix-542"]);
        let reviewed_commit = git_commit(wt_path, "fix: branch-only review target");
        let main_commit = git_commit(repo_dir, "chore: main advanced after fork");
        assert_ne!(fork_point, main_commit);

        let conn = db.separate_conn().unwrap();
        conn.execute(
            "INSERT INTO task_dispatches (
                id, kanban_card_id, to_agent_id, dispatch_type, status, title, context, result, created_at, updated_at
             ) VALUES (
                'dispatch-review-merge-base', 'card-review-merge-base', 'agent-1', 'implementation', 'completed',
                'Done', ?1, ?2, datetime('now'), datetime('now')
             )",
            rusqlite::params![
                serde_json::json!({}).to_string(),
                serde_json::json!({
                    "completed_worktree_path": wt_path,
                    "completed_branch": "wt/fix-542",
                    "completed_commit": reviewed_commit.clone(),
                })
                .to_string(),
            ],
        )
        .unwrap();
        drop(conn);

        let context =
            build_review_context(&db, "card-review-merge-base", "agent-1", &json!({})).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&context).unwrap();

        assert_eq!(parsed["reviewed_commit"], reviewed_commit);
        assert_eq!(parsed["branch"], "wt/fix-542");
        assert_eq!(parsed["merge_base"], fork_point);
    }

    #[test]
    fn review_context_skips_missing_merge_base_for_unknown_branch() {
        let mut obj = serde_json::Map::new();
        let (repo, _repo_override) = setup_test_repo();
        let repo_dir = repo.path().to_str().unwrap();
        let reviewed_commit = crate::services::platform::git_head_commit(repo_dir).unwrap();
        obj.insert("worktree_path".to_string(), json!(repo_dir));
        obj.insert("branch".to_string(), json!("missing-branch"));
        obj.insert("reviewed_commit".to_string(), json!(reviewed_commit));

        inject_review_merge_base_context(&mut obj);

        assert!(
            !obj.contains_key("merge_base"),
            "missing git merge-base must leave merge_base absent"
        );
    }

    #[test]
    fn review_context_accepts_latest_work_dispatch_commit_for_same_issue() {
        let db = test_db();
        seed_card(&db, "card-review-match", "review");
        set_card_issue_number(&db, "card-review-match", 305);

        let (repo, _repo_override) = setup_test_repo();
        let repo_dir = repo.path().to_str().unwrap();
        let matching_commit = git_commit(repo_dir, "fix: target commit (#305)");

        let conn = db.separate_conn().unwrap();
        conn.execute(
            "INSERT INTO task_dispatches (
                id, kanban_card_id, to_agent_id, dispatch_type, status, title, context, result, created_at, updated_at
             ) VALUES (
                'dispatch-review-match', 'card-review-match', 'agent-1', 'implementation', 'completed',
                'Done', ?1, ?2, datetime('now'), datetime('now')
             )",
            rusqlite::params![
                serde_json::json!({}).to_string(),
                serde_json::json!({
                    "completed_worktree_path": repo_dir,
                    "completed_branch": "main",
                    "completed_commit": matching_commit.clone(),
                })
                .to_string(),
            ],
        )
        .unwrap();
        drop(conn);

        let context =
            build_review_context(&db, "card-review-match", "agent-1", &json!({})).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&context).unwrap();

        assert_eq!(parsed["reviewed_commit"], matching_commit);
        assert_eq!(parsed["worktree_path"], repo_dir);
        assert_eq!(parsed["branch"], "main");
        assert!(
            parsed.get("merge_base").is_none(),
            "main branch reviews must not inject an empty merge-base diff"
        );
    }

    #[test]
    fn review_context_rejects_latest_work_dispatch_commit_from_other_issue() {
        let db = test_db();
        seed_card(&db, "card-review-mismatch", "review");
        set_card_issue_number(&db, "card-review-mismatch", 305);

        let (repo, _repo_override) = setup_test_repo();
        let repo_dir = repo.path().to_str().unwrap();
        let expected_commit = git_commit(repo_dir, "fix: target commit (#305)");
        let poisoned_commit = git_commit(repo_dir, "chore: unrelated (#999)");

        let conn = db.separate_conn().unwrap();
        conn.execute(
            "INSERT INTO task_dispatches (
                id, kanban_card_id, to_agent_id, dispatch_type, status, title, context, result, created_at, updated_at
             ) VALUES (
                'dispatch-review-mismatch', 'card-review-mismatch', 'agent-1', 'implementation', 'completed',
                'Done', ?1, ?2, datetime('now'), datetime('now')
             )",
            rusqlite::params![
                serde_json::json!({}).to_string(),
                serde_json::json!({
                    "completed_worktree_path": repo_dir,
                    "completed_branch": "main",
                    "completed_commit": poisoned_commit.clone(),
                })
                .to_string(),
            ],
        )
        .unwrap();
        drop(conn);

        let context =
            build_review_context(&db, "card-review-mismatch", "agent-1", &json!({})).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&context).unwrap();

        assert_eq!(parsed["reviewed_commit"], expected_commit);
        assert_ne!(parsed["reviewed_commit"], poisoned_commit);
        assert_eq!(parsed["worktree_path"], repo_dir);
        assert_eq!(parsed["branch"], "main");
    }

    #[test]
    fn review_context_skips_poisoned_non_main_worktree_when_latest_commit_does_not_match_issue() {
        let db = test_db();
        seed_card(&db, "card-review-worktree-fallback", "review");
        set_card_issue_number(&db, "card-review-worktree-fallback", 320);

        let (repo, _repo_override) = setup_test_repo();
        let repo_dir = repo.path().to_str().unwrap();
        let expected_commit = git_commit(repo_dir, "fix: target commit (#320)");
        let wt_dir = repo.path().join("wt-320");
        let wt_path = wt_dir.to_str().unwrap();

        run_git(
            repo_dir,
            &["worktree", "add", wt_path, "-b", "wt/320-phase6"],
        );
        let poisoned_commit = git_commit(wt_path, "chore: unrelated worktree drift (#999)");
        assert_ne!(
            expected_commit, poisoned_commit,
            "poisoned worktree head must differ from the issue commit fallback"
        );

        let conn = db.separate_conn().unwrap();
        conn.execute(
            "INSERT INTO task_dispatches (
                id, kanban_card_id, to_agent_id, dispatch_type, status, title, context, result, created_at, updated_at
             ) VALUES (
                'dispatch-review-worktree-fallback', 'card-review-worktree-fallback', 'agent-1', 'implementation', 'completed',
                'Done', ?1, ?2, datetime('now'), datetime('now')
             )",
            rusqlite::params![
                serde_json::json!({}).to_string(),
                serde_json::json!({
                    "completed_worktree_path": wt_path,
                    "completed_branch": "wt/320-phase6",
                    "completed_commit": poisoned_commit.clone(),
                })
                .to_string(),
            ],
        )
        .unwrap();
        drop(conn);

        let context =
            build_review_context(&db, "card-review-worktree-fallback", "agent-1", &json!({}))
                .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&context).unwrap();

        assert_eq!(parsed["worktree_path"], repo_dir);
        assert_eq!(parsed["branch"], "main");
        assert_eq!(parsed["reviewed_commit"], expected_commit);
        assert_ne!(parsed["reviewed_commit"], poisoned_commit);
    }

    #[test]
    fn review_context_rejects_repo_head_fallback_when_repo_root_is_dirty() {
        let db = test_db();
        seed_card(&db, "card-review-dirty-root", "review");

        let (repo, _repo_override) = setup_test_repo();
        let repo_dir = repo.path().to_str().unwrap();
        std::fs::write(repo.path().join("tracked.txt"), "baseline\n").unwrap();
        run_git(repo_dir, &["add", "tracked.txt"]);
        run_git(repo_dir, &["commit", "-m", "feat: add tracked file"]);
        std::fs::write(repo.path().join("tracked.txt"), "dirty\n").unwrap();

        let err = build_review_context(&db, "card-review-dirty-root", "agent-1", &json!({}))
            .expect_err("dirty repo root must block repo HEAD fallback");

        assert!(
            err.to_string()
                .contains("repo-root HEAD fallback is unsafe while tracked changes exist"),
            "unexpected error: {err:#}"
        );
    }

    #[test]
    fn review_context_rejects_commitless_completed_work_when_repo_root_is_dirty() {
        let db = test_db();
        seed_card(&db, "card-review-dirty-completion", "review");

        let (repo, _repo_override) = setup_test_repo();
        let repo_dir = repo.path().to_str().unwrap();
        std::fs::write(repo.path().join("tracked.txt"), "baseline\n").unwrap();
        run_git(repo_dir, &["add", "tracked.txt"]);
        run_git(repo_dir, &["commit", "-m", "feat: add tracked file"]);
        std::fs::write(repo.path().join("tracked.txt"), "dirty\n").unwrap();

        let conn = db.separate_conn().unwrap();
        conn.execute(
            "INSERT INTO task_dispatches (
                id, kanban_card_id, to_agent_id, dispatch_type, status, title, context, result, created_at, updated_at
             ) VALUES (
                'dispatch-review-dirty-completion', 'card-review-dirty-completion', 'agent-1', 'implementation', 'completed',
                'Implemented without commit', ?1, ?2, datetime('now'), datetime('now')
             )",
            rusqlite::params![
                serde_json::json!({}).to_string(),
                serde_json::json!({}).to_string(),
            ],
        )
        .unwrap();
        drop(conn);

        let err = build_review_context(&db, "card-review-dirty-completion", "agent-1", &json!({}))
            .expect_err("dirty repo root must block fallback after commitless completion");

        assert!(
            err.to_string()
                .contains("repo-root HEAD fallback is unsafe while tracked changes exist"),
            "unexpected error: {err:#}"
        );
    }

    #[test]
    fn review_context_rejects_explicit_noop_latest_work_dispatch() {
        let db = test_db();
        seed_card(&db, "card-review-noop", "review");

        let conn = db.separate_conn().unwrap();
        conn.execute(
            "INSERT INTO task_dispatches (
                id, kanban_card_id, to_agent_id, dispatch_type, status, title, context, result, created_at, updated_at
             ) VALUES (
                'dispatch-review-noop', 'card-review-noop', 'agent-1', 'implementation', 'completed',
                'No changes needed', ?1, ?2, datetime('now'), datetime('now')
             )",
            rusqlite::params![
                serde_json::json!({}).to_string(),
                serde_json::json!({
                    "work_outcome": "noop",
                    "completed_without_changes": true,
                    "notes": "spec already satisfied",
                })
                .to_string(),
            ],
        )
        .unwrap();
        drop(conn);

        let err = build_review_context(&db, "card-review-noop", "agent-1", &json!({}))
            .expect_err("explicit noop work must not create a review dispatch");
        assert!(
            err.to_string()
                .contains("latest completed work dispatch is noop"),
            "unexpected error: {err:#}"
        );
    }

    #[test]
    fn review_context_recovers_issue_branch_from_reviewed_commit_membership() {
        let db = test_db();
        seed_card(&db, "card-review-contains-branch", "review");
        set_card_issue_number(&db, "card-review-contains-branch", 610);

        let (repo, _repo_override) = setup_test_repo();
        let repo_dir = repo.path().to_str().unwrap();
        run_git(repo_dir, &["checkout", "-b", "feat/610-review"]);
        let reviewed_commit = git_commit(repo_dir, "fix: recover branch from commit (#610)");
        let fork_point = run_git(repo_dir, &["rev-parse", "HEAD^"]);
        let fork_point = String::from_utf8_lossy(&fork_point.stdout)
            .trim()
            .to_string();
        run_git(repo_dir, &["checkout", "main"]);

        let context =
            build_review_context(&db, "card-review-contains-branch", "agent-1", &json!({}))
                .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&context).unwrap();

        assert_eq!(parsed["reviewed_commit"], reviewed_commit);
        assert_eq!(parsed["worktree_path"], repo_dir);
        assert_eq!(parsed["branch"], "feat/610-review");
        assert_eq!(parsed["merge_base"], fork_point);
    }

    #[test]
    fn review_context_includes_quality_checklist_and_verdict_guidance() {
        let db = test_db();
        seed_card(&db, "card-review-quality", "review");

        let (repo, _repo_override) = setup_test_repo();
        let repo_dir = repo.path().to_str().unwrap();
        let completed_commit = crate::services::platform::git_head_commit(repo_dir).unwrap();

        let conn = db.separate_conn().unwrap();
        conn.execute(
            "INSERT INTO task_dispatches (
                id, kanban_card_id, to_agent_id, dispatch_type, status, title, context, result, created_at, updated_at
             ) VALUES (
                'dispatch-review-quality', 'card-review-quality', 'agent-1', 'implementation', 'completed',
                'Done', ?1, ?2, datetime('now'), datetime('now')
             )",
            rusqlite::params![
                serde_json::json!({}).to_string(),
                serde_json::json!({
                    "completed_worktree_path": repo_dir,
                    "completed_branch": "main",
                    "completed_commit": completed_commit,
                })
                .to_string(),
            ],
        )
        .unwrap();
        drop(conn);

        let context =
            build_review_context(&db, "card-review-quality", "agent-1", &json!({})).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&context).unwrap();
        let checklist = parsed["review_quality_checklist"]
            .as_array()
            .expect("checklist array must exist");

        assert_eq!(
            parsed["review_quality_scope_reminder"],
            REVIEW_QUALITY_SCOPE_REMINDER
        );
        assert_eq!(
            parsed["review_verdict_guidance"],
            REVIEW_VERDICT_IMPROVE_GUIDANCE
        );
        assert_eq!(checklist.len(), REVIEW_QUALITY_CHECKLIST.len());
        assert!(checklist.iter().any(|item| {
            item.as_str()
                .unwrap_or_default()
                .contains("race condition / 동시성 이슈")
        }));
        assert!(checklist.iter().any(|item| {
            item.as_str()
                .unwrap_or_default()
                .contains("에러 핸들링 누락")
        }));
    }
}
