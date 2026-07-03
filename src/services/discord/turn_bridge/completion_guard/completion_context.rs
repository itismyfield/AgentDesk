//! Work-dispatch completion context + commit attribution split out of
//! `completion_guard.rs` (#3479). Resolves the completed commit/branch for an
//! implementation/rework dispatch from agent output, Postgres completion
//! hints, and git history, and builds the structured completion result.
//!
//! Behaviour-preserving verbatim extraction; only visibility was adjusted.

use super::super::super::*;
use crate::services::git::GitCommand;
use sqlx::Row;

fn decode_pg_completion_hint_field<T>(
    decoded: Result<Option<T>, sqlx::Error>,
    dispatch_id: &str,
    column: &'static str,
) -> Option<T> {
    match decoded {
        Ok(value) => value,
        Err(error) => {
            tracing::warn!(
                dispatch_id = %dispatch_id,
                column,
                "failed to decode postgres completion hint field: {error}"
            );
            None
        }
    }
}

/// Extract the last git commit SHA from agent turn output.
///
/// Scans the output for `git commit` result lines like:
///   `[main abc1234] fix: some message`
///   `[wt/304-rework def5678] feat: add feature`
///
/// Returns the **last** (most recent) match, resolved to full SHA via
/// `git rev-parse` in the given CWD.  This is the most reliable commit
/// capture method because it reads what the agent actually committed.
fn extract_commit_sha_from_output(output: &str, cwd: &str) -> Option<String> {
    // Pattern: [branch_or_tag SHORT_SHA] message
    // Git commit output format: [main abc1234] commit message here
    let mut last_short_sha: Option<&str> = None;
    for line in output.lines().rev() {
        let trimmed = line.trim();
        // Fast pre-check before full parse
        if !trimmed.starts_with('[') {
            continue;
        }
        // Parse: [branch_name SHA] rest
        let after_bracket = match trimmed.strip_prefix('[') {
            Some(s) => s,
            None => continue,
        };
        let close_idx = match after_bracket.find(']') {
            Some(i) => i,
            None => continue,
        };
        let inside = &after_bracket[..close_idx];
        // Split into branch and SHA: "main abc1234" or "wt/304 def5678"
        let parts: Vec<&str> = inside.split_whitespace().collect();
        if parts.len() != 2 {
            continue;
        }
        let candidate_sha = parts[1];
        // Validate: 7-12 hex chars (short SHA from git commit output)
        if candidate_sha.len() >= 7
            && candidate_sha.len() <= 12
            && candidate_sha.chars().all(|c| c.is_ascii_hexdigit())
        {
            last_short_sha = Some(candidate_sha);
            break; // Scanning in reverse, first match is the last commit
        }
    }
    let short_sha = last_short_sha?;
    // Resolve short SHA to full SHA
    GitCommand::new()
        .args(["rev-parse", short_sha])
        .repo(cwd)
        .run_output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
}

/// Context needed to resolve the correct completed commit for a dispatch.
pub(super) struct DispatchCompletionHints {
    pub(super) issue_number: Option<i64>,
    pub(super) dispatch_created_at: Option<String>,
    pub(super) target_repo: Option<String>,
    pub(super) baseline_commit: Option<String>,
    /// Commit SHA extracted directly from agent output (most reliable).
    pub(super) output_commit: Option<String>,
    pub(super) output_commit_repo_dir: Option<String>,
}

#[derive(Default)]
struct ParsedCompletionHintContext {
    target_repo: Option<String>,
    baseline_commit: Option<String>,
}

fn parse_completion_hint_context(
    dispatch_id: &str,
    context_raw: Option<&str>,
    fallback_repo: Option<String>,
) -> ParsedCompletionHintContext {
    let parsed = context_raw.and_then(|raw| match serde_json::from_str::<serde_json::Value>(raw) {
        Ok(value) => Some(value),
        Err(error) => {
            tracing::warn!(
                dispatch_id = %dispatch_id,
                error = %error,
                "failed to parse postgres completion hint context JSON"
            );
            None
        }
    });

    ParsedCompletionHintContext {
        target_repo: parsed
            .as_ref()
            .and_then(|value| value.get("target_repo"))
            .and_then(|value| value.as_str())
            .map(str::to_string)
            .or(fallback_repo),
        baseline_commit: parsed
            .as_ref()
            .and_then(|value| value.get("baseline_commit"))
            .and_then(|value| value.as_str())
            .map(str::to_string),
    }
}

pub(super) fn lookup_dispatch_completion_hints(
    pg_pool: Option<&sqlx::PgPool>,
    dispatch_id: &str,
) -> DispatchCompletionHints {
    if let Some(pool) = pg_pool {
        let dispatch_id_owned = dispatch_id.to_string();
        match crate::utils::async_bridge::block_on_pg_result(
            pool,
            move |bridge_pool| async move {
                let row = sqlx::query(
                    "SELECT kc.github_issue_number,
                            td.created_at::text AS created_at,
                            td.context,
                            kc.repo_id
                     FROM task_dispatches td
                     LEFT JOIN kanban_cards kc ON kc.id = td.kanban_card_id
                     WHERE td.id = $1",
                )
                .bind(&dispatch_id_owned)
                .fetch_optional(&bridge_pool)
                .await
                .map_err(|error| {
                    format!(
                        "load postgres completion hints for dispatch {dispatch_id_owned}: {error}"
                    )
                })?;
                Ok(row.map(|row| {
                    let issue_number = decode_pg_completion_hint_field(
                        row.try_get::<Option<i64>, _>("github_issue_number"),
                        &dispatch_id_owned,
                        "github_issue_number",
                    );
                    let dispatch_created_at = decode_pg_completion_hint_field(
                        row.try_get::<Option<String>, _>("created_at"),
                        &dispatch_id_owned,
                        "created_at",
                    );
                    let context_raw = decode_pg_completion_hint_field(
                        row.try_get::<Option<String>, _>("context"),
                        &dispatch_id_owned,
                        "context",
                    );
                    let fallback_repo = decode_pg_completion_hint_field(
                        row.try_get::<Option<String>, _>("repo_id"),
                        &dispatch_id_owned,
                        "repo_id",
                    );
                    let parsed_context = parse_completion_hint_context(
                        &dispatch_id_owned,
                        context_raw.as_deref(),
                        fallback_repo,
                    );
                    (
                        issue_number,
                        dispatch_created_at,
                        parsed_context.target_repo,
                        parsed_context.baseline_commit,
                    )
                }))
            },
            |error| error,
        ) {
            Ok(Some((issue_number, dispatch_created_at, target_repo, baseline_commit))) => {
                return DispatchCompletionHints {
                    issue_number,
                    dispatch_created_at,
                    target_repo,
                    baseline_commit,
                    output_commit: None,
                    output_commit_repo_dir: None,
                };
            }
            Ok(None) => {}
            Err(error) => {
                tracing::warn!(
                    dispatch_id = %dispatch_id,
                    "failed to load postgres completion hints: {error}"
                );
            }
        }
    }

    DispatchCompletionHints {
        issue_number: None,
        dispatch_created_at: None,
        target_repo: None,
        baseline_commit: None,
        output_commit: None,
        output_commit_repo_dir: None,
    }
}

pub(super) fn completion_repo_dirs(
    adk_cwd: Option<&str>,
    hints: &DispatchCompletionHints,
) -> Vec<String> {
    let mut dirs = Vec::new();
    let mut push_dir = |candidate: Option<String>| {
        if let Some(path) = candidate.filter(|path| std::path::Path::new(path).is_dir()) {
            if !dirs.iter().any(|existing| existing == &path) {
                dirs.push(path);
            }
        }
    };

    push_dir(adk_cwd.map(str::to_string));
    push_dir(
        hints
            .target_repo
            .as_deref()
            .and_then(|value| {
                crate::services::platform::shell::resolve_repo_dir_for_target(Some(value)).ok()
            })
            .flatten(),
    );
    dirs
}

pub(super) fn extract_output_commit_from_repo_dirs(
    output: &str,
    repo_dirs: &[String],
) -> Option<(String, String)> {
    repo_dirs.iter().find_map(|repo_dir| {
        extract_commit_sha_from_output(output, repo_dir).map(|sha| (repo_dir.clone(), sha))
    })
}

fn completion_main_repo_dir(
    adk_cwd: Option<&str>,
    repo_dirs: &[String],
    hints: &DispatchCompletionHints,
) -> Option<String> {
    crate::services::platform::shell::resolve_repo_dir_for_target(hints.target_repo.as_deref())
        .ok()
        .flatten()
        .or_else(|| adk_cwd.map(str::to_string))
        .or_else(|| repo_dirs.first().cloned())
}

pub(super) fn work_dispatch_completion_context(
    adk_cwd: Option<&str>,
    hints: &DispatchCompletionHints,
) -> Option<serde_json::Value> {
    let repo_dirs = completion_repo_dirs(adk_cwd, hints);
    let main_repo_dir = completion_main_repo_dir(adk_cwd, &repo_dirs, hints);
    // Commit resolution priority:
    // 1) Agent's own commit extracted from turn output (most reliable — direct evidence)
    // 2) Time-scoped: newest commit since dispatch start, preferring issue-number match
    // 3) Mainline range scan from dispatch baseline with revert filtering
    // 4) Issue grep: recent commits matching (#issue_number)
    let output_commit = hints.output_commit.clone().and_then(|commit| {
        let repo_dir = hints
            .output_commit_repo_dir
            .clone()
            .or_else(|| repo_dirs.first().cloned())?;
        Some((repo_dir, commit))
    });
    let time_scoped_commit = hints.dispatch_created_at.as_deref().and_then(|since| {
        repo_dirs.iter().find_map(|repo_dir| {
            crate::services::platform::shell::git_best_commit_for_dispatch(
                repo_dir,
                since,
                hints.issue_number,
            )
            .map(|commit| (repo_dir.clone(), commit))
        })
    });
    let mainline_commit =
        if let (Some(issue_number), Some(repo_dir)) = (hints.issue_number, main_repo_dir) {
            let baseline_commit = hints
                .baseline_commit
                .clone()
                .or_else(|| crate::services::platform::shell::git_mainline_head_commit(&repo_dir));
            baseline_commit.and_then(|baseline_commit| {
                crate::services::platform::shell::git_mainline_commit_for_issue_since(
                    &repo_dir,
                    &baseline_commit,
                    issue_number,
                )
                .map(|commit| (repo_dir, commit))
            })
        } else {
            None
        };
    let issue_grep_commit = hints.issue_number.and_then(|issue_number| {
        repo_dirs.iter().find_map(|repo_dir| {
            crate::services::platform::shell::git_latest_commit_for_issue(repo_dir, issue_number)
                .map(|commit| (repo_dir.clone(), commit))
        })
    });

    let (cwd, completed_commit) = output_commit
        .or(time_scoped_commit)
        .or(mainline_commit)
        .or(issue_grep_commit)?;
    let mut obj = serde_json::Map::new();
    obj.insert(
        "completed_worktree_path".to_string(),
        serde_json::Value::String(cwd.clone()),
    );
    obj.insert(
        "completed_commit".to_string(),
        serde_json::Value::String(completed_commit),
    );
    if let Some(target_repo) = hints.target_repo.as_deref() {
        obj.insert(
            "target_repo".to_string(),
            serde_json::Value::String(target_repo.to_string()),
        );
    }
    if let Some(branch) = crate::services::platform::shell::git_branch_name(&cwd) {
        obj.insert(
            "completed_branch".to_string(),
            serde_json::Value::String(branch),
        );
    }
    Some(serde_json::Value::Object(obj))
}

pub(super) fn completion_result_with_context(
    source: &str,
    needs_reconcile: bool,
    adk_cwd: Option<&str>,
    hints: &DispatchCompletionHints,
) -> serde_json::Value {
    let mut result = work_dispatch_completion_context(adk_cwd, hints)
        .unwrap_or_else(|| serde_json::Value::Object(serde_json::Map::new()));
    if let Some(obj) = result.as_object_mut() {
        obj.insert(
            "completion_source".to_string(),
            serde_json::Value::String(source.to_string()),
        );
        if needs_reconcile {
            obj.insert("needs_reconcile".to_string(), serde_json::Value::Bool(true));
        }
    }
    result
}

pub(crate) fn build_work_dispatch_completion_result(
    pg_pool: Option<&sqlx::PgPool>,
    dispatch_id: &str,
    source: &str,
    needs_reconcile: bool,
    adk_cwd: Option<&str>,
    turn_output: Option<&str>,
) -> serde_json::Value {
    let mut hints = lookup_dispatch_completion_hints(pg_pool, dispatch_id);
    let repo_dirs = completion_repo_dirs(adk_cwd, &hints);
    if let Some((repo_dir, output_commit)) =
        turn_output.and_then(|output| extract_output_commit_from_repo_dirs(output, &repo_dirs))
    {
        hints.output_commit_repo_dir = Some(repo_dir);
        hints.output_commit = Some(output_commit);
    }
    completion_result_with_context(source, needs_reconcile, adk_cwd, &hints)
}

fn summarize_tracked_change_paths(paths: &[String]) -> Option<String> {
    if paths.is_empty() {
        return None;
    }
    let preview = paths.iter().take(5).cloned().collect::<Vec<_>>().join(", ");
    let remaining = paths.len().saturating_sub(5);
    Some(if remaining > 0 {
        format!("{preview} (+{remaining} more)")
    } else {
        preview
    })
}

pub(super) fn tracked_change_summary(adk_cwd: Option<&str>) -> Option<String> {
    let cwd = adk_cwd.filter(|p| std::path::Path::new(p).is_dir())?;
    let paths = crate::services::platform::shell::git_tracked_change_paths(cwd)?;
    summarize_tracked_change_paths(&paths)
}

pub(super) fn noop_completion_context(
    adk_cwd: Option<&str>,
    full_response: Option<&str>,
) -> serde_json::Value {
    let mut obj = serde_json::Map::new();
    obj.insert(
        "work_outcome".to_string(),
        serde_json::Value::String("noop".to_string()),
    );
    obj.insert(
        "completed_without_changes".to_string(),
        serde_json::Value::Bool(true),
    );
    obj.insert(
        "card_status_target".to_string(),
        serde_json::Value::String("ready".to_string()),
    );
    if let Some(response) = full_response {
        let trimmed = response.trim();
        if !trimmed.is_empty() {
            obj.insert(
                "notes".to_string(),
                serde_json::Value::String(truncate_str(trimmed, 4000).to_string()),
            );
        }
    }
    if let Some(cwd) = adk_cwd.filter(|p| std::path::Path::new(p).is_dir()) {
        obj.insert(
            "completed_worktree_path".to_string(),
            serde_json::Value::String(cwd.to_string()),
        );
        if let Some(branch) = crate::services::platform::shell::git_branch_name(cwd) {
            obj.insert(
                "completed_branch".to_string(),
                serde_json::Value::String(branch),
            );
        }
    }
    serde_json::Value::Object(obj)
}
