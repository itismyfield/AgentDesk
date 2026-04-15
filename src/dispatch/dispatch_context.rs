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
    description: Option<String>,
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
            "SELECT github_issue_number, repo_id, description FROM kanban_cards WHERE id = ?1",
            [card_id],
            |row| {
                Ok(CardDispatchInfo {
                    issue_number: row.get(0)?,
                    repo_id: row.get(1)?,
                    description: row.get(2)?,
                })
            },
        )
        .ok()
    })
}

fn load_card_issue_repo(db: &Db, card_id: &str) -> Option<(Option<i64>, Option<String>)> {
    load_card_dispatch_info(db, card_id).map(|info| (info.issue_number, info.repo_id))
}

fn normalize_target_repo_token(raw: &str) -> Option<String> {
    let trimmed = raw
        .trim_matches(|ch: char| matches!(ch, '`' | '"' | '\'' | '(' | ')' | '[' | ']' | '{' | '}'))
        .trim_end_matches(|ch: char| matches!(ch, '.' | ',' | ':' | ';'));
    if trimmed.is_empty() {
        return None;
    }
    Some(trimmed.to_string())
}

fn resolve_target_repo_path_candidate(raw: &str) -> Option<String> {
    let candidate = normalize_target_repo_token(raw)?;
    if !crate::services::platform::shell::looks_like_explicit_repo_path(&candidate) {
        return None;
    }
    crate::services::platform::shell::resolve_repo_dir_for_target(Some(&candidate))
        .ok()
        .flatten()
}

fn extract_target_repo_from_description(description: &str) -> Option<String> {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let labeled = RE.get_or_init(|| {
        regex::Regex::new(
            r"(?i)(?:target_repo|target repo|repo_path|repo path|repo dir|external repo(?: path)?)\s*[:=]\s*([^\s]+)",
        )
        .expect("target repo description regex must compile")
    });
    for caps in labeled.captures_iter(description) {
        if let Some(resolved) = caps
            .get(1)
            .and_then(|value| resolve_target_repo_path_candidate(value.as_str()))
        {
            return Some(resolved);
        }
    }

    description
        .split_whitespace()
        .filter_map(resolve_target_repo_path_candidate)
        .next()
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
    info.description
        .as_deref()
        .and_then(extract_target_repo_from_description)
        .or(info.repo_id)
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
    if let Some(obj) = ctx_val.as_object_mut() {
        if !obj.contains_key("reviewed_commit") {
            let latest_work_target = latest_completed_work_dispatch_target(db, kanban_card_id);
            let validated_work_target = latest_work_target.as_ref().filter(|t| {
                let valid = commit_belongs_to_card_issue(
                    db,
                    kanban_card_id,
                    &t.reviewed_commit,
                    t.target_repo.as_deref().or(target_repo.as_deref()),
                );
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
                apply_review_target_context(target, obj);
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
