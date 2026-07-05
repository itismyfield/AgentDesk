use super::*;

pub(super) const CLAUDE_TUI_BUSY_FOLLOWUP_NOTICE: &str = "⚠ Claude TUI가 아직 이전 터미널 턴을 처리 중이라 이 메시지를 주입하지 않았습니다. 현재 응답이 끝난 뒤 다시 보내 주세요.";
pub(super) const CLAUDE_TUI_BUSY_FOLLOWUP_ALREADY_QUEUED_NOTICE: &str =
    "📬 이 메시지는 이미 큐에 들어가 있어 추가 적재하지 않았습니다. 큐 결과를 기다려 주세요.";
pub(super) const CLAUDE_TUI_BUSY_FOLLOWUP_ALREADY_ACTIVE_NOTICE: &str =
    "📬 이 메시지는 이미 처리 중이라 추가 적재하지 않았습니다. 현재 결과를 기다려 주세요.";
pub(super) const CLAUDE_TUI_BUSY_FOLLOWUP_DEDUP_NOTICE: &str =
    "📬 방금 동일한 메시지가 큐에 적재되어 중복으로 무시했습니다. 큐 결과를 기다려 주세요.";
pub(super) const CLAUDE_TUI_BUSY_FOLLOWUP_QUEUE_UNREACHABLE_NOTICE: &str =
    "⚠ 내부 처리 큐에 접근하지 못해 이 메시지를 적재하지 못했습니다. 잠시 후 다시 보내 주세요.";
pub(super) fn claude_tui_busy_followup_refusal_notice(
    reason: Option<crate::services::turn_orchestrator::EnqueueRefusalReason>,
) -> &'static str {
    match reason {
        Some(crate::services::turn_orchestrator::EnqueueRefusalReason::AlreadyActiveTurn) => {
            CLAUDE_TUI_BUSY_FOLLOWUP_ALREADY_ACTIVE_NOTICE
        }
        Some(crate::services::turn_orchestrator::EnqueueRefusalReason::SourceIdAlreadyQueued) => {
            CLAUDE_TUI_BUSY_FOLLOWUP_ALREADY_QUEUED_NOTICE
        }
        Some(crate::services::turn_orchestrator::EnqueueRefusalReason::LastItemDedup) => {
            CLAUDE_TUI_BUSY_FOLLOWUP_DEDUP_NOTICE
        }
        // #3297 r3 — a post-retry purge-tombstone refusal is user-actionable
        // the same way as an unreachable actor: resend shortly.
        Some(crate::services::turn_orchestrator::EnqueueRefusalReason::ActorUnreachable)
        | Some(crate::services::turn_orchestrator::EnqueueRefusalReason::MailboxClosed) => {
            CLAUDE_TUI_BUSY_FOLLOWUP_QUEUE_UNREACHABLE_NOTICE
        }
        None => CLAUDE_TUI_BUSY_FOLLOWUP_NOTICE,
    }
}

// #3813 Phase 3 (§4 / AC#6): compact operational status shown on the intake
// placeholder while the hosted-TUI busy preflight readiness wait blocks
// (up to ~45s). Transient — replaced by dispatch streaming on success or by the
// queued-card / delete / refusal-notice paths on the busy branch.
#[cfg(unix)]
pub(super) const HOSTED_TUI_READINESS_WAIT_NOTICE: &str =
    "⏳ TUI 준비 대기 중… 이전 터미널 턴이 끝나면 이어서 처리합니다.";

#[cfg(unix)]
pub(super) fn readiness_wait_compact_status(
    wait: &HostedTuiBusyPreflightReadinessWait,
) -> &'static str {
    match wait {
        HostedTuiBusyPreflightReadinessWait::Codex
        | HostedTuiBusyPreflightReadinessWait::ClaudePromptMarkerOnly
        | HostedTuiBusyPreflightReadinessWait::ClaudePromptMarkerOrIdleTranscript(_) => {
            HOSTED_TUI_READINESS_WAIT_NOTICE
        }
    }
}

#[cfg(unix)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ClaudeTuiBusyFollowupDiagnostic {
    pub(super) tmux_session_name: String,
    pub(super) prompt_marker_detected: bool,
    pub(super) prompt_draft_detected: bool,
    pub(super) previous_tui_turn_still_running: bool,
    pub(super) tmux_pane_alive: bool,
    pub(super) capture_available: bool,
    pub(super) watcher_state: &'static str,
    pub(super) watcher_owner_channel_id: Option<u64>,
    pub(super) inflight_state: &'static str,
    pub(super) transcript_turn_state: crate::services::tui_turn_state::TuiTurnState,
    pub(super) pane_tail: String,
}

#[cfg(unix)]
impl ClaudeTuiBusyFollowupDiagnostic {
    pub(super) fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "tmux_session_name": self.tmux_session_name,
            "prompt_marker_detected": self.prompt_marker_detected,
            "prompt_draft_detected": self.prompt_draft_detected,
            "previous_tui_turn_still_running": self.previous_tui_turn_still_running,
            "tmux_pane_alive": self.tmux_pane_alive,
            "capture_available": self.capture_available,
            "watcher_state": self.watcher_state,
            "watcher_owner_channel_id": self.watcher_owner_channel_id,
            "inflight_state": self.inflight_state,
            "transcript_turn_state": self.transcript_turn_state.as_str(),
            "pane_tail": self.pane_tail,
        })
    }
}

#[cfg(unix)]
pub(super) fn classify_inflight_diagnostic_state(
    inflight: Option<&InflightTurnState>,
) -> &'static str {
    let Some(inflight) = inflight else {
        return "missing";
    };
    let Some(updated_at_unix) =
        super::super::super::inflight::parse_updated_at_unix(&inflight.updated_at)
    else {
        return "stale_unparseable_updated_at";
    };
    let age_secs = chrono::Local::now()
        .timestamp()
        .saturating_sub(updated_at_unix);
    if age_secs >= super::super::super::inflight::INFLIGHT_STALENESS_THRESHOLD_SECS as i64 {
        "stale"
    } else if inflight.effective_relay_owner_kind()
        == super::super::super::inflight::RelayOwnerKind::Watcher
    {
        "watcher_owned"
    } else if inflight.effective_relay_owner_kind()
        == super::super::super::inflight::RelayOwnerKind::StandbyRelay
    {
        "standby_relay_owned"
    } else if inflight.effective_relay_owner_kind()
        == super::super::super::inflight::RelayOwnerKind::Unknown
    {
        "relay_owner_unknown"
    } else {
        "present"
    }
}

#[cfg(unix)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct HostedTuiPromptReadinessSnapshot {
    pub(super) prompt_marker_detected: bool,
    pub(super) prompt_draft_detected: bool,
    pub(super) tmux_pane_alive: bool,
    pub(super) capture_available: bool,
    pub(super) pane_tail: String,
}

#[cfg(unix)]
impl HostedTuiPromptReadinessSnapshot {
    pub(super) fn jsonl_authoritative(tmux_pane_alive: bool) -> Self {
        Self {
            prompt_marker_detected: false,
            prompt_draft_detected: false,
            tmux_pane_alive,
            capture_available: false,
            pane_tail: "<not captured; JSONL turn state is authoritative>".to_string(),
        }
    }
}

#[cfg(unix)]
pub(super) fn classify_claude_tui_followup_submission(
    snapshot: &HostedTuiPromptReadinessSnapshot,
    watcher_state: &'static str,
    watcher_owner_channel_id: Option<u64>,
    inflight_state: &'static str,
    transcript_turn_state: crate::services::tui_turn_state::TuiTurnState,
    tmux_session_name: &str,
) -> Option<ClaudeTuiBusyFollowupDiagnostic> {
    let structured_turn_busy = transcript_turn_state.is_busy();
    let draft_blocks_submission =
        snapshot.tmux_pane_alive && snapshot.prompt_draft_detected && inflight_state != "missing";
    if !structured_turn_busy && !draft_blocks_submission {
        return None;
    }
    Some(ClaudeTuiBusyFollowupDiagnostic {
        tmux_session_name: tmux_session_name.to_string(),
        prompt_marker_detected: snapshot.prompt_marker_detected,
        prompt_draft_detected: snapshot.prompt_draft_detected,
        previous_tui_turn_still_running: structured_turn_busy,
        tmux_pane_alive: snapshot.tmux_pane_alive,
        capture_available: snapshot.capture_available,
        watcher_state,
        watcher_owner_channel_id,
        inflight_state,
        transcript_turn_state,
        pane_tail: snapshot.pane_tail.clone(),
    })
}

#[cfg(unix)]
pub(super) fn hosted_tui_draft_should_enter_provider_recovery(
    provider: &ProviderKind,
    snapshot: &HostedTuiPromptReadinessSnapshot,
) -> bool {
    matches!(provider, ProviderKind::Codex)
        && snapshot.tmux_pane_alive
        && snapshot.prompt_marker_detected
        && snapshot.prompt_draft_detected
}

/// #3208: resolve the JSONL transcript for the Claude TUI session that is
/// *actually* serving this channel's tmux session.
///
/// The naive resolution `claude_transcript_path(current_path, session_id)` is
/// brittle in production:
///   - `session_id` is frequently `None` / a non-UUID fingerprint on the
///     Discord follow-up path (sessions resume via `runtime_cached_provider_session`
///     and the real Claude session_id UUID is never carried into intake).
///   - `current_path` is the channel's *configured* workspace, but the live TUI
///     often runs in a rotating worktree (`worktrees/claude-adk-cc-<ts>`) — the
///     DB-restored worktree cwd is ignored at turn start. The workspace project
///     dir then holds only stale transcripts, so the probe reads `Unknown` (or a
///     stale `Idle`), and the screen-marker fallback false-flags a genuinely
///     idle (background-agents-running) turn as busy → the 45s readiness
///     timeout in #3208.
///
/// #3212 (codex P1): the bare newest-in-cwd fallback (`latest_claude_transcript_for_cwd`
/// with `UNIX_EPOCH` and an empty exclude) trusts newest mtime under the pane
/// cwd with NO per-session identity. When two same-cwd sessions run
/// concurrently it can adopt the WRONG transcript:
///   - a finished OTHER session's Idle transcript → false-ready → injects a
///     follow-up into a still-busy TUI, or
///   - another still-Busy transcript → wrong queue / requeue loop.
/// The fix establishes a strong per-session identity BEFORE the cwd-mtime
/// fallback, and hardens the fallback itself.
///
/// Resolution order (strongest identity first):
///   1. The live runtime binding's `output_path` for this tmux session — this
///      is the watcher's output transcript for the *actual* session and is the
///      only per-session identity we carry. Trusted when the file exists.
///   2. `claude_transcript_path(current_path, session_id)` when it both has a
///      valid UUID and the file exists (the happy path).
///   3. newest UUID transcript under the live tmux pane's *actual* cwd
///      (`pane_cwd`), then `current_path`, BUT only with:
///        - a `launch_mtime_cutoff` floor (reject transcripts older than this
///          session's launch — they belong to a prior session), and
///        - the already-claimed transcripts (`exclude`) of OTHER live sessions
///          filtered out, and
///        - an ambiguity guard: when MORE THAN ONE qualifying transcript exists
///          in a cwd and we have no stronger identity, we refuse to guess
///          (return `None`) rather than risk false-ready / false-busy.
// #3034: transcript-path resolver exercised by the session-strategy lifecycle
// tests; the live followup path resolves transcripts elsewhere. Test contract.
#[cfg(unix)]
#[allow(dead_code)]
pub(super) fn resolve_claude_followup_transcript_path(
    current_path: Option<&str>,
    session_id: Option<&str>,
    pane_cwd: Option<&std::path::Path>,
    claude_home: Option<&std::path::Path>,
) -> Option<std::path::PathBuf> {
    resolve_claude_followup_transcript_path_with_identity(
        current_path,
        session_id,
        pane_cwd,
        claude_home,
        None,
        Some(std::time::SystemTime::UNIX_EPOCH),
        &std::collections::HashSet::new(),
    )
}

/// #3212: identity-aware resolver. `runtime_binding` is the strongest per-session
/// identity (the live watcher output transcript path); `launch_mtime_cutoff`
/// floors the cwd-mtime fallback so a finished prior session's transcript is
/// never adopted; `exclude` drops transcripts already claimed by OTHER live
/// sessions. Production wrappers supply these from the tmux runtime binding
/// table; tests drive them directly. The bare 4-arg wrapper above keeps the
/// previous call sites compiling (with the legacy permissive behaviour) but is
/// no longer used on the production follow-up path.
///
/// #3212 (codex P1-1): `launch_mtime_cutoff` is `Option`:
///   - `Some(t)` — only cwd-fallback transcripts modified at/after `t` (this
///     session's launch) qualify. A stale prior-session transcript is rejected.
///   - `None` — the launch time could NOT be reliably obtained. The cwd-mtime
///     fallback is then DISABLED entirely (we never adopt an unverified
///     candidate). Stronger identities (runtime binding, exact UUID) still
///     resolve; otherwise we return `None` (prompt-marker-only) and accept the
///     minor false-busy over the false-ready of adopting an unverifiable
///     transcript.
///
/// #3212 (codex P1-2): the ambiguity guard is a HARD stop. Candidates from BOTH
/// `pane_cwd` AND `current_path` are collected into one set; if more than one
/// qualifies (after cutoff + exclude) with no stronger identity, we return
/// `None` rather than fall through and adopt a single `current_path` candidate.
#[cfg(unix)]
pub(super) fn resolve_claude_followup_transcript_path_with_identity(
    current_path: Option<&str>,
    session_id: Option<&str>,
    pane_cwd: Option<&std::path::Path>,
    claude_home: Option<&std::path::Path>,
    runtime_binding: Option<&crate::services::tui_prompt_dedupe::TuiRuntimeBinding>,
    launch_mtime_cutoff: Option<std::time::SystemTime>,
    exclude: &std::collections::HashSet<std::path::PathBuf>,
) -> Option<std::path::PathBuf> {
    // 1. Strongest identity: the live runtime binding's output transcript for
    //    THIS tmux session. This is the only path we carry that is bound to the
    //    actual session, so it disambiguates concurrent same-cwd sessions.
    if let Some(binding) = runtime_binding.filter(|binding| {
        binding.runtime_kind == crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui
    }) {
        let bound_path = std::path::PathBuf::from(binding.output_path.trim());
        if !binding.output_path.trim().is_empty() && bound_path.exists() {
            return Some(bound_path);
        }
    }

    // 2. Happy path: exact (current_path, session_id) UUID transcript.
    if let (Some(current_path), Some(session_id)) = (current_path, session_id)
        && let Ok(path) = crate::services::claude_tui::transcript_tail::claude_transcript_path(
            std::path::Path::new(current_path),
            session_id,
            claude_home,
        )
        && path.exists()
    {
        return Some(path);
    }

    // 3. cwd-mtime fallback, but guarded.
    //
    // P1-1: with no reliable launch cutoff we MUST NOT adopt any unverified
    // candidate — disable the fallback and return None (prompt-marker-only).
    let Some(launch_mtime_cutoff) = launch_mtime_cutoff else {
        return None;
    };
    // P1-2: collect candidates across BOTH cwds into ONE set, then apply a HARD
    // ambiguity guard. Picking newest-mtime among >1 candidate is exactly the
    // false-ready / false-busy bug; a per-cwd `continue` could fall through and
    // adopt a single `current_path` candidate while pane_cwd was ambiguous —
    // that is forbidden. Only a single unambiguous candidate across all cwds
    // (at/after launch, after exclude) may be adopted.
    let mut candidate_cwds: Vec<std::path::PathBuf> = Vec::new();
    if let Some(pane_cwd) = pane_cwd {
        candidate_cwds.push(pane_cwd.to_path_buf());
    }
    if let Some(current_path) = current_path {
        let workspace = std::path::PathBuf::from(current_path);
        if !candidate_cwds.contains(&workspace) {
            candidate_cwds.push(workspace);
        }
    }
    let mut all_candidates: Vec<std::path::PathBuf> = Vec::new();
    let mut seen: std::collections::HashSet<std::path::PathBuf> = std::collections::HashSet::new();
    for cwd in candidate_cwds {
        for path in crate::services::claude_tui::transcript_tail::claude_transcripts_for_cwd_since(
            &cwd,
            launch_mtime_cutoff,
            claude_home,
            exclude,
        ) {
            if seen.insert(path.clone()) {
                all_candidates.push(path);
            }
        }
        // Short-circuit: once two distinct candidates exist anywhere we are
        // already ambiguous and will return None regardless of the next cwd.
        if all_candidates.len() > 1 {
            return None;
        }
    }
    match all_candidates.len() {
        1 => Some(all_candidates.into_iter().next().expect("len == 1")),
        // 0 → nothing qualifies; >1 → ambiguous concurrent sessions. Never guess.
        _ => None,
    }
}

/// #3212 (codex P1-1): the launch-mtime cutoff for the cwd-fallback, sourced
/// from the live Claude process's start time (the tmux pane PID's start time).
/// `None` when the pane PID or its start time cannot be obtained — callers then
/// take the conservative no-fallback path rather than risk adopting a stale
/// same-cwd transcript (false-ready).
#[cfg(unix)]
fn claude_session_launch_mtime_cutoff(
    tmux_session_name: Option<&str>,
) -> Option<std::time::SystemTime> {
    let pid = crate::services::platform::tmux::pane_pid(tmux_session_name?)?;
    crate::services::platform::tmux::process_start_time(pid)
}

#[cfg(unix)]
pub(super) fn observe_claude_tui_transcript_state_for_session(
    current_path: Option<&str>,
    session_id: Option<&str>,
    tmux_session_name: Option<&str>,
) -> crate::services::tui_turn_state::TuiTurnState {
    let pane_cwd = tmux_session_name
        .and_then(crate::services::tmux_diagnostics::tmux_session_pane_cwd)
        .map(std::path::PathBuf::from);
    let runtime_binding = tmux_session_name
        .and_then(crate::services::tui_prompt_dedupe::runtime_binding_for_tmux_session);
    let launch_mtime_cutoff = claude_session_launch_mtime_cutoff(tmux_session_name);
    let Some(transcript_path) = resolve_claude_followup_transcript_path_with_identity(
        current_path,
        session_id,
        pane_cwd.as_deref(),
        None,
        runtime_binding.as_ref(),
        launch_mtime_cutoff,
        &std::collections::HashSet::new(),
    ) else {
        return crate::services::tui_turn_state::TuiTurnState::Unknown;
    };
    let provider = ProviderKind::Claude;
    let probe =
        crate::services::tui_turn_state::JsonlTurnStateProbe::new(&provider, &transcript_path);
    crate::services::tui_turn_state::TuiTurnStateProbe::observe(&probe)
}

#[cfg(unix)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum HostedTuiBusyPreflightReadinessWait {
    Codex,
    ClaudePromptMarkerOnly,
    ClaudePromptMarkerOrIdleTranscript(std::path::PathBuf),
}

#[cfg(unix)]
pub(super) fn hosted_tui_busy_preflight_readiness_wait(
    provider: &ProviderKind,
    current_path: Option<&str>,
    session_id: Option<&str>,
    tmux_session_name: Option<&str>,
) -> HostedTuiBusyPreflightReadinessWait {
    let pane_cwd = tmux_session_name
        .and_then(crate::services::tmux_diagnostics::tmux_session_pane_cwd)
        .map(std::path::PathBuf::from);
    let runtime_binding = tmux_session_name
        .and_then(crate::services::tui_prompt_dedupe::runtime_binding_for_tmux_session);
    let launch_mtime_cutoff = claude_session_launch_mtime_cutoff(tmux_session_name);
    hosted_tui_busy_preflight_readiness_wait_with_claude_home(
        provider,
        current_path,
        session_id,
        pane_cwd.as_deref(),
        None,
        runtime_binding.as_ref(),
        launch_mtime_cutoff,
    )
}

#[cfg(unix)]
pub(super) fn hosted_tui_busy_preflight_readiness_wait_with_claude_home(
    provider: &ProviderKind,
    current_path: Option<&str>,
    session_id: Option<&str>,
    pane_cwd: Option<&std::path::Path>,
    claude_home: Option<&std::path::Path>,
    runtime_binding: Option<&crate::services::tui_prompt_dedupe::TuiRuntimeBinding>,
    launch_mtime_cutoff: Option<std::time::SystemTime>,
) -> HostedTuiBusyPreflightReadinessWait {
    if matches!(provider, ProviderKind::Codex) {
        return HostedTuiBusyPreflightReadinessWait::Codex;
    }
    // #3208: resolve the *running* session's transcript (worktree-aware), not
    // just `claude_transcript_path(current_path, session_id)`, so the idle
    // JSONL fallback engages for sessions running in a rotating worktree.
    // #3212: prefer the runtime binding's per-session output transcript over the
    // ambiguous newest-in-cwd guess so we never wait on the wrong session; the
    // launch-mtime cutoff (P1-1) floors the cwd fallback to this session's launch.
    let Some(transcript_path) = resolve_claude_followup_transcript_path_with_identity(
        current_path,
        session_id,
        pane_cwd,
        claude_home,
        runtime_binding,
        launch_mtime_cutoff,
        &std::collections::HashSet::new(),
    ) else {
        return HostedTuiBusyPreflightReadinessWait::ClaudePromptMarkerOnly;
    };
    // Missing Claude JSONL files currently observe as Idle. Only pass a
    // transcript path to the fallback once the file exists, so cold sessions
    // still require the visible prompt marker before we inject a follow-up.
    if transcript_path.exists() {
        HostedTuiBusyPreflightReadinessWait::ClaudePromptMarkerOrIdleTranscript(transcript_path)
    } else {
        HostedTuiBusyPreflightReadinessWait::ClaudePromptMarkerOnly
    }
}

#[cfg(unix)]
pub(super) fn observe_codex_tui_rollout_state_for_cwd(
    current_path: Option<&str>,
    tmux_session_name: Option<&str>,
    provider_session_id: Option<&str>,
) -> crate::services::tui_turn_state::TuiTurnState {
    let runtime_binding = tmux_session_name
        .and_then(crate::services::tui_prompt_dedupe::runtime_binding_for_tmux_session)
        .filter(|binding| {
            binding.runtime_kind == crate::services::agent_protocol::RuntimeHandoffKind::CodexTui
        });
    observe_codex_tui_rollout_state_for_cwd_with_sessions(
        current_path,
        provider_session_id,
        None,
        runtime_binding.as_ref(),
    )
}

#[cfg(unix)]
pub(super) fn observe_codex_tui_rollout_state_for_cwd_with_sessions(
    current_path: Option<&str>,
    provider_session_id: Option<&str>,
    sessions_dir: Option<&std::path::Path>,
    runtime_binding: Option<&crate::services::tui_prompt_dedupe::TuiRuntimeBinding>,
) -> crate::services::tui_turn_state::TuiTurnState {
    let Some(current_path) = current_path else {
        return crate::services::tui_turn_state::TuiTurnState::Unknown;
    };
    let cwd = std::path::Path::new(current_path);
    if let Some(binding) = runtime_binding {
        let rollout_path = std::path::Path::new(&binding.output_path);
        if std::fs::metadata(rollout_path).is_err() {
            return crate::services::tui_turn_state::TuiTurnState::Unknown;
        }
        if !crate::services::codex_tui::rollout_tail::rollout_file_matches_cwd(rollout_path, cwd) {
            return crate::services::tui_turn_state::TuiTurnState::Unknown;
        }
        return crate::services::codex_tui::rollout_tail::observe_rollout_turn_state(rollout_path);
    }
    let resolved = sessions_dir
        .map(std::path::Path::to_path_buf)
        .or_else(|| crate::services::codex_tui::rollout_tail::default_codex_sessions_dir());
    let Some(sessions_dir) = resolved else {
        return crate::services::tui_turn_state::TuiTurnState::Unknown;
    };
    if let Some(provider_session_id) = provider_session_id
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        let selection = crate::services::codex_tui::session::resolve_codex_tui_session(
            Some(provider_session_id),
            cwd,
            Some(&sessions_dir),
            false,
        );
        if let Some(rollout_path) = selection.rollout_path.as_deref() {
            return crate::services::codex_tui::rollout_tail::observe_rollout_turn_state(
                rollout_path,
            );
        }
        return crate::services::tui_turn_state::TuiTurnState::Unknown;
    }
    let Some(rollout_path) = crate::services::codex_tui::rollout_tail::latest_rollout_for_cwd_since(
        cwd,
        std::time::SystemTime::UNIX_EPOCH,
        &sessions_dir,
    ) else {
        // No rollout file found for this cwd — treat as idle (session not yet started).
        return crate::services::tui_turn_state::TuiTurnState::Idle;
    };
    let rollout_state =
        crate::services::codex_tui::rollout_tail::observe_rollout_turn_state(&rollout_path);
    if rollout_state.is_busy() {
        return rollout_state;
    }
    crate::services::tui_turn_state::TuiTurnState::Unknown
}

#[cfg(unix)]
pub(super) fn tui_busy_followup_diagnostic(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: serenity::ChannelId,
    tmux_session_name: Option<&str>,
    remote_profile_present: bool,
    current_path: Option<&str>,
    session_id: Option<&str>,
) -> Option<ClaudeTuiBusyFollowupDiagnostic> {
    if !matches!(provider, ProviderKind::Claude | ProviderKind::Codex) || remote_profile_present {
        return None;
    }
    let tmux_session_name = tmux_session_name?;
    let selection =
        crate::services::provider_hosting::resolve_provider_session_selection_with_channel(
            provider,
            claude::is_tmux_available(),
            Some(channel_id.get()),
        );
    if selection.driver != crate::services::provider_hosting::ProviderSessionDriver::TuiHosting
        || crate::services::claude_tui::hook_server::current_hook_endpoint().is_none()
        || !crate::services::tmux_diagnostics::tmux_session_has_live_pane(tmux_session_name)
    {
        return None;
    }

    let watcher_entry = shared
        .tmux_watchers
        .iter()
        .find(|entry| entry.tmux_session_name == tmux_session_name);
    let owner_channel_id = shared
        .tmux_watchers
        .owner_channel_for_tmux_session(tmux_session_name)
        .map(|channel_id| channel_id.get());
    let (watcher_state, watcher_owner_channel_id) = watcher_entry
        .as_ref()
        .map(|entry| {
            let state = if entry.cancel.load(std::sync::atomic::Ordering::Relaxed) {
                "cancelled"
            } else if entry.heartbeat_stale() {
                "stale"
            } else if entry.paused.load(std::sync::atomic::Ordering::Relaxed) {
                "paused"
            } else {
                "attached"
            };
            (state, owner_channel_id)
        })
        .unwrap_or(("missing", None));
    let previous_inflight =
        super::super::super::inflight::load_inflight_state(provider, channel_id.get());
    let inflight_state = classify_inflight_diagnostic_state(previous_inflight.as_ref());
    let transcript_turn_state = match provider {
        ProviderKind::Claude => observe_claude_tui_transcript_state_for_session(
            current_path,
            session_id,
            Some(tmux_session_name),
        ),
        ProviderKind::Codex => observe_codex_tui_rollout_state_for_cwd(
            current_path,
            Some(tmux_session_name),
            session_id,
        ),
        _ => crate::services::tui_turn_state::TuiTurnState::Unknown,
    };
    if transcript_turn_state == crate::services::tui_turn_state::TuiTurnState::Idle {
        return None;
    }
    if transcript_turn_state.is_busy() {
        // #3981: a turn stopped immediately (⏳-removal / `!stop` / `/stop` /
        // watchdog) before claude wrote a terminator or the
        // `[Request interrupted by user]` marker leaves a trailing bare
        // `type=user` envelope. The pure classifier (`tui_turn_state.rs`) only
        // sees disk shape and structurally reports `UserSubmitted` (busy)
        // forever, so the next message wedges into `*_tui_busy_pre_submit` with
        // no terminator ever arriving to dequeue it. `Streaming` (an `assistant`
        // envelope already exists) is unconditionally live and is NEVER
        // reclaimed here (INV-1).
        //
        // For a Claude `UserSubmitted` only, corroborate "stranded/stopped" with
        // TWO independent runtime signals before trusting the busy verdict
        // (INV-2, AND): (a) runtime-activity quiescence — no relay
        // jsonl/`.generation` mtime advance for >=
        // STALE_USER_SUBMITTED_RECLAIM_SECS — and (b) the live pane shows the
        // at-rest prompt marker, which is suppressed during a genuine agentic
        // turn (`intake_turn.rs` #3208 A), so marker=true ⟹ not mid-turn. Only
        // when BOTH hold do we fall through to `None` (pass) instead of emitting
        // the busy diagnostic. Codex is left on the existing JSONL-authoritative
        // path (its composer marker semantics are out of scope for #3981).
        if matches!(provider, ProviderKind::Claude)
            && transcript_turn_state == crate::services::tui_turn_state::TuiTurnState::UserSubmitted
        {
            let activity_age_secs =
                crate::services::tui_turn_state::runtime_activity_age_secs(tmux_session_name);
            let prompt_marker_detected =
                crate::services::claude_tui::input::prompt_readiness_snapshot(tmux_session_name)
                    .prompt_marker_detected;
            if crate::services::tui_turn_state::user_submitted_is_stale_stranded(
                transcript_turn_state,
                activity_age_secs,
                prompt_marker_detected,
            ) {
                return None;
            }
        }
        let snapshot = HostedTuiPromptReadinessSnapshot::jsonl_authoritative(true);
        return classify_claude_tui_followup_submission(
            &snapshot,
            watcher_state,
            watcher_owner_channel_id,
            inflight_state,
            transcript_turn_state,
            tmux_session_name,
        );
    }

    let snapshot = match provider {
        ProviderKind::Codex => {
            let snapshot =
                crate::services::codex_tui::input::prompt_readiness_snapshot(tmux_session_name);
            HostedTuiPromptReadinessSnapshot {
                prompt_marker_detected: snapshot.composer_marker_detected,
                prompt_draft_detected: snapshot.prompt_draft_detected,
                tmux_pane_alive: snapshot.tmux_pane_alive,
                capture_available: snapshot.capture_available,
                pane_tail: snapshot.pane_tail,
            }
        }
        _ => {
            let snapshot =
                crate::services::claude_tui::input::prompt_readiness_snapshot(tmux_session_name);
            HostedTuiPromptReadinessSnapshot {
                prompt_marker_detected: snapshot.prompt_marker_detected,
                prompt_draft_detected: snapshot.prompt_draft_detected,
                tmux_pane_alive: snapshot.tmux_pane_alive,
                capture_available: snapshot.capture_available,
                pane_tail: snapshot.pane_tail,
            }
        }
    };
    if hosted_tui_draft_should_enter_provider_recovery(provider, &snapshot) {
        return None;
    }
    classify_claude_tui_followup_submission(
        &snapshot,
        watcher_state,
        watcher_owner_channel_id,
        inflight_state,
        transcript_turn_state,
        tmux_session_name,
    )
}

pub(super) async fn enqueue_busy_tui_followup_for_retry(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: serenity::ChannelId,
    request_owner: serenity::UserId,
    user_msg_id: serenity::MessageId,
    user_text: &str,
    reply_context: Option<String>,
    has_reply_boundary: bool,
    merge_consecutive: bool,
    pending_uploads: Vec<String>,
    voice_announcement: Option<crate::voice::prompt::VoiceTranscriptAnnouncement>,
) -> MailboxEnqueueOutcome {
    super::super::super::mailbox_enqueue_intervention(
        shared,
        provider,
        channel_id,
        build_race_requeued_intervention(
            request_owner,
            user_msg_id,
            user_text,
            reply_context,
            has_reply_boundary,
            merge_consecutive,
            pending_uploads,
            voice_announcement,
        ),
    )
    .await
}

#[cfg(unix)]
pub(super) fn recapture_inflight_offset_after_successful_busy_wait(
    output_path: Option<&str>,
    previous_offset: u64,
) -> u64 {
    output_path
        .and_then(|path| std::fs::metadata(path).ok())
        .map(|metadata| metadata.len())
        .unwrap_or(previous_offset)
}

// #3813 Phase 3 (AC#7 "readiness-wait status rendering"): the compact status a
// hosted-TUI busy preflight readiness wait surfaces on the intake placeholder is
// non-empty, user-facing ("준비 대기"), and identical across every detection
// variant (the internal strategy difference is meaningless to the user).
#[cfg(all(test, unix))]
mod readiness_wait_status_tests {
    use super::HOSTED_TUI_READINESS_WAIT_NOTICE;
    use super::HostedTuiBusyPreflightReadinessWait;
    use super::readiness_wait_compact_status;
    use std::path::PathBuf;

    #[test]
    fn every_variant_renders_the_nonempty_readiness_label() {
        for wait in [
            HostedTuiBusyPreflightReadinessWait::Codex,
            HostedTuiBusyPreflightReadinessWait::ClaudePromptMarkerOnly,
            HostedTuiBusyPreflightReadinessWait::ClaudePromptMarkerOrIdleTranscript(PathBuf::from(
                "/tmp/transcript.jsonl",
            )),
        ] {
            let status = readiness_wait_compact_status(&wait);
            assert!(!status.is_empty(), "readiness status must be non-empty");
            assert!(
                status.contains("준비 대기"),
                "readiness status must surface the waiting state: {status}"
            );
            assert_eq!(status, HOSTED_TUI_READINESS_WAIT_NOTICE);
        }
    }
}

/// #4139: the enqueue-refusal branch restores the taken recovery context and
/// rewrites the placeholder into the refusal notice. Lives here (non-baselined
/// sibling) so the baselined intake root carries only the call.
pub(super) async fn apply_tui_busy_enqueue_refusal(
    shared: &Arc<SharedData>,
    http: &Arc<serenity::http::Http>,
    channel_id: ChannelId,
    placeholder_msg_id: MessageId,
    session_retry_context: Option<
        &crate::services::discord::router::turn_start::FormattedSessionRetryContext,
    >,
    refusal_reason: Option<crate::services::turn_orchestrator::EnqueueRefusalReason>,
) {
    put_back_session_retry_context(
        shared,
        channel_id,
        session_retry_context,
        refusal_reason.map(|reason| reason.as_str()),
    );
    let notice = claude_tui_busy_followup_refusal_notice(refusal_reason);
    let _ = super::super::super::http::edit_channel_message(
        http,
        channel_id,
        placeholder_msg_id,
        notice,
    )
    .await;
}
