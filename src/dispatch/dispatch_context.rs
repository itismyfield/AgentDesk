use anyhow::Result;
use rusqlite::OptionalExtension;
use serde_json::json;

use crate::db::Db;
use crate::db::agents::load_agent_channel_bindings;
use crate::services::provider::ProviderKind;

use super::dispatch_channel::provider_from_channel_suffix;

#[derive(Debug, Clone, PartialEq, Eq)]
struct DispatchExecutionTarget {
    reviewed_commit: String,
    branch: Option<String>,
    worktree_path: Option<String>,
    target_repo: Option<String>,
}

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

pub(crate) fn dispatch_type_force_new_session_default(dispatch_type: Option<&str>) -> Option<bool> {
    match dispatch_type {
        Some("implementation") | Some("review") | Some("rework") => Some(true),
        Some("review-decision") => Some(false),
        _ => None,
    }
}

pub(crate) fn dispatch_type_uses_thread_routing(dispatch_type: Option<&str>) -> bool {
    !matches!(dispatch_type, Some("phase-gate"))
}

pub(super) fn dispatch_context_with_session_strategy(
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

pub(super) fn resolve_parent_dispatch_context(
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

fn load_card_issue_repo(db: &Db, card_id: &str) -> Option<(Option<i64>, Option<String>)> {
    load_card_dispatch_info(db, card_id).map(|info| (info.issue_number, info.repo_id))
}

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

pub(crate) fn inject_review_dispatch_identifiers(
    db: &Db,
    card_id: &str,
    dispatch_type: &str,
    obj: &mut serde_json::Map<String, serde_json::Value>,
) {
    let snapshot = serde_json::Value::Object(obj.clone());
    let repo = json_string_field(&snapshot, "repo")
        .or_else(|| json_string_field(&snapshot, "target_repo"))
        .map(str::to_string)
        .or_else(|| resolve_card_target_repo_ref(db, card_id, Some(&snapshot)));
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
pub(super) fn resolve_card_target_repo_ref(
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

fn resolve_card_repo_dir_with_context(
    db: &Db,
    card_id: &str,
    context: Option<&serde_json::Value>,
    purpose: &str,
) -> Result<Option<String>> {
    let target_repo = resolve_card_target_repo_ref(db, card_id, context);
    crate::services::platform::shell::resolve_repo_dir_for_target(target_repo.as_deref())
        .map_err(|e| anyhow::anyhow!("Cannot {purpose} for card {}: {}", card_id, e))
}

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

fn git_commit_exists(dir: &str, commit_sha: &str) -> bool {
    std::process::Command::new("git")
        .args(["cat-file", "-e", &format!("{commit_sha}^{{commit}}")])
        .current_dir(dir)
        .output()
        .ok()
        .is_some_and(|output| output.status.success())
}

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

fn refresh_review_target_worktree(
    db: &Db,
    card_id: &str,
    context: &serde_json::Value,
    target: &DispatchExecutionTarget,
) -> Result<Option<DispatchExecutionTarget>> {
    if target
        .worktree_path
        .as_deref()
        .is_some_and(|path| std::path::Path::new(path).is_dir())
    {
        return Ok(Some(target.clone()));
    }

    if let Some(stale_path) = target.worktree_path.as_deref() {
        tracing::warn!(
            "[dispatch] Review dispatch for card {}: latest work target path '{}' no longer exists — attempting fallback",
            card_id,
            stale_path
        );
    }

    if let Some((wt_path, wt_branch, _wt_commit)) =
        resolve_card_worktree(db, card_id, Some(context))?
    {
        if git_commit_exists(&wt_path, &target.reviewed_commit) {
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
            "[dispatch] Review dispatch for card {}: active issue worktree does not contain commit {} — skipping path refresh",
            card_id,
            &target.reviewed_commit[..8.min(target.reviewed_commit.len())]
        );
    }

    if let Some(repo_dir) = resolve_card_repo_dir_with_context(
        db,
        card_id,
        Some(context),
        "recover review target repo",
    )? {
        if git_commit_exists(&repo_dir, &target.reviewed_commit) {
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
    }

    tracing::warn!(
        "[dispatch] Review dispatch for card {}: no usable worktree or repo path contains commit {} after stale worktree cleanup",
        card_id,
        &target.reviewed_commit[..8.min(target.reviewed_commit.len())]
    );
    Ok(None)
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
    let target_repo = context_json
        .as_ref()
        .and_then(|v| json_string_field(v, "target_repo"))
        .or_else(|| {
            result_json
                .as_ref()
                .and_then(|v| json_string_field(v, "target_repo"))
        })
        .map(str::to_string)
        .or_else(|| resolve_card_target_repo_ref(db, kanban_card_id, None));

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

pub(super) fn validate_dispatch_completion_evidence_on_conn(
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
pub(crate) fn resolve_card_worktree(
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
        target_repo: resolve_card_target_repo_ref(db, card_id, context),
    }))
}

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
        target.target_repo = resolve_card_target_repo_ref(db, kanban_card_id, context);
        target
    }))
}

/// Build the context JSON string for a review dispatch.
///
/// Injects `reviewed_commit`, `branch`, `worktree_path`, and provider info.
/// Prefers worktree branch (if found for this card's issue) over main HEAD.
pub(super) fn build_review_context(
    db: &Db,
    kanban_card_id: &str,
    to_agent_id: &str,
    context: &serde_json::Value,
) -> Result<String> {
    let mut ctx_val = dispatch_context_with_session_strategy("review", context);
    let target_repo = resolve_card_target_repo_ref(db, kanban_card_id, Some(&ctx_val));
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
                    if card_issue_number.is_none() {
                        Some(target.clone())
                    } else {
                        refresh_review_target_worktree(db, kanban_card_id, &ctx_snapshot, target)?
                    }
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
                if let Some((ref wt_path, ref wt_branch, ref wt_commit)) =
                    resolve_card_worktree(db, kanban_card_id, Some(&ctx_snapshot))?
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
        inject_review_dispatch_identifiers(db, kanban_card_id, "review", obj);

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Db;
    use crate::engine::PolicyEngine;
    use serde_json::json;
    use std::process::Command;
    use std::sync::MutexGuard;

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
        PolicyEngine::new(&config, db.clone()).unwrap()
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

        let context = build_review_context(
            &db,
            "card-review-identifiers",
            "agent-1",
            &json!({
                "worktree_path": repo_dir,
                "branch": "wt/692-review",
                "reviewed_commit": reviewed_commit,
            }),
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
}
