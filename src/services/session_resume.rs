//! `/resume` — rebind a Discord channel (or any session row) to a *previous*
//! provider session so the next turn resumes that conversation from the target
//! worktree.
//!
//! Two entry points share one core (`perform_resume_rebind`):
//!   * the HTTP route `POST /api/sessions/{session_key}/resume-previous`
//!     ([`resume_previous_session`]), used by the orchestrator for recovery, and
//!   * the Discord `/resume [session_id]` slash command (see
//!     `services::discord::commands::session`).
//!
//! The rebind is durable-first: it UPDATEs `sessions.cwd` +
//! `sessions.claude_session_id` (via [`rebind_session_provider_pg`]) so the
//! change survives a restart, then mirrors the same target into the in-memory
//! `DiscordSession` (via `health::rebind_channel_provider_session`) so it takes
//! effect on the very next turn without a restart. A DB-only rebind would be
//! shadowed by a stale in-memory `current_path` (auto-restore early-returns when
//! `current_path` is already set), which is why the in-memory mirror is not
//! optional when a runtime owns the channel.
//!
//! Teardown of the channel's current tmux/turn reuses `force_kill_turn` — the
//! same lifecycle path `/force-kill` uses — so no cleanup logic is duplicated.

use axum::{
    Json,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
};
use serde::Deserialize;
use serde_json::json;
use sqlx::PgPool;

use crate::app_state::AppState;
use crate::db::dispatched_sessions::{
    SessionRebindContext, load_force_kill_session_pg, load_session_rebind_context_pg,
    rebind_session_provider_pg,
};
use crate::services::discord::health::{
    HealthRegistry, channel_has_active_turn, rebind_channel_provider_session,
};
use crate::services::discord::session_identity::tmux_name_from_session_key;
use crate::services::provider::ProviderKind;
use crate::services::turn_lifecycle::{TurnLifecycleTarget, force_kill_turn};
use poise::serenity_prelude::ChannelId;

/// Request body for `POST /api/sessions/{session_key}/resume-previous`.
///
/// Both fields optional: supply `session_id` (+ optional `cwd`) to force a
/// specific rebind; omit both to auto-select the channel's most recent prior
/// provider session.
#[derive(Debug, Default, Clone, Deserialize)]
pub struct ResumePreviousOptions {
    /// Target provider session id to resume. When omitted, the previous session
    /// is auto-selected from the workspace's transcripts.
    #[serde(default)]
    pub session_id: Option<String>,
    /// Target working directory (worktree) the resumed session lives in. When
    /// omitted with an explicit `session_id`, the row's current `cwd` is kept.
    #[serde(default)]
    pub cwd: Option<String>,
}

/// Successful rebind result — returned to the HTTP caller and rendered into the
/// slash-command reply.
#[derive(Debug, Clone)]
pub(crate) struct ResumeRebindOutcome {
    pub(crate) target_session_id: String,
    pub(crate) target_cwd: String,
    pub(crate) previous_session_id: Option<String>,
    pub(crate) previous_cwd: Option<String>,
    pub(crate) tmux_killed: bool,
    pub(crate) lifecycle_path: &'static str,
    pub(crate) in_memory_rebound: bool,
    /// `true` when the target was auto-selected (no explicit `session_id`).
    pub(crate) auto_selected: bool,
}

/// Failure modes, each mapping to a distinct HTTP status.
#[derive(Debug)]
pub(crate) enum ResumeRebindError {
    /// No `sessions` row exists for the given `session_key`.
    SessionNotFound,
    /// The channel has an in-flight dispatch or active turn; rebinding now would
    /// leave the running process writing to the old transcript.
    ActiveTurn,
    /// Auto mode found no prior provider session to resume.
    NoPreviousSession,
    /// Explicit `session_id` given but no `cwd` is known (row has no `cwd` and
    /// none was supplied).
    MissingCwd,
    /// Target `cwd` does not exist on disk.
    TargetCwdMissing(String),
    /// Auto-selection is only wired for Claude transcripts.
    AutoUnsupportedProvider(String),
    Database(String),
}

impl ResumeRebindError {
    pub(crate) fn into_response(self) -> (StatusCode, Json<serde_json::Value>) {
        match self {
            ResumeRebindError::SessionNotFound => (
                StatusCode::NOT_FOUND,
                Json(json!({"error": "session not found"})),
            ),
            ResumeRebindError::ActiveTurn => (
                StatusCode::CONFLICT,
                Json(json!({
                    "error": "channel has an active turn or dispatch; stop it before resuming a previous session"
                })),
            ),
            ResumeRebindError::NoPreviousSession => (
                StatusCode::NOT_FOUND,
                Json(json!({
                    "error": "no previous provider session found to resume; pass an explicit session_id"
                })),
            ),
            ResumeRebindError::MissingCwd => (
                StatusCode::UNPROCESSABLE_ENTITY,
                Json(json!({
                    "error": "target cwd is unknown; supply cwd alongside session_id"
                })),
            ),
            ResumeRebindError::TargetCwdMissing(path) => (
                StatusCode::UNPROCESSABLE_ENTITY,
                Json(json!({"error": format!("target cwd does not exist: {path}")})),
            ),
            ResumeRebindError::AutoUnsupportedProvider(provider) => (
                StatusCode::UNPROCESSABLE_ENTITY,
                Json(json!({
                    "error": format!(
                        "auto previous-session selection is only supported for Claude; provider={provider}. Pass an explicit session_id."
                    )
                })),
            ),
            ResumeRebindError::Database(error) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": error})),
            ),
        }
    }
}

impl ResumeRebindOutcome {
    pub(crate) fn into_response(self, session_key: &str) -> (StatusCode, Json<serde_json::Value>) {
        (
            StatusCode::OK,
            Json(json!({
                "ok": true,
                "session_key": session_key,
                "target_session_id": self.target_session_id,
                "target_cwd": self.target_cwd,
                "previous_session_id": self.previous_session_id,
                "previous_cwd": self.previous_cwd,
                "tmux_killed": self.tmux_killed,
                "lifecycle_path": self.lifecycle_path,
                "in_memory_rebound": self.in_memory_rebound,
                "auto_selected": self.auto_selected,
            })),
        )
    }
}

/// POST /api/sessions/{session_key}/resume-previous
///
/// Rebind the session identified by `session_key` to a previous provider
/// session. Mirrors the forwarding + teardown contract of `/force-kill`.
pub async fn resume_previous_session(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(session_key): Path<String>,
    Json(opts): Json<ResumePreviousOptions>,
) -> (StatusCode, Json<serde_json::Value>) {
    let Some(tmux_name) = tmux_name_from_session_key(&session_key) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": "invalid session_key format — expected legacy host:tmux or namespaced provider/token/host:tmux"
            })),
        );
    };

    let provider_info =
        crate::services::provider::parse_provider_and_channel_from_tmux_name(&tmux_name);
    let provider_name = provider_info
        .as_ref()
        .map(|(provider, _)| provider.as_str());

    let Some(pool) = state.pg_pool_ref() else {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": "postgres pool unavailable"})),
        );
    };

    // Resolve the runtime channel + owning node exactly like force-kill so
    // cross-node requests forward to the owner instead of rebinding a row this
    // node does not run.
    let (_active_dispatch_id, _agent_id, runtime_channel_id, session_provider, owner_instance_id) =
        match load_force_kill_session_pg(pool, &session_key, provider_name).await {
            Ok(Some(tuple)) => tuple,
            Ok(None) => {
                return (
                    StatusCode::NOT_FOUND,
                    Json(json!({"error": "session not found"})),
                );
            }
            Err(error) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": error})),
                );
            }
        };

    if !crate::services::session_forwarding::is_forwarded_request(&headers) {
        let forward_context =
            crate::services::session_forwarding::ForwardCallerContext::from(&state);
        match crate::services::session_forwarding::resolve_forward_target(
            &forward_context,
            owner_instance_id.as_deref(),
            pool,
        )
        .await
        {
            crate::services::session_forwarding::ForwardResolution::Local => {}
            crate::services::session_forwarding::ForwardResolution::Forward(target) => {
                return crate::services::session_forwarding::forward_resume_previous(
                    &forward_context,
                    &target,
                    &session_key,
                    opts.session_id.as_deref(),
                    opts.cwd.as_deref(),
                )
                .await;
            }
            crate::services::session_forwarding::ForwardResolution::Unavailable {
                status,
                body,
            } => {
                return (status, Json(body));
            }
        }
    }

    let provider = provider_info
        .as_ref()
        .map(|(provider, _)| provider.clone())
        .or_else(|| session_provider.as_deref().and_then(ProviderKind::from_str));
    let channel_id = runtime_channel_id
        .as_deref()
        .and_then(|id| id.parse::<u64>().ok())
        .map(ChannelId::new);

    match perform_resume_rebind(
        pool,
        state.health_registry.as_deref(),
        &session_key,
        provider,
        channel_id,
        &tmux_name,
        &opts,
    )
    .await
    {
        Ok(outcome) => outcome.into_response(&session_key),
        Err(error) => error.into_response(),
    }
}

/// Core rebind logic shared by the HTTP route and the slash command. Callers
/// resolve `provider` / `channel_id` (and do any forwarding) first, then hand
/// off here. `provider` is `None` only for unparseable session keys — in that
/// case tmux teardown and the in-memory mirror are skipped but the DB rebind
/// still runs.
pub(crate) async fn perform_resume_rebind(
    pool: &PgPool,
    registry: Option<&HealthRegistry>,
    session_key: &str,
    provider: Option<ProviderKind>,
    channel_id: Option<ChannelId>,
    tmux_name: &str,
    opts: &ResumePreviousOptions,
) -> Result<ResumeRebindOutcome, ResumeRebindError> {
    let Some(SessionRebindContext {
        active_dispatch_id,
        cwd: current_cwd,
        claude_session_id: current_session_id,
    }) = load_session_rebind_context_pg(pool, session_key)
        .await
        .map_err(ResumeRebindError::Database)?
    else {
        return Err(ResumeRebindError::SessionNotFound);
    };

    // Guard 1: an attached dispatch owns this session's lifecycle.
    if active_dispatch_id.is_some() {
        return Err(ResumeRebindError::ActiveTurn);
    }
    // Guard 2: a live interactive turn (no dispatch) is still writing output.
    if let (Some(registry), Some(provider), Some(channel_id)) =
        (registry, provider.as_ref(), channel_id)
        && channel_has_active_turn(registry, provider.as_str(), channel_id).await
    {
        return Err(ResumeRebindError::ActiveTurn);
    }

    // Resolve the rebind target.
    let (target_session_id, target_cwd, auto_selected) = match opts.session_id.as_deref() {
        Some(session_id) if !session_id.trim().is_empty() => {
            let cwd = opts
                .cwd
                .as_deref()
                .map(str::to_string)
                .or_else(|| current_cwd.clone())
                .ok_or(ResumeRebindError::MissingCwd)?;
            (session_id.trim().to_string(), cwd, false)
        }
        _ => {
            let provider = provider
                .clone()
                .ok_or_else(|| ResumeRebindError::AutoUnsupportedProvider("unknown".to_string()))?;
            if !matches!(provider, ProviderKind::Claude) {
                return Err(ResumeRebindError::AutoUnsupportedProvider(
                    provider.as_str().to_string(),
                ));
            }
            let candidate = discover_previous_claude_session(
                current_cwd.as_deref(),
                current_session_id.as_deref(),
            )
            .ok_or(ResumeRebindError::NoPreviousSession)?;
            (candidate.session_id, candidate.cwd, true)
        }
    };

    // Reject a target worktree that no longer exists — resuming into a missing
    // cwd would silently start a fresh session in the wrong place.
    if !std::path::Path::new(&target_cwd).is_dir() {
        return Err(ResumeRebindError::TargetCwdMissing(target_cwd));
    }

    // Teardown the channel's current tmux/turn via the shared lifecycle path.
    let mut tmux_killed = false;
    let mut lifecycle_path = "skipped-no-runtime";
    if let (Some(registry), Some(provider), Some(channel_id)) =
        (registry, provider.as_ref(), channel_id)
    {
        let lifecycle = force_kill_turn(
            Some(registry),
            &TurnLifecycleTarget {
                provider: Some(provider.clone()),
                channel_id: Some(channel_id),
                tmux_name: tmux_name.to_string(),
            },
            "resume rebind (/resume)",
            "force_kill",
        )
        .await;
        tmux_killed = lifecycle.tmux_killed;
        lifecycle_path = lifecycle.lifecycle_path;
    }

    // Durable rebind: repoint cwd + provider session id in the sessions row.
    let rows = rebind_session_provider_pg(pool, session_key, &target_cwd, &target_session_id)
        .await
        .map_err(ResumeRebindError::Database)?;
    if rows == 0 {
        // Row disappeared between the context load and the update.
        return Err(ResumeRebindError::SessionNotFound);
    }

    // In-memory mirror so the next turn resumes without a restart.
    let mut in_memory_rebound = false;
    if let (Some(registry), Some(provider), Some(channel_id)) =
        (registry, provider.as_ref(), channel_id)
    {
        in_memory_rebound = rebind_channel_provider_session(
            registry,
            provider.as_str(),
            channel_id,
            &target_cwd,
            &target_session_id,
        )
        .await;
    }

    Ok(ResumeRebindOutcome {
        target_session_id,
        target_cwd,
        previous_session_id: current_session_id,
        previous_cwd: current_cwd,
        tmux_killed,
        lifecycle_path,
        in_memory_rebound,
        auto_selected,
    })
}

/// An auto-selected previous-session candidate: the worktree and provider
/// session id to resume.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PreviousSessionCandidate {
    pub(crate) cwd: String,
    pub(crate) session_id: String,
}

/// Auto-select the most recent prior Claude session for a channel whose current
/// binding is `(current_cwd, current_session_id)`.
///
/// Heuristic: scan the current worktree and its sibling worktrees (the managed
/// worktrees living under the same parent directory) for Claude transcripts
/// (`~/.claude/projects/<slug>/<uuid>.jsonl`). Pick the transcript with the
/// newest mtime that is NOT the current binding. The transcript filename stem is
/// the provider session id; its owning cwd is the worktree it was scanned under.
///
/// Returns `None` when no distinct prior transcript exists.
pub(crate) fn discover_previous_claude_session(
    current_cwd: Option<&str>,
    current_session_id: Option<&str>,
) -> Option<PreviousSessionCandidate> {
    discover_previous_claude_session_in(current_cwd, current_session_id, None)
}

/// [`discover_previous_claude_session`] with an injectable Claude home (for
/// tests). Production passes `None`, which resolves the real `~/.claude`.
pub(crate) fn discover_previous_claude_session_in(
    current_cwd: Option<&str>,
    current_session_id: Option<&str>,
    claude_home: Option<&std::path::Path>,
) -> Option<PreviousSessionCandidate> {
    let current_cwd = current_cwd?;
    let current_path = std::path::Path::new(current_cwd);
    let parent = current_path.parent()?;

    // Candidate worktree directories: the current cwd plus its siblings.
    let mut worktrees: Vec<std::path::PathBuf> = Vec::new();
    worktrees.push(current_path.to_path_buf());
    if let Ok(entries) = std::fs::read_dir(parent) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() && path != current_path {
                worktrees.push(path);
            }
        }
    }

    let empty_exclude = std::collections::HashSet::new();
    let mut best: Option<(std::time::SystemTime, PreviousSessionCandidate)> = None;

    for worktree in worktrees {
        let transcripts =
            crate::services::claude_tui::transcript_tail::claude_transcripts_for_cwd_since(
                &worktree,
                std::time::UNIX_EPOCH,
                claude_home,
                &empty_exclude,
            );
        for transcript in transcripts {
            let Some(session_id) = transcript
                .file_stem()
                .and_then(|stem| stem.to_str())
                .map(str::to_string)
            else {
                continue;
            };
            // Skip the channel's currently-bound session.
            if Some(session_id.as_str()) == current_session_id {
                continue;
            }
            let Ok(modified) = std::fs::metadata(&transcript).and_then(|meta| meta.modified())
            else {
                continue;
            };
            let candidate = PreviousSessionCandidate {
                cwd: worktree.to_string_lossy().to_string(),
                session_id,
            };
            match &best {
                Some((best_mtime, _)) if *best_mtime >= modified => {}
                _ => best = Some((modified, candidate)),
            }
        }
    }

    best.map(|(_, candidate)| candidate)
}

#[cfg(test)]
mod tests {
    use super::*;
    use filetime::{FileTime, set_file_mtime};

    #[test]
    fn discover_returns_none_without_current_cwd() {
        assert_eq!(discover_previous_claude_session(None, None), None);
    }

    fn unique_tmp(tag: &str) -> std::path::PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("resume-{tag}-{}-{nanos}", std::process::id()))
    }

    fn write_transcript(
        claude_home: &std::path::Path,
        cwd: &std::path::Path,
        sid: &str,
        mtime_secs: i64,
    ) -> std::path::PathBuf {
        let dir = crate::services::claude_tui::transcript_tail::claude_project_dir_for_cwd(
            cwd,
            Some(claude_home),
        )
        .unwrap();
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("{sid}.jsonl"));
        std::fs::write(&path, b"{}\n").unwrap();
        set_file_mtime(&path, FileTime::from_unix_time(mtime_secs, 0)).unwrap();
        path
    }

    #[test]
    fn discover_skips_current_and_picks_newest_prior_across_siblings() {
        let tmp = unique_tmp("newest");
        let claude_home = tmp.join(".claude");
        let parent = tmp.join("worktrees");
        let cwd_a = parent.join("wt-a");
        let cwd_b = parent.join("wt-b");
        std::fs::create_dir_all(&cwd_a).unwrap();
        std::fs::create_dir_all(&cwd_b).unwrap();

        let current = "11111111-1111-1111-1111-111111111111";
        let prior_a = "22222222-2222-2222-2222-222222222222";
        let prior_b = "33333333-3333-3333-3333-333333333333";

        // Current binding is the newest file overall, but must be skipped.
        write_transcript(&claude_home, &cwd_a, current, 3_000);
        write_transcript(&claude_home, &cwd_a, prior_a, 1_000);
        // Sibling wt-b holds the newest *prior* transcript.
        write_transcript(&claude_home, &cwd_b, prior_b, 2_000);

        let result = discover_previous_claude_session_in(
            Some(cwd_a.to_str().unwrap()),
            Some(current),
            Some(&claude_home),
        )
        .expect("a prior session should be selected");

        assert_eq!(result.session_id, prior_b, "newest prior transcript wins");
        assert_eq!(result.cwd, cwd_b.to_string_lossy());

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn discover_returns_none_when_only_current_binding_exists() {
        let tmp = unique_tmp("only-current");
        let claude_home = tmp.join(".claude");
        let parent = tmp.join("worktrees");
        let cwd = parent.join("wt-a");
        std::fs::create_dir_all(&cwd).unwrap();

        let current = "44444444-4444-4444-4444-444444444444";
        write_transcript(&claude_home, &cwd, current, 5_000);

        assert_eq!(
            discover_previous_claude_session_in(
                Some(cwd.to_str().unwrap()),
                Some(current),
                Some(&claude_home),
            ),
            None,
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }
}
