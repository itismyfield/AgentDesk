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
//! `sessions.claude_session_id` (via [`rebind_session_provider_by_id_pg`]) so the
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
    self as dispatched_sessions_db, SessionRebindContext, load_force_kill_session_pg,
    load_session_rebind_context_pg, rebind_session_provider_by_id_pg,
};
use crate::services::discord::health::{
    HealthRegistry, channel_has_active_turn, clear_resume_runtime_owner_after_death,
    rebind_channel_provider_session, resume_runtime_for_channel,
    retain_resume_runtime_owner_before_teardown,
};
use crate::services::discord::session_identity::tmux_name_from_session_key;
use crate::services::provider::ProviderKind;
use crate::services::turn_lifecycle::{TurnLifecycleTarget, force_kill_turn};
use poise::serenity_prelude::ChannelId;

const RESUME_CRITICAL_SECTION_WARN_AFTER: std::time::Duration = std::time::Duration::from_secs(5);

struct ResumeCriticalSectionWatchdog(tokio::task::JoinHandle<()>);

impl Drop for ResumeCriticalSectionWatchdog {
    fn drop(&mut self) {
        self.0.abort();
    }
}

fn start_resume_critical_section_watchdog(
    channel_id: ChannelId,
    provider: &ProviderKind,
    warn_after: std::time::Duration,
) -> ResumeCriticalSectionWatchdog {
    let provider = provider.as_str().to_string();
    ResumeCriticalSectionWatchdog(tokio::spawn(async move {
        tokio::time::sleep(warn_after).await;
        crate::services::observability::metrics::record_resume_critical_section_overrun(
            channel_id.get(),
            &provider,
        );
        tracing::warn!(
            channel_id = channel_id.get(),
            provider,
            threshold_secs = warn_after.as_secs(),
            "resume channel transition critical section exceeded its observation threshold"
        );
    }))
}

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
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ResumeRuntimeBindingClearOutcome {
    Cleared { tmux_session: String },
    AlreadyAbsent { tmux_session: String },
    RetainedLive { tmux_session: String },
}

impl ResumeRuntimeBindingClearOutcome {
    fn status(&self) -> &'static str {
        match self {
            Self::Cleared { .. } => "cleared",
            Self::AlreadyAbsent { .. } => "already_absent",
            Self::RetainedLive { .. } => "retained_live",
        }
    }

    fn tmux_session(&self) -> &str {
        match self {
            Self::Cleared { tmux_session }
            | Self::AlreadyAbsent { tmux_session }
            | Self::RetainedLive { tmux_session } => tmux_session,
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct ResumeRebindOutcome {
    pub(crate) target_session_id: String,
    pub(crate) target_cwd: String,
    pub(crate) previous_session_id: Option<String>,
    pub(crate) previous_cwd: Option<String>,
    pub(crate) tmux_killed: bool,
    pub(crate) lifecycle_path: &'static str,
    pub(crate) runtime_binding_clear: ResumeRuntimeBindingClearOutcome,
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
    /// Another intake/resume transition held the channel lock beyond the bounded
    /// acquisition window. The caller may retry without partial mutation.
    TransitionBusy,
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
    Filesystem(String),
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
            ResumeRebindError::TransitionBusy => (
                StatusCode::CONFLICT,
                Json(json!({
                    "error": format!(
                        "채널 세션 전환이 {}초 동안 계속 사용 중입니다. 잠시 후 /resume을 다시 시도해 주세요.",
                        crate::services::discord::SESSION_TRANSITION_LOCK_WAIT_TIMEOUT.as_secs()
                    )
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
            ResumeRebindError::Database(error) | ResumeRebindError::Filesystem(error) => (
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
                "runtime_binding_clear": {
                    "status": self.runtime_binding_clear.status(),
                    "tmux_session": self.runtime_binding_clear.tmux_session(),
                },
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
    let Some(pool) = state.pg_pool_ref() else {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": "postgres pool unavailable"})),
        );
    };
    let forward_context = crate::services::session_forwarding::ForwardCallerContext::from(&state);
    dispatch_resume_previous(
        pool,
        state.health_registry.as_deref(),
        &forward_context,
        Some(&headers),
        &session_key,
        &opts,
    )
    .await
}

/// Shared entry point for the HTTP route and the `/resume` slash command: load
/// the owning node, forward cross-node when this node is not the owner (so a
/// gateway node never mutates a session it does not run — S1), and otherwise
/// run the local rebind. HTTP callers pass the incoming headers so forwarded
/// requests are fenced against both the expected and current database owner;
/// in-process callers pass `None` and can originate (but never receive) forwarding.
pub(crate) async fn dispatch_resume_previous(
    pool: &PgPool,
    registry: Option<&HealthRegistry>,
    forward_context: &crate::services::session_forwarding::ForwardCallerContext,
    request_headers: Option<&HeaderMap>,
    session_key: &str,
    opts: &ResumePreviousOptions,
) -> (StatusCode, Json<serde_json::Value>) {
    let Some(tmux_name) = tmux_name_from_session_key(session_key) else {
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

    // Resolve the runtime channel + owning node exactly like force-kill so
    // cross-node requests forward to the owner instead of rebinding a row this
    // node does not run.
    let (_active_dispatch_id, _agent_id, runtime_channel_id, session_provider, owner_instance_id) =
        match load_force_kill_session_pg(pool, session_key, provider_name).await {
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

    let is_forwarded =
        request_headers.is_some_and(crate::services::session_forwarding::is_forwarded_request);
    if let Some(headers) = request_headers
        && let Err(response) = crate::services::session_forwarding::enforce_receiver_fence(
            headers,
            owner_instance_id.as_deref(),
            forward_context.cluster_instance_id.as_deref(),
        )
    {
        return response;
    }
    if !is_forwarded {
        match crate::services::session_forwarding::resolve_forward_target(
            forward_context,
            owner_instance_id.as_deref(),
            pool,
        )
        .await
        {
            crate::services::session_forwarding::ForwardResolution::Local => {}
            crate::services::session_forwarding::ForwardResolution::Forward(target) => {
                return crate::services::session_forwarding::forward_resume_previous(
                    forward_context,
                    &target,
                    session_key,
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
        registry,
        session_key,
        provider,
        channel_id,
        &tmux_name,
        opts,
    )
    .await
    {
        Ok(outcome) => outcome.into_response(session_key),
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
    let resume_runtime = if let (Some(registry), Some(provider), Some(channel_id)) =
        (registry, provider.as_ref(), channel_id)
    {
        resume_runtime_for_channel(registry, provider, channel_id).await
    } else {
        None
    };
    let _transition_guard = match resume_runtime.as_ref().zip(channel_id) {
        Some((shared, channel_id)) => Some(
            shared
                .acquire_session_transition(channel_id)
                .await
                .map_err(|_| ResumeRebindError::TransitionBusy)?,
        ),
        None => None,
    };
    // Keep the guard through the conservatively-biased pane probe below. Releasing
    // it earlier would let intake claim the channel from the old runtime snapshot
    // while `/resume` is still deciding whether that runtime must be kept. The
    // critical section deliberately has no cancelling timeout: after the durable
    // DB rebind commits, cancelling before teardown, the pane decision, and the
    // in-memory mirror complete would expose a mixed binding. Observe overruns
    // instead, while preserving the transition atomically through completion.
    let _critical_section_watchdog = match channel_id.zip(provider.as_ref()) {
        Some((channel_id, provider)) if _transition_guard.is_some() => {
            Some(start_resume_critical_section_watchdog(
                channel_id,
                provider,
                RESUME_CRITICAL_SECTION_WARN_AFTER,
            ))
        }
        _ => None,
    };

    let Some(SessionRebindContext {
        resolved_session_key: _,
        session_id,
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
            // B1: never adopt a session id that some channel is *currently*
            // bound to (would repoint two channels at one session and thrash
            // their bindings — the #2843 live-binding hazard). Candidates are
            // additionally scoped to this channel's own worktree lineage inside
            // `discover_previous_claude_session_in`, so a sibling agent's
            // worktree (e.g. `claude-adk-dash-cc-*` next to `claude-adk-cc-*`)
            // can never be selected.
            let live_bound = dispatched_sessions_db::load_live_bound_session_ids_pg(pool)
                .await
                .map_err(ResumeRebindError::Database)?;
            let candidate = discover_previous_claude_session_off_runtime(
                current_cwd.clone(),
                current_session_id.clone(),
                live_bound,
                None,
            )
            .await?
            .ok_or(ResumeRebindError::NoPreviousSession)?;
            (candidate.session_id, candidate.cwd, true)
        }
    };

    // Reject a target worktree that no longer exists — resuming into a missing
    // cwd would silently start a fresh session in the wrong place.
    if !std::path::Path::new(&target_cwd).is_dir() {
        return Err(ResumeRebindError::TargetCwdMissing(target_cwd));
    }

    // P1-B — durable-first ordering: commit the DB rebind BEFORE tearing down
    // the current tmux. If the durable UPDATE fails we return here without
    // having destroyed the live session (teardown is skipped), so the channel
    // keeps working on its old binding and the operator can retry. Only once
    // the row is repointed do we kill the old tmux and mirror in memory.
    let rows = rebind_session_provider_by_id_pg(pool, session_id, &target_cwd, &target_session_id)
        .await
        .map_err(ResumeRebindError::Database)?;
    if rows == 0 {
        // Row disappeared between the context load and the update.
        return Err(ResumeRebindError::SessionNotFound);
    }

    // Preserve the current authoritative owner before teardown can remove the
    // watcher slot. If the pane survives, this owner-only entry bridges output
    // until the resumed turn replaces it; confirmed pane death clears it below.
    let owner_retained_before_teardown =
        if let (Some(shared), Some(channel_id)) = (resume_runtime.as_deref(), channel_id) {
            retain_resume_runtime_owner_before_teardown(shared, channel_id, tmux_name)
        } else {
            false
        };

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

    // If teardown actually retired the pane, forget its stale transcript/session
    // mirror. Otherwise keep the live binding and its scan offset so output that
    // arrives during the rebind remains eligible for delayed delivery.
    let pane_liveness = if tmux_killed {
        crate::services::platform::tmux::PaneLiveness::DeadOrAbsent
    } else {
        // #4794 adopts #4489's existing three-state/two-second pane probe through
        // its async adapter; this call site contributes only the conservative
        // state decision, while JoinError is mapped to ProbeError by the adapter.
        crate::services::tmux_diagnostics::probe_tmux_session_pane_liveness(tmux_name).await
    };
    let runtime_binding_clear = match pane_liveness {
        crate::services::platform::tmux::PaneLiveness::DeadOrAbsent => {
            if owner_retained_before_teardown && let Some(shared) = resume_runtime.as_deref() {
                clear_resume_runtime_owner_after_death(shared, tmux_name);
            }
            clear_resume_runtime_binding(tmux_name)
        }
        crate::services::platform::tmux::PaneLiveness::Live
        | crate::services::platform::tmux::PaneLiveness::ProbeError => {
            ResumeRuntimeBindingClearOutcome::RetainedLive {
                tmux_session: tmux_name.trim().to_string(),
            }
        }
    };

    // In-memory mirror so the next turn resumes without a restart.
    let in_memory_rebound = if let (Some(shared), Some(provider), Some(channel_id)) =
        (resume_runtime.as_deref(), provider.as_ref(), channel_id)
    {
        rebind_channel_provider_session(
            shared,
            provider,
            channel_id,
            &target_cwd,
            &target_session_id,
            tmux_name,
            matches!(
                runtime_binding_clear,
                ResumeRuntimeBindingClearOutcome::RetainedLive { .. }
            ),
        )
        .await;
        true
    } else {
        false
    };

    Ok(ResumeRebindOutcome {
        target_session_id,
        target_cwd,
        previous_session_id: current_session_id,
        previous_cwd: current_cwd,
        tmux_killed,
        lifecycle_path,
        runtime_binding_clear,
        in_memory_rebound,
        auto_selected,
    })
}

fn clear_resume_runtime_binding(tmux_session: &str) -> ResumeRuntimeBindingClearOutcome {
    let tmux_session = tmux_session.trim().to_string();
    if crate::services::tui_prompt_dedupe::clear_tmux_runtime_binding(&tmux_session) {
        ResumeRuntimeBindingClearOutcome::Cleared { tmux_session }
    } else {
        ResumeRuntimeBindingClearOutcome::AlreadyAbsent { tmux_session }
    }
}

/// An auto-selected previous-session candidate: the worktree and provider
/// session id to resume.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PreviousSessionCandidate {
    pub(crate) cwd: String,
    pub(crate) session_id: String,
}

/// Derive the *lineage stem* of a managed-worktree directory name: the name
/// with a trailing `-YYYYMMDD-HHMMSS` timestamp removed (the suffix
/// `create_git_worktree` appends as `{provider}-{safe_name}-{ts}`). Two
/// worktrees belong to the same channel lineage iff their stems are equal, so
/// `claude-adk-cc-20260723-054531` and `claude-adk-cc-20260723-050333` share the
/// stem `claude-adk-cc`, while the sibling agent `claude-adk-dash-cc` (stem
/// `claude-adk-dash-cc`) does not. Names without the timestamp suffix are their
/// own stem (only same-name worktrees match — effectively same-worktree scope).
fn worktree_lineage_stem(dir_name: &str) -> &str {
    let bytes = dir_name.as_bytes();
    // Match a trailing `-\d{8}-\d{6}` (date-time) and, if present, cut it off.
    // 15 trailing chars: `-DDDDDDDD-DDDDDD` → prefix length = len - 16.
    if bytes.len() > 16 {
        let tail = &dir_name[dir_name.len() - 16..];
        let tb = tail.as_bytes();
        let shaped = tb[0] == b'-'
            && tb[9] == b'-'
            && tb[1..9].iter().all(u8::is_ascii_digit)
            && tb[10..16].iter().all(u8::is_ascii_digit);
        if shaped {
            return &dir_name[..dir_name.len() - 16];
        }
    }
    dir_name
}

/// Auto-select the most recent prior Claude session for a channel.
///
/// Scope (B1 — channel attribution): candidate worktrees are the current cwd
/// plus only those sibling directories that share the current worktree's
/// *lineage stem* ([`worktree_lineage_stem`]). This prevents auto-select from
/// crossing into an unrelated agent's / channel's worktree that merely lives in
/// the same `worktrees/` parent. Within those worktrees, each Claude transcript
/// (`~/.claude/projects/<slug>/<uuid>.jsonl`, stem = provider session id) is a
/// candidate; the newest mtime wins.
///
/// Exclusions: the channel's own current binding (`current_session_id`) and any
/// session id in `live_bound` (currently bound to *some* channel's session row)
/// are skipped, so auto-select never adopts a session another live channel is
/// using — restoring the #2843 live-binding protection an empty exclude bypassed.
///
/// `claude_home` is injectable for tests; production passes `None`.
///
/// Returns `None` when no distinct, unbound prior transcript exists in-lineage.
async fn discover_previous_claude_session_off_runtime(
    current_cwd: Option<String>,
    current_session_id: Option<String>,
    live_bound: std::collections::HashSet<String>,
    claude_home: Option<std::path::PathBuf>,
) -> Result<Option<PreviousSessionCandidate>, ResumeRebindError> {
    tokio::task::spawn_blocking(move || {
        discover_previous_claude_session_scoped(
            current_cwd.as_deref(),
            current_session_id.as_deref(),
            &live_bound,
            claude_home.as_deref(),
        )
    })
    .await
    .map_err(|error| {
        ResumeRebindError::Filesystem(format!("previous-session discovery task failed: {error}"))
    })
}

pub(crate) fn discover_previous_claude_session_scoped(
    current_cwd: Option<&str>,
    current_session_id: Option<&str>,
    live_bound: &std::collections::HashSet<String>,
    claude_home: Option<&std::path::Path>,
) -> Option<PreviousSessionCandidate> {
    let current_cwd = current_cwd?;
    let current_path = std::path::Path::new(current_cwd);
    let parent = current_path.parent()?;
    let lineage = current_path
        .file_name()
        .and_then(|name| name.to_str())
        .map(worktree_lineage_stem)?;

    // Candidate worktrees: current cwd + siblings sharing the lineage stem.
    let mut worktrees: Vec<std::path::PathBuf> = vec![current_path.to_path_buf()];
    if let Ok(entries) = std::fs::read_dir(parent) {
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() || path == current_path {
                continue;
            }
            let same_lineage = path
                .file_name()
                .and_then(|name| name.to_str())
                .map(worktree_lineage_stem)
                == Some(lineage);
            if same_lineage {
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
            // Skip the channel's own current binding and any live-bound session.
            if Some(session_id.as_str()) == current_session_id || live_bound.contains(&session_id) {
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
    use std::sync::Arc;

    use super::*;
    use filetime::{FileTime, set_file_mtime};

    #[test]
    fn transition_busy_response_exposes_retryable_korean_contract() {
        let (status, Json(body)) = ResumeRebindError::TransitionBusy.into_response();
        assert_eq!(status, StatusCode::CONFLICT);
        let error = body["error"]
            .as_str()
            .expect("busy response has error text");
        assert!(error.contains("3초"));
        assert!(error.contains("/resume을 다시 시도"));
    }

    #[tokio::test(start_paused = true)]
    async fn critical_section_watchdog_records_once_without_cancelling_work() {
        let channel_id = ChannelId::new(9_479_404);
        let provider = ProviderKind::Claude;
        let count = || {
            crate::services::observability::metrics::snapshot()
                .into_iter()
                .find(|row| row.channel_id == channel_id.get() && row.provider == "claude")
                .map_or(0, |row| row.resume_critical_section_overrun)
        };
        let before = count();
        let watchdog = start_resume_critical_section_watchdog(
            channel_id,
            &provider,
            RESUME_CRITICAL_SECTION_WARN_AFTER,
        );
        tokio::task::yield_now().await;

        tokio::time::advance(
            RESUME_CRITICAL_SECTION_WARN_AFTER - std::time::Duration::from_secs(1),
        )
        .await;
        tokio::task::yield_now().await;
        assert_eq!(count(), before);

        tokio::time::advance(std::time::Duration::from_secs(1)).await;
        tokio::task::yield_now().await;
        assert_eq!(count(), before + 1);
        drop(watchdog);
    }

    #[tokio::test(start_paused = true)]
    async fn completed_critical_section_cancels_its_watchdog() {
        let channel_id = ChannelId::new(9_479_405);
        let provider = ProviderKind::Claude;
        let before = crate::services::observability::metrics::snapshot()
            .into_iter()
            .find(|row| row.channel_id == channel_id.get() && row.provider == "claude")
            .map_or(0, |row| row.resume_critical_section_overrun);
        let watchdog = start_resume_critical_section_watchdog(
            channel_id,
            &provider,
            RESUME_CRITICAL_SECTION_WARN_AFTER,
        );
        tokio::task::yield_now().await;
        drop(watchdog);
        tokio::time::advance(RESUME_CRITICAL_SECTION_WARN_AFTER).await;
        tokio::task::yield_now().await;

        let after = crate::services::observability::metrics::snapshot()
            .into_iter()
            .find(|row| row.channel_id == channel_id.get() && row.provider == "claude")
            .map_or(0, |row| row.resume_critical_section_overrun);
        assert_eq!(after, before);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn off_runtime_discovery_preserves_selection_and_runtime_progress() {
        let tmp = unique_tmp("off-runtime");
        let claude_home = tmp.join(".claude");
        let parent = tmp.join("worktrees");
        let cwd = parent.join("claude-adk-cc-20260101-000001");
        std::fs::create_dir_all(&cwd).unwrap();
        let prior = "22222222-2222-2222-2222-222222222222";
        write_transcript(&claude_home, &cwd, prior, 2_000);

        let expected = discover_previous_claude_session_scoped(
            Some(cwd.to_str().unwrap()),
            None,
            &no_live_bound(),
            Some(&claude_home),
        );
        let selected = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            discover_previous_claude_session_off_runtime(
                Some(cwd.to_string_lossy().to_string()),
                None,
                no_live_bound(),
                Some(claude_home),
            ),
        )
        .await
        .expect("blocking discovery must complete")
        .expect("blocking task must not fail")
        .expect("prior session must be selected");

        assert_eq!(Some(selected.clone()), expected);
        assert_eq!(selected.session_id, prior);
        assert_eq!(selected.cwd, cwd.to_string_lossy());
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn discover_returns_none_without_current_cwd() {
        assert_eq!(
            discover_previous_claude_session_scoped(
                None,
                None,
                &std::collections::HashSet::new(),
                None,
            ),
            None,
        );
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

    fn no_live_bound() -> std::collections::HashSet<String> {
        std::collections::HashSet::new()
    }

    fn runtime_binding(session_id: &str) -> crate::services::tui_prompt_dedupe::TuiRuntimeBinding {
        crate::services::tui_prompt_dedupe::TuiRuntimeBinding {
            runtime_kind: crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui,
            output_path: format!("/runtime/{session_id}.jsonl"),
            relay_output_path: None,
            input_fifo_path: None,
            session_id: Some(session_id.to_string()),
            last_offset: 0,
            relay_last_offset: None,
        }
    }

    #[tokio::test]
    async fn resume_production_path_clears_stale_binding_and_rebinds_runtime() {
        let _guard = crate::services::tui_prompt_dedupe::TEST_LOCK
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        crate::services::tui_prompt_dedupe::reset_state_for_tests();
        let pg_db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let shared = crate::services::discord::make_shared_data_for_tests();
        let registry = HealthRegistry::new();
        registry
            .register("claude".to_string(), Arc::clone(&shared))
            .await;
        let channel_id = ChannelId::new(4_794_101);
        let tmux = "AgentDesk-claude-resume-production-path";
        let unrelated = "AgentDesk-claude-resume-unrelated";
        let session_key = "claude/test/host:AgentDesk-claude-resume-production-path";
        let old_session_id = "11111111-1111-1111-1111-111111111111";
        let target_session_id = "22222222-2222-2222-2222-222222222222";
        let old_cwd = tempfile::tempdir().expect("old cwd");
        let target_cwd = tempfile::tempdir().expect("target cwd");
        let old_cwd = old_cwd.path().to_str().expect("utf8 old cwd");
        let target_cwd = target_cwd.path().to_str().expect("utf8 target cwd");
        // #5185: this test asserts the `DeadOrAbsent` branch. Left to the real
        // probe, that answer comes from a `tmux has-session` subprocess under a
        // two-second wall-clock bound, and a bound that is generous on an idle
        // machine is not generous inside a ~6.9k-test parallel sweep: the probe
        // times out, maps to `ProbeError`, and the assertion below sees
        // `RetainedLive`. Measured exactly that way in a full sweep, passing
        // when run alone. State the pane's liveness instead of racing for it.
        //
        // Held as a guard, not cleared after the assertions: a failing assert
        // unwinds, and a hand-rolled cleanup at the bottom of the test would
        // then leak the forced answer into the process-global override map --
        // the same shape this branch removed from `tmux_common.rs`.
        let _liveness_override = crate::services::tmux_diagnostics::PaneLivenessOverrideGuard::set(
            tmux,
            crate::services::platform::tmux::PaneLiveness::DeadOrAbsent,
        );

        sqlx::query(
            "INSERT INTO sessions
             (session_key, provider, status, cwd, claude_session_id,
              raw_provider_session_id, last_heartbeat)
             VALUES ($1, 'claude', 'idle', $2, $3, $3, NOW())",
        )
        .bind(session_key)
        .bind(old_cwd)
        .bind(old_session_id)
        .execute(&pool)
        .await
        .expect("seed resume session");
        rebind_channel_provider_session(
            &shared,
            &ProviderKind::Claude,
            channel_id,
            old_cwd,
            old_session_id,
            tmux,
            false,
        )
        .await;
        crate::services::tui_prompt_dedupe::register_provider_session(
            "claude",
            old_session_id,
            tmux,
        );
        crate::services::tui_prompt_dedupe::register_tmux_runtime_binding(
            tmux,
            runtime_binding(old_session_id),
        );
        crate::services::tui_prompt_dedupe::register_tmux_runtime_binding(
            unrelated,
            runtime_binding("unrelated-sid"),
        );

        let outcome = perform_resume_rebind(
            &pool,
            Some(&registry),
            session_key,
            Some(ProviderKind::Claude),
            Some(channel_id),
            tmux,
            &ResumePreviousOptions {
                session_id: Some(target_session_id.to_string()),
                cwd: Some(target_cwd.to_string()),
            },
        )
        .await
        .expect("resume target");

        assert_eq!(outcome.lifecycle_path, "runtime-fallback");
        assert!(outcome.in_memory_rebound);
        assert_eq!(
            outcome.runtime_binding_clear,
            ResumeRuntimeBindingClearOutcome::Cleared {
                tmux_session: tmux.to_string(),
            }
        );
        assert!(
            crate::services::tui_prompt_dedupe::runtime_binding_for_tmux_session(tmux).is_none(),
            "the retired runtime must not restore its provider session id"
        );
        assert_eq!(
            crate::services::tui_prompt_dedupe::provider_session_for_tmux("claude", tmux),
            None
        );
        assert!(
            crate::services::tui_prompt_dedupe::runtime_binding_for_tmux_session(unrelated)
                .is_some(),
            "resume must retain unrelated runtime bindings"
        );
        let (selected_session_id, selected_cwd) =
            crate::services::discord::resume_launch_state_for_tests(&shared, channel_id)
                .await
                .expect("target launch state");
        assert_eq!(selected_session_id.as_deref(), Some(target_session_id));
        assert_eq!(selected_cwd, target_cwd);

        let durable: (Option<String>, Option<String>, Option<String>) = sqlx::query_as(
            "SELECT cwd, claude_session_id, raw_provider_session_id
             FROM sessions WHERE session_key = $1",
        )
        .bind(session_key)
        .fetch_one(&pool)
        .await
        .expect("load durable resume target");
        assert_eq!(durable.0.as_deref(), Some(target_cwd));
        assert_eq!(durable.1.as_deref(), Some(target_session_id));
        assert_eq!(durable.2.as_deref(), Some(target_session_id));

        pool.close().await;
        pg_db.drop().await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn resume_production_path_retains_live_owner_after_teardown() {
        let _env_guard = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if !crate::services::platform::tmux::is_available() {
            eprintln!("skipping retained-live resume test: tmux unavailable");
            return;
        }
        let _dedupe_guard = crate::services::tui_prompt_dedupe::TEST_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        crate::services::tui_prompt_dedupe::reset_state_for_tests();
        let pg_db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let shared = crate::services::discord::make_shared_data_for_tests();
        let registry = HealthRegistry::new();
        registry
            .register("claude".to_string(), Arc::clone(&shared))
            .await;
        let channel_id = ChannelId::new(4_794_103);
        let tmux = format!("AgentDesk-resume-retained-live-{}", std::process::id());
        let session_key = format!("claude/test/host:{tmux}");
        let old_session_id = "77777777-7777-7777-7777-777777777777";
        let target_session_id = "88888888-8888-8888-8888-888888888888";
        let old_cwd = tempfile::tempdir().expect("old cwd");
        let target_cwd = tempfile::tempdir().expect("target cwd");
        let old_cwd = old_cwd.path().to_str().expect("utf8 old cwd");
        let target_cwd = target_cwd.path().to_str().expect("utf8 target cwd");

        let created = crate::services::platform::tmux::create_session(&tmux, None, "sleep 60")
            .expect("create retained-live tmux session");
        assert!(created.status.success(), "tmux session must start");
        sqlx::query(
            "INSERT INTO sessions
             (session_key, provider, status, cwd, claude_session_id,
              raw_provider_session_id, last_heartbeat)
             VALUES ($1, 'claude', 'idle', $2, $3, $3, NOW())",
        )
        .bind(&session_key)
        .bind(old_cwd)
        .bind(old_session_id)
        .execute(&pool)
        .await
        .expect("seed retained-live session");
        rebind_channel_provider_session(
            &shared,
            &ProviderKind::Claude,
            channel_id,
            old_cwd,
            old_session_id,
            &tmux,
            false,
        )
        .await;
        crate::services::discord::register_resume_watcher_for_tests(&shared, channel_id, &tmux);
        crate::services::tui_prompt_dedupe::register_provider_session(
            "claude",
            old_session_id,
            &tmux,
        );
        crate::services::tui_prompt_dedupe::register_tmux_runtime_binding(
            &tmux,
            runtime_binding(old_session_id),
        );
        crate::services::turn_lifecycle::set_force_kill_preserve_tmux_for_test(&tmux, true);

        let outcome = perform_resume_rebind(
            &pool,
            Some(&registry),
            &session_key,
            Some(ProviderKind::Claude),
            Some(channel_id),
            &tmux,
            &ResumePreviousOptions {
                session_id: Some(target_session_id.to_string()),
                cwd: Some(target_cwd.to_string()),
            },
        )
        .await
        .expect("retained-live resume target");

        assert_eq!(
            outcome.runtime_binding_clear,
            ResumeRuntimeBindingClearOutcome::RetainedLive {
                tmux_session: tmux.clone(),
            }
        );
        assert_eq!(
            crate::services::discord::resume_owner_channel_for_tests(&shared, &tmux),
            Some(channel_id),
            "production teardown must leave an authoritative owner-only binding"
        );
        assert!(
            crate::services::tui_prompt_dedupe::runtime_binding_for_tmux_session(&tmux).is_some(),
            "retained live transcript binding and offset must survive"
        );

        crate::services::turn_lifecycle::set_force_kill_preserve_tmux_for_test(&tmux, false);
        let _ = crate::services::platform::tmux::kill_session(&tmux, "retained-live test cleanup");
        pool.close().await;
        pg_db.drop().await;
    }

    #[test]
    fn resume_runtime_binding_clear_absence_is_success() {
        let _guard = crate::services::tui_prompt_dedupe::TEST_LOCK
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        crate::services::tui_prompt_dedupe::reset_state_for_tests();

        assert_eq!(
            clear_resume_runtime_binding("AgentDesk-claude-already-dead"),
            ResumeRuntimeBindingClearOutcome::AlreadyAbsent {
                tmux_session: "AgentDesk-claude-already-dead".to_string(),
            }
        );
    }

    #[tokio::test]
    async fn production_resume_lock_blocks_effective_intake_snapshot_until_rebind_finishes() {
        let pg_db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let shared = crate::services::discord::make_shared_data_for_tests();
        let registry = HealthRegistry::new();
        registry
            .register("claude".to_string(), Arc::clone(&shared))
            .await;
        let channel_id = ChannelId::new(4_794_102);
        let old_cwd = tempfile::tempdir().expect("old cwd");
        let target_cwd = tempfile::tempdir().expect("target cwd");
        let old_cwd = old_cwd.path().to_str().expect("utf8 old cwd").to_string();
        let target_cwd = target_cwd
            .path()
            .to_str()
            .expect("utf8 target cwd")
            .to_string();
        let old_session_id = "44444444-4444-4444-4444-444444444444";
        let target_session_id = "55555555-5555-5555-5555-555555555555";
        let tmux = "AgentDesk-claude-resume-intake-lock";
        let session_key = "claude/test/host:AgentDesk-claude-resume-intake-lock";
        sqlx::query(
            "INSERT INTO sessions
             (session_key, provider, status, cwd, claude_session_id,
              raw_provider_session_id, last_heartbeat)
             VALUES ($1, 'claude', 'idle', $2, $3, $3, NOW())",
        )
        .bind(session_key)
        .bind(&old_cwd)
        .bind(old_session_id)
        .execute(&pool)
        .await
        .expect("seed lock test session");
        rebind_channel_provider_session(
            &shared,
            &ProviderKind::Claude,
            channel_id,
            &old_cwd,
            old_session_id,
            tmux,
            false,
        )
        .await;

        let held = shared
            .session_transition_lock(channel_id)
            .lock_owned()
            .await;
        let resume_pool = pool.clone();
        let resume_registry = registry;
        let resume_target_cwd = target_cwd.clone();
        let resume = tokio::spawn(async move {
            perform_resume_rebind(
                &resume_pool,
                Some(&resume_registry),
                session_key,
                Some(ProviderKind::Claude),
                Some(channel_id),
                tmux,
                &ResumePreviousOptions {
                    session_id: Some(target_session_id.to_string()),
                    cwd: Some(resume_target_cwd),
                },
            )
            .await
        });
        tokio::task::yield_now().await;
        assert!(
            !resume.is_finished(),
            "production resume must acquire the channel lock"
        );
        drop(held);

        let resume_lock = shared.session_transition_lock(channel_id);
        let resume_guard = resume_lock.lock().await;
        let busy_transition =
            crate::services::discord::try_intake_runtime_transition_after_redirect(
                &shared,
                channel_id,
                (None, false, old_cwd.clone()),
            )
            .await;
        assert!(
            busy_transition.is_err(),
            "production intake must defer immediately while resume holds the lock"
        );
        drop(resume_guard);

        let outcome = resume.await.unwrap().expect("production resume succeeds");
        assert!(outcome.in_memory_rebound);
        let transition = crate::services::discord::try_intake_runtime_transition_after_redirect(
            &shared,
            channel_id,
            (None, false, old_cwd.clone()),
        )
        .await
        .expect("production intake acquires the released transition lock");
        assert_eq!(transition.state.0.as_deref(), Some(target_session_id));
        assert_eq!(transition.state.2, target_cwd);
        let blocked_resume_pool = pool.clone();
        let blocked_resume_registry = HealthRegistry::new();
        blocked_resume_registry
            .register("claude".to_string(), Arc::clone(&shared))
            .await;
        let blocked_target_cwd = target_cwd.clone();
        let blocked_resume = tokio::spawn(async move {
            perform_resume_rebind(
                &blocked_resume_pool,
                Some(&blocked_resume_registry),
                session_key,
                Some(ProviderKind::Claude),
                Some(channel_id),
                tmux,
                &ResumePreviousOptions {
                    session_id: Some(target_session_id.to_string()),
                    cwd: Some(blocked_target_cwd),
                },
            )
            .await
        });
        let (claim_started_tx, claim_started_rx) = tokio::sync::oneshot::channel();
        let (claim_release_tx, claim_release_rx) = tokio::sync::oneshot::channel();
        let claim = tokio::spawn(async move {
            transition
                .complete_mailbox_claim(async move {
                    let _ = claim_started_tx.send(());
                    let _ = claim_release_rx.await;
                })
                .await;
        });
        claim_started_rx
            .await
            .expect("intake reaches the production claim span");
        tokio::task::yield_now().await;
        assert!(
            !blocked_resume.is_finished(),
            "resume must remain excluded until the intake claim completes"
        );
        let _ = claim_release_tx.send(());
        claim.await.expect("claim span completes");
        blocked_resume
            .await
            .expect("blocked resume task joins")
            .expect("blocked resume succeeds after the claim");

        pool.close().await;
        pg_db.drop().await;
    }

    #[test]
    fn lineage_stem_strips_only_datetime_suffix() {
        assert_eq!(
            worktree_lineage_stem("claude-adk-cc-20260723-054531"),
            "claude-adk-cc"
        );
        assert_eq!(
            worktree_lineage_stem("claude-adk-cc-20260723-050333"),
            "claude-adk-cc"
        );
        // Different agent slug — different lineage.
        assert_eq!(
            worktree_lineage_stem("claude-adk-dash-cc"),
            "claude-adk-dash-cc"
        );
        // No datetime suffix — the whole name is the stem.
        assert_eq!(worktree_lineage_stem("issue-123-fix"), "issue-123-fix");
    }

    #[test]
    fn discover_skips_current_and_picks_newest_prior_in_lineage() {
        let tmp = unique_tmp("newest");
        let claude_home = tmp.join(".claude");
        let parent = tmp.join("worktrees");
        // Same lineage stem `claude-adk-cc`, different timestamps.
        let cwd_a = parent.join("claude-adk-cc-20260101-000001");
        let cwd_b = parent.join("claude-adk-cc-20260101-000002");
        std::fs::create_dir_all(&cwd_a).unwrap();
        std::fs::create_dir_all(&cwd_b).unwrap();

        let current = "11111111-1111-1111-1111-111111111111";
        let prior_a = "22222222-2222-2222-2222-222222222222";
        let prior_b = "33333333-3333-3333-3333-333333333333";

        // Current binding is the newest file overall, but must be skipped.
        write_transcript(&claude_home, &cwd_a, current, 3_000);
        write_transcript(&claude_home, &cwd_a, prior_a, 1_000);
        // Sibling (same lineage) holds the newest *prior* transcript.
        write_transcript(&claude_home, &cwd_b, prior_b, 2_000);

        let result = discover_previous_claude_session_scoped(
            Some(cwd_a.to_str().unwrap()),
            Some(current),
            &no_live_bound(),
            Some(&claude_home),
        )
        .expect("a prior session should be selected");

        assert_eq!(result.session_id, prior_b, "newest prior transcript wins");
        assert_eq!(result.cwd, cwd_b.to_string_lossy());

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn discover_ignores_out_of_lineage_sibling_worktree() {
        let tmp = unique_tmp("lineage");
        let claude_home = tmp.join(".claude");
        let parent = tmp.join("worktrees");
        let cwd = parent.join("claude-adk-cc-20260101-000001");
        // Unrelated agent (different lineage stem) with a NEWER transcript.
        let other_agent = parent.join("claude-adk-dash-cc-20260101-000009");
        std::fs::create_dir_all(&cwd).unwrap();
        std::fs::create_dir_all(&other_agent).unwrap();

        let current = "11111111-1111-1111-1111-111111111111";
        let dash_prior = "99999999-9999-9999-9999-999999999999";
        write_transcript(&claude_home, &cwd, current, 1_000);
        // Newer, but belongs to a different agent lineage → must be ignored.
        write_transcript(&claude_home, &other_agent, dash_prior, 9_000);

        assert_eq!(
            discover_previous_claude_session_scoped(
                Some(cwd.to_str().unwrap()),
                Some(current),
                &no_live_bound(),
                Some(&claude_home),
            ),
            None,
            "an out-of-lineage sibling's transcript must never be auto-selected",
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn discover_excludes_live_bound_session() {
        let tmp = unique_tmp("live-bound");
        let claude_home = tmp.join(".claude");
        let parent = tmp.join("worktrees");
        let cwd = parent.join("claude-adk-cc-20260101-000001");
        std::fs::create_dir_all(&cwd).unwrap();

        let current = "11111111-1111-1111-1111-111111111111";
        let prior_bound = "22222222-2222-2222-2222-222222222222";
        write_transcript(&claude_home, &cwd, current, 1_000);
        write_transcript(&claude_home, &cwd, prior_bound, 2_000);

        // The only prior transcript is currently bound to some live channel.
        let mut live = std::collections::HashSet::new();
        live.insert(prior_bound.to_string());

        assert_eq!(
            discover_previous_claude_session_scoped(
                Some(cwd.to_str().unwrap()),
                Some(current),
                &live,
                Some(&claude_home),
            ),
            None,
            "a session another channel is actively bound to must not be adopted",
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn discover_returns_none_when_only_current_binding_exists() {
        let tmp = unique_tmp("only-current");
        let claude_home = tmp.join(".claude");
        let parent = tmp.join("worktrees");
        let cwd = parent.join("claude-adk-cc-20260101-000001");
        std::fs::create_dir_all(&cwd).unwrap();

        let current = "44444444-4444-4444-4444-444444444444";
        write_transcript(&claude_home, &cwd, current, 5_000);

        assert_eq!(
            discover_previous_claude_session_scoped(
                Some(cwd.to_str().unwrap()),
                Some(current),
                &no_live_bound(),
                Some(&claude_home),
            ),
            None,
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }
}
