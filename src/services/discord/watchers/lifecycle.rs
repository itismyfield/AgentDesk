use super::*;

#[path = "lifecycle/activity.rs"]
mod activity;
pub(super) use self::activity::maybe_refresh_watcher_activity_heartbeat;
#[allow(unused_imports)]
pub(in crate::services::discord) use self::activity::{
    HeartbeatRefreshMatch, HeartbeatRefreshOutcome, refresh_session_heartbeat_from_tmux_output,
    refresh_session_heartbeat_from_tmux_output_detailed, touch_session_activity,
};

#[path = "codex_tui_restore.rs"]
mod codex_restore;

#[derive(Debug, PartialEq, Eq)]
pub(super) enum LivenessProbeOutcome {
    /// No dead marker observed; the tmux pane liveness check answered.
    PaneCheckOnly { alive: bool },
    /// Both the dead marker and the live pane exist — the marker is stale
    /// (e.g. a prior wrapper recorded its own death but the session has
    /// been respawned, or `POST /api/inflight/rebind` adopted an
    /// externally-owned tmux session whose previous watcher marked the
    /// pane dead). Callers should remove the marker and treat the session
    /// as live; otherwise the watcher short-circuits to dead in its first
    /// poll and defeats the rebind forward-only relay contract.
    StaleMarkerClearAndAlive,
    /// Dead marker present and the pane really is gone — honour the marker.
    MarkerHonoredDead,
}

/// #2853 — for claude_tui sessions whose AgentDesk-side relay JSONL never lands
/// on disk (claude TUI writes its rollout to `~/.claude/projects/<cwd>/<uuid>.jsonl`),
/// fall back to the freshest Claude rollout transcript under the launched
/// session cwd; otherwise restart recovery hits the `no output file` branch and
/// never re-attaches a watcher to a live claude_tui pane. The claude_tui
/// inflight has `session_id = None` (#2843), so the rollout is resolved by
/// cwd + freshest-transcript, honoring #2843's anti-stealing constraints
/// (tmux launch-script mtime floor; exclude transcripts claimed by other live
/// Claude TUI sessions).
fn claude_tui_transcript_fallback_path(
    provider: &crate::services::provider::ProviderKind,
    tmux_session_name: &str,
    workspace: Option<&str>,
    restored_cwd: Option<&str>,
    shared: &Arc<SharedData>,
    claude_home: Option<&std::path::Path>,
    restore_claimed_transcripts: &std::collections::HashSet<std::path::PathBuf>,
) -> Option<String> {
    if *provider != crate::services::provider::ProviderKind::Claude {
        return None;
    }
    let scan_context = claude_tui_restore_scan_context(tmux_session_name, restored_cwd, workspace)?;
    let mut claimed_by_other_sessions =
        super::super::tui_prompt_relay::other_session_claimed_transcripts(
            shared,
            tmux_session_name,
        );
    claimed_by_other_sessions.extend(restore_claimed_transcripts.iter().cloned());
    claude_tui_transcript_fallback_path_for_context(
        provider,
        &scan_context.cwd,
        scan_context.modified_since,
        claude_home,
        &claimed_by_other_sessions,
    )
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ClaudeTuiRestoreScanContext {
    cwd: std::path::PathBuf,
    modified_since: std::time::SystemTime,
}

fn claude_tui_restore_scan_context(
    tmux_session_name: &str,
    restored_cwd: Option<&str>,
    workspace: Option<&str>,
) -> Option<ClaudeTuiRestoreScanContext> {
    let launch_context =
        super::super::tui_prompt_relay::claude_tui_launch_context(tmux_session_name);
    let fallback_modified_since = if launch_context.is_none() {
        claude_tui_restore_fallback_modified_since(tmux_session_name)
    } else {
        None
    };
    select_claude_tui_restore_scan_context(
        launch_context,
        fallback_modified_since,
        restored_cwd,
        workspace,
    )
}

fn select_claude_tui_restore_scan_context(
    launch_context: Option<(std::path::PathBuf, std::time::SystemTime)>,
    fallback_modified_since: Option<std::time::SystemTime>,
    restored_cwd: Option<&str>,
    workspace: Option<&str>,
) -> Option<ClaudeTuiRestoreScanContext> {
    if let Some((launch_cwd, launch_mtime)) = launch_context {
        let cwd = select_claude_tui_restore_scan_cwd(Some(launch_cwd), restored_cwd, workspace)?;
        return Some(ClaudeTuiRestoreScanContext {
            cwd,
            modified_since: launch_mtime,
        });
    }
    let cwd = select_claude_tui_restore_scan_cwd(None, restored_cwd, workspace)?;
    Some(ClaudeTuiRestoreScanContext {
        cwd,
        modified_since: fallback_modified_since?,
    })
}

fn claude_tui_restore_fallback_modified_since(
    tmux_session_name: &str,
) -> Option<std::time::SystemTime> {
    [
        crate::services::tmux_common::CLAUDE_TUI_LAUNCH_SCRIPT_TEMP_EXT,
        "generation",
        crate::services::tmux_common::TMUX_RUNTIME_KIND_TEMP_EXT,
    ]
    .into_iter()
    .filter_map(|ext| {
        crate::services::tmux_common::resolve_session_temp_path(tmux_session_name, ext)
    })
    .filter_map(|path| {
        std::fs::metadata(path)
            .and_then(|metadata| metadata.modified())
            .ok()
    })
    .next()
}

fn select_claude_tui_restore_scan_cwd(
    launch_cwd: Option<std::path::PathBuf>,
    restored_cwd: Option<&str>,
    workspace: Option<&str>,
) -> Option<std::path::PathBuf> {
    launch_cwd
        .filter(|path| !path.as_os_str().is_empty())
        .or_else(|| {
            restored_cwd
                .map(str::trim)
                .filter(|path| !path.is_empty())
                .map(std::path::PathBuf::from)
        })
        .or_else(|| {
            workspace
                .map(str::trim)
                .filter(|path| !path.is_empty())
                .map(std::path::PathBuf::from)
        })
}

fn claude_tui_transcript_fallback_path_for_context(
    provider: &crate::services::provider::ProviderKind,
    cwd: &std::path::Path,
    launch_mtime: std::time::SystemTime,
    claude_home: Option<&std::path::Path>,
    exclude: &std::collections::HashSet<std::path::PathBuf>,
) -> Option<String> {
    if *provider != crate::services::provider::ProviderKind::Claude {
        return None;
    }
    let transcript =
        crate::services::claude_tui::transcript_tail::latest_claude_transcript_for_cwd(
            cwd,
            launch_mtime,
            claude_home,
            exclude,
        )?;
    Some(transcript.display().to_string())
}

#[cfg(test)]
mod claude_tui_transcript_fallback_tests {
    use crate::services::provider::ProviderKind;
    use std::collections::HashSet;
    use std::path::{Path, PathBuf};
    use std::time::{Duration, SystemTime};

    fn write_transcript(home: &Path, cwd: &Path, session_id: &str, body: &[u8]) -> PathBuf {
        let transcript = crate::services::claude_tui::transcript_tail::claude_transcript_path(
            cwd,
            session_id,
            Some(home),
        )
        .unwrap();
        std::fs::create_dir_all(transcript.parent().unwrap()).unwrap();
        std::fs::write(&transcript, body).unwrap();
        transcript
    }

    #[test]
    fn resolves_freshest_claude_transcript_when_wrapper_jsonl_absent() {
        let home = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let session_id = "11111111-1111-4111-8111-111111111111";
        let transcript = write_transcript(
            home.path(),
            cwd.path(),
            session_id,
            b"{\"type\":\"assistant\"}\n",
        );

        let resolved = super::claude_tui_transcript_fallback_path_for_context(
            &ProviderKind::Claude,
            cwd.path(),
            SystemTime::UNIX_EPOCH,
            Some(home.path()),
            &HashSet::new(),
        );
        assert_eq!(
            resolved.as_deref(),
            transcript.to_str(),
            "claude_tui fallback must recover onto the live rollout transcript"
        );
    }

    #[test]
    fn returns_none_for_non_claude_provider() {
        let home = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        assert!(
            super::claude_tui_transcript_fallback_path_for_context(
                &ProviderKind::Codex,
                cwd.path(),
                SystemTime::UNIX_EPOCH,
                Some(home.path()),
                &HashSet::new(),
            )
            .is_none(),
            "codex uses its own rollout fallback, not the claude transcript path"
        );
    }

    #[test]
    fn returns_none_without_transcript() {
        let home = tempfile::tempdir().unwrap();
        let empty_cwd = tempfile::tempdir().unwrap();
        assert!(
            super::claude_tui_transcript_fallback_path_for_context(
                &ProviderKind::Claude,
                empty_cwd.path(),
                SystemTime::UNIX_EPOCH,
                Some(home.path()),
                &HashSet::new(),
            )
            .is_none()
        );
    }

    #[test]
    fn excludes_transcripts_claimed_by_other_shared_workspace_sessions() {
        let home = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let own_transcript = write_transcript(
            home.path(),
            cwd.path(),
            "22222222-2222-4222-8222-222222222222",
            b"{\"type\":\"assistant\",\"session\":\"own\"}\n",
        );
        std::thread::sleep(Duration::from_millis(20));
        let other_transcript = write_transcript(
            home.path(),
            cwd.path(),
            "33333333-3333-4333-8333-333333333333",
            b"{\"type\":\"assistant\",\"session\":\"other\"}\n",
        );
        let exclude = HashSet::from([other_transcript]);

        let resolved = super::claude_tui_transcript_fallback_path_for_context(
            &ProviderKind::Claude,
            cwd.path(),
            SystemTime::UNIX_EPOCH,
            Some(home.path()),
            &exclude,
        );

        assert_eq!(
            resolved.as_deref(),
            own_transcript.to_str(),
            "shared-workspace restore must not steal another live session transcript"
        );
    }

    #[test]
    fn applies_launch_time_floor_to_skip_prior_session_transcripts() {
        let home = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        write_transcript(
            home.path(),
            cwd.path(),
            "44444444-4444-4444-8444-444444444444",
            b"{\"type\":\"assistant\",\"session\":\"old\"}\n",
        );
        std::thread::sleep(Duration::from_millis(20));
        let launch_mtime = SystemTime::now();
        std::thread::sleep(Duration::from_millis(20));
        let current_transcript = write_transcript(
            home.path(),
            cwd.path(),
            "55555555-5555-4555-8555-555555555555",
            b"{\"type\":\"assistant\",\"session\":\"current\"}\n",
        );

        let resolved = super::claude_tui_transcript_fallback_path_for_context(
            &ProviderKind::Claude,
            cwd.path(),
            launch_mtime,
            Some(home.path()),
            &HashSet::new(),
        );

        assert_eq!(
            resolved.as_deref(),
            current_transcript.to_str(),
            "restart restore must ignore transcripts older than the tmux launch"
        );
    }

    #[test]
    fn restore_allocation_excludes_already_selected_rotated_transcript_for_same_cwd_sessions() {
        let home = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let first_rotated = write_transcript(
            home.path(),
            cwd.path(),
            "66666666-6666-4666-8666-666666666666",
            b"{\"type\":\"assistant\",\"session\":\"rotated-a\"}\n",
        );
        std::thread::sleep(Duration::from_millis(20));
        let second_rotated = write_transcript(
            home.path(),
            cwd.path(),
            "77777777-7777-4777-8777-777777777777",
            b"{\"type\":\"assistant\",\"session\":\"rotated-b\"}\n",
        );
        let launch_mtime = SystemTime::UNIX_EPOCH;
        let mut restore_claims = HashSet::new();

        let first = super::claude_tui_transcript_fallback_path_for_context(
            &ProviderKind::Claude,
            cwd.path(),
            launch_mtime,
            Some(home.path()),
            &restore_claims,
        )
        .expect("first restore selection");
        restore_claims.insert(PathBuf::from(&first));
        let second = super::claude_tui_transcript_fallback_path_for_context(
            &ProviderKind::Claude,
            cwd.path(),
            launch_mtime,
            Some(home.path()),
            &restore_claims,
        )
        .expect("second restore selection");

        assert_eq!(first, second_rotated.to_string_lossy());
        assert_eq!(second, first_rotated.to_string_lossy());
        assert_ne!(
            first, second,
            "same restore scan must allocate distinct rotated transcripts"
        );
    }

    #[test]
    fn missing_launch_context_uses_restored_cwd_with_marker_mtime_floor() {
        let home = tempfile::tempdir().unwrap();
        let restored_cwd = tempfile::tempdir().unwrap();
        let configured_cwd = tempfile::tempdir().unwrap();
        write_transcript(
            home.path(),
            restored_cwd.path(),
            "88888888-8888-4888-8888-888888888888",
            b"{\"type\":\"assistant\",\"session\":\"old\"}\n",
        );
        std::thread::sleep(Duration::from_millis(20));
        let marker_mtime = SystemTime::now();
        std::thread::sleep(Duration::from_millis(20));
        let current_transcript = write_transcript(
            home.path(),
            restored_cwd.path(),
            "99999999-9999-4999-8999-999999999999",
            b"{\"type\":\"assistant\",\"session\":\"current\"}\n",
        );
        let configured_transcript = write_transcript(
            home.path(),
            configured_cwd.path(),
            "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa",
            b"{\"type\":\"assistant\",\"session\":\"configured\"}\n",
        );

        let context = super::select_claude_tui_restore_scan_context(
            None,
            Some(marker_mtime),
            Some(restored_cwd.path().to_str().unwrap()),
            Some(configured_cwd.path().to_str().unwrap()),
        )
        .expect("missing launch script should still scan with marker floor");

        assert_eq!(context.cwd, restored_cwd.path());
        assert_eq!(context.modified_since, marker_mtime);
        let resolved = super::claude_tui_transcript_fallback_path_for_context(
            &ProviderKind::Claude,
            &context.cwd,
            context.modified_since,
            Some(home.path()),
            &HashSet::new(),
        );

        assert_eq!(resolved.as_deref(), current_transcript.to_str());
        assert_ne!(resolved.as_deref(), configured_transcript.to_str());
    }

    #[test]
    fn restore_scan_cwd_prefers_actual_launch_worktree_then_db_then_configured_workspace() {
        let configured = tempfile::tempdir().unwrap();
        let db_worktree = tempfile::tempdir().unwrap();
        let launch_worktree = tempfile::tempdir().unwrap();

        assert_eq!(
            super::select_claude_tui_restore_scan_cwd(
                Some(launch_worktree.path().to_path_buf()),
                Some(db_worktree.path().to_str().unwrap()),
                Some(configured.path().to_str().unwrap()),
            )
            .as_deref(),
            Some(launch_worktree.path())
        );
        assert_eq!(
            super::select_claude_tui_restore_scan_cwd(
                None,
                Some(db_worktree.path().to_str().unwrap()),
                Some(configured.path().to_str().unwrap()),
            )
            .as_deref(),
            Some(db_worktree.path())
        );
        assert_eq!(
            super::select_claude_tui_restore_scan_cwd(
                None,
                Some("   "),
                Some(configured.path().to_str().unwrap()),
            )
            .as_deref(),
            Some(configured.path())
        );
    }
}

pub(super) fn evaluate_liveness_probe(
    marker_present: bool,
    pane_alive: bool,
) -> LivenessProbeOutcome {
    match (marker_present, pane_alive) {
        (true, true) => LivenessProbeOutcome::StaleMarkerClearAndAlive,
        (true, false) => LivenessProbeOutcome::MarkerHonoredDead,
        (false, alive) => LivenessProbeOutcome::PaneCheckOnly { alive },
    }
}

pub(super) async fn probe_tmux_session_liveness(tmux_session_name: &str) -> bool {
    let marker_path = crate::services::tmux_common::session_dead_marker_path(tmux_session_name);
    let marker_present = std::path::Path::new(&marker_path).exists();

    let pane_alive = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        tokio::task::spawn_blocking({
            let name = tmux_session_name.to_string();
            move || tmux_session_has_live_pane(&name)
        }),
    )
    .await
    .unwrap_or(Ok(false))
    .unwrap_or(false);

    match evaluate_liveness_probe(marker_present, pane_alive) {
        LivenessProbeOutcome::StaleMarkerClearAndAlive => {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] 🧹 clearing stale .pane_dead marker for {tmux_session_name} — tmux session is alive"
            );
            let _ = std::fs::remove_file(&marker_path);
            true
        }
        LivenessProbeOutcome::MarkerHonoredDead => false,
        LivenessProbeOutcome::PaneCheckOnly { alive } => alive,
    }
}

pub(super) fn watcher_output_file_offset(output_path: &str) -> Option<u64> {
    std::fs::metadata(output_path).ok().map(|meta| meta.len())
}

pub(super) fn cancel_suppression_applies_to_watcher_death(
    cancel_induced_candidate: bool,
    terminal_delivery_observed: bool,
) -> bool {
    cancel_induced_candidate && !terminal_delivery_observed
}

/// #3898 — whether a watcher-observed tmux death should attempt the
/// resume-aborted restart handoff (`resume_aborted_restart_turn`). This is the
/// ONLY user-facing lifecycle signal for a genuinely abnormal mid-turn pane
/// crash: the turn was aborted *before* terminal delivery and the pane did not
/// exit through a normal-completion path (`turn completed` / `exit:0` /
/// `routine fresh`).
///
/// The legacy "session ended: tmux pane exited. Send a new message to start a
/// new session." Discord notice (removed in #3898) is intentionally NOT
/// reinstated here. It was both noise and factually wrong:
/// - It required `terminal_delivery_observed`, so it never covered a genuine
///   mid-turn crash — that case routes to the restart handoff below, not to a
///   notice. The only deaths it actually fired on were delivered-then-idle /
///   cleanup / force-kill teardowns that left no normal-completion marker,
///   i.e. normal idle exits (false positive).
/// - A pane death does NOT start a fresh session: the DB `claude_session_id`
///   persists and the next message resumes the conversation with `--resume`,
///   so "start a new session" was incorrect for every death that reached it.
pub(super) fn tmux_death_should_attempt_restart_handoff(
    cancel_induced: bool,
    prompt_too_long_killed: bool,
    terminal_delivery_observed: bool,
    is_normal_completion: bool,
) -> bool {
    !cancel_induced
        && !prompt_too_long_killed
        && !terminal_delivery_observed
        && !is_normal_completion
}

pub(super) async fn handle_tmux_watcher_observed_death(
    channel_id: ChannelId,
    http: &Arc<serenity::Http>,
    shared: &Arc<SharedData>,
    tmux_session_name: &str,
    output_path: &str,
    _watcher_provider: &ProviderKind,
    prompt_too_long_killed: bool,
    terminal_delivery_observed: bool,
) {
    let ts = chrono::Local::now().format("%H:%M:%S");
    let diagnostic = build_tmux_death_diagnostic(tmux_session_name, Some(output_path));
    if let Some(diag) = diagnostic.as_deref() {
        tracing::info!(
            "  [{ts}] 👁 tmux session {tmux_session_name} ended, watcher stopping ({diag})"
        );
    } else {
        tracing::info!("  [{ts}] 👁 tmux session {tmux_session_name} ended, watcher stopping");
    }
    let reason_short = read_tmux_exit_reason(tmux_session_name);
    let is_normal_completion =
        tmux_death_is_normal_completion(reason_short.as_deref(), diagnostic.as_deref());
    // The watcher cleanup path that follows an explicit cancel (user removed
    // the activity reaction or invoked /stop) writes
    // `record_tmux_exit_reason("watcher cleanup: dead session after turn")`
    // and tears the session down. Without this gate that synthetic reason
    // surfaces as a 🔴 lifecycle notification AND as the "대화를 이어붙이지
    // 못했습니다" handoff — both of which are noise for a user who just
    // canceled the turn themselves. The same suppression applies to the
    // immediate-respawn watcher death that can fire seconds later when the
    // next message arrives, since both are direct consequences of the cancel.
    //
    // For provider-native TUI relays the active watcher may be tailing a
    // rollout/transcript path rather than the legacy tmux-wrapper jsonl. Use
    // this watcher instance's actual output path for the EOF boundary check.
    // Also, once this watcher has observed terminal delivery for a fresh turn,
    // a later pane death is a lifecycle event for that turn, not the previous
    // reset/cancel cleanup. This signal is intentionally broader than the
    // watcher-local `turn_result_relayed` flag: session-bound StreamRelay can
    // deliver the Discord response before the watcher finishes its later
    // inflight/mailbox cleanup block.
    let death_output_offset = watcher_output_file_offset(output_path);
    let cancel_induced_candidate = cancel_induced_watcher_death_async(
        channel_id,
        tmux_session_name,
        death_output_offset,
        shared.pg_pool.as_ref(),
    )
    .await;
    let cancel_induced = cancel_suppression_applies_to_watcher_death(
        cancel_induced_candidate,
        terminal_delivery_observed,
    );
    // #3898 — the legacy "session ended … start a new session" Discord notice was
    // removed. It false-fired on normal idle / cleanup / force-kill teardown
    // (no `turn completed` / `exit:0` / `routine fresh` marker → classified
    // abnormal) and was factually wrong (a pane death resumes via `--resume`, it
    // does not start a fresh session). The genuine mid-turn crash signal is the
    // restart handoff below; cancel suppression is still computed because it
    // gates that handoff. `is_normal_completion` already folds in the exit-reason
    // normal-completion check (`tmux_death_is_normal_completion`), so it is the
    // single source of truth for the restart-handoff suppression.
    let attempt_restart_handoff = tmux_death_should_attempt_restart_handoff(
        cancel_induced,
        prompt_too_long_killed,
        terminal_delivery_observed,
        is_normal_completion,
    );
    if cancel_induced {
        tracing::info!(
            "  [{ts}] 👁 tmux session {tmux_session_name} ended after recent cancel/turn-stop, skipping lifecycle notification + restart handoff"
        );
    } else if cancel_induced_candidate {
        tracing::info!(
            "  [{ts}] 👁 tmux session {tmux_session_name} ended after a relayed terminal turn; ignoring stale cancel/turn-stop suppression"
        );
    } else if !is_normal_completion {
        tracing::info!(
            "  [{ts}] 👁 tmux session {tmux_session_name} ended without normal completion, skipping Discord lifecycle notification"
        );
    } else {
        tracing::info!(
            "  [{ts}] 👁 tmux session {tmux_session_name} ended after normal completion, skipping lifecycle notification"
        );
    }
    if attempt_restart_handoff {
        let _ =
            resume_aborted_restart_turn(channel_id, http, shared, tmux_session_name, output_path)
                .await;
    }
}

pub(super) fn extract_result_error_text(value: &serde_json::Value) -> String {
    let errors = value
        .get("errors")
        .and_then(|v| v.as_array())
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str().map(str::trim))
                .filter(|item| !item.is_empty())
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default();

    if !errors.trim().is_empty() {
        errors
    } else {
        value
            .get("result")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string()
    }
}

/// Resolve a restored session's persisted cwd (worktree) from the `sessions`
/// table, scoped to the unique Discord `channel_id`.
///
/// #3207 (part 2) P0-b: `session_key` derives from the sanitized/truncated
/// channel NAME, so name-colliding channels would resolve EACH OTHER's
/// persisted cwd straight into the restored runtime state. The
/// `channel_id = $2` predicate is the cross-channel guard; legacy NULL
/// `channel_id` rows are intentionally NOT reused (that is exactly the hazard
/// being closed — reuse self-heals on the next turn once the row is stamped).
pub(super) fn load_restored_session_cwd(
    pg_pool: Option<&sqlx::PgPool>,
    session_keys: &[String],
    channel_id: u64,
) -> Option<String> {
    if let Some(pg_pool) = pg_pool {
        let session_keys = session_keys.to_vec();
        let channel_id = channel_id.to_string();
        return crate::utils::async_bridge::block_on_pg_result(
            pg_pool,
            move |pool| async move {
                for session_key in session_keys {
                    let path = sqlx::query_scalar::<_, String>(
                        "SELECT cwd FROM sessions \
                         WHERE session_key = $1 AND channel_id = $2 LIMIT 1",
                    )
                    .bind(&session_key)
                    .bind(&channel_id)
                    .fetch_optional(&pool)
                    .await
                    .map_err(|error| format!("load tmux restore cwd {session_key}: {error}"))?;
                    if let Some(path) =
                        path.filter(|path| !path.is_empty() && std::path::Path::new(path).is_dir())
                    {
                        return Ok(Some(path));
                    }
                }
                Ok(None)
            },
            |message| message,
        )
        .ok()
        .flatten();
    }

    let _ = (session_keys, channel_id);
    None
}

pub(super) fn push_transcript_event(
    events: &mut Vec<SessionTranscriptEvent>,
    event: SessionTranscriptEvent,
) {
    let has_payload = !event.content.trim().is_empty()
        || event
            .summary
            .as_deref()
            .is_some_and(|value| !value.trim().is_empty())
        || event
            .tool_name
            .as_deref()
            .is_some_and(|value| !value.trim().is_empty());
    if has_payload
        || matches!(
            event.kind,
            SessionTranscriptEventKind::Thinking
                | SessionTranscriptEventKind::Result
                | SessionTranscriptEventKind::Error
                | SessionTranscriptEventKind::Task
                | SessionTranscriptEventKind::System
        )
    {
        events.push(event);
    }
}

pub(super) const REDACTED_THINKING_STATUS_LINE: &str = "💭 Thinking...";

pub(super) fn redacted_thinking_transcript_event() -> SessionTranscriptEvent {
    SessionTranscriptEvent {
        kind: SessionTranscriptEventKind::Thinking,
        tool_name: None,
        summary: None,
        content: String::new(),
        status: Some("info".to_string()),
        is_error: false,
    }
}

pub(super) fn inflight_duration_ms(started_at: Option<&str>) -> Option<i64> {
    let started_at = started_at?.trim();
    if started_at.is_empty() {
        return None;
    }
    let parsed = chrono::NaiveDateTime::parse_from_str(started_at, "%Y-%m-%d %H:%M:%S").ok()?;
    let elapsed = chrono::Local::now().naive_local() - parsed;
    Some(elapsed.num_milliseconds().max(0))
}

pub(super) fn load_restored_provider_session_id(
    pg_pool: Option<&sqlx::PgPool>,
    token_hash: &str,
    provider: &ProviderKind,
    channel_name: &str,
) -> Option<String> {
    let tmux_name = provider.build_tmux_session_name(channel_name);
    let session_keys =
        super::super::adk_session::build_session_key_candidates(token_hash, provider, &tmux_name);

    if let Some(pg_pool) = pg_pool {
        let session_keys = session_keys.clone();
        let provider_name = provider.as_str().to_string();
        return crate::utils::async_bridge::block_on_pg_result(
            pg_pool,
            move |pool| async move {
                for session_key in session_keys {
                    let session_id = sqlx::query_scalar::<_, Option<String>>(
                        "SELECT claude_session_id
                         FROM sessions
                         WHERE session_key = $1 AND provider = $2
                         LIMIT 1",
                    )
                    .bind(&session_key)
                    .bind(&provider_name)
                    .fetch_optional(&pool)
                    .await
                    .map_err(|error| format!("load tmux provider session {session_key}: {error}"))?
                    .flatten();
                    if let Some(session_id) = session_id.filter(|session_id| !session_id.is_empty())
                    {
                        return Ok(Some(session_id));
                    }
                }
                Ok(None)
            },
            |message| message,
        )
        .ok()
        .flatten();
    }

    let _ = session_keys;
    None
}

pub(super) fn recovery_handled_channel_key(channel_id: u64) -> String {
    format!("recovery_handled_channel:{channel_id}")
}

pub(super) fn watcher_has_post_work_ready_evidence(
    full_response: &str,
    tool_state: &WatcherToolState,
    _task_notification_kind: Option<TaskNotificationKind>,
) -> bool {
    !full_response.trim().is_empty() || tool_state.any_tool_used
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::discord::tmux_watcher_now_ms;
    use poise::serenity_prelude::ChannelId;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};

    fn test_watcher_handle(tmux_session_name: &str, output_path: &str) -> TmuxWatcherHandle {
        TmuxWatcherHandle {
            tmux_session_name: tmux_session_name.to_string(),
            output_path: output_path.to_string(),
            paused: Arc::new(AtomicBool::new(false)),
            resume_offset: Arc::new(std::sync::Mutex::new(None)),
            cancel: Arc::new(AtomicBool::new(false)),
            pause_epoch: Arc::new(AtomicU64::new(0)),
            turn_delivered: Arc::new(AtomicBool::new(false)),
            last_heartbeat_ts_ms: Arc::new(AtomicI64::new(tmux_watcher_now_ms())),
        }
    }

    // #3898 — cancel suppression is the surviving gate that protects the restart
    // handoff (and previously gated the now-removed "session ended" notice).
    #[test]
    fn cancel_suppression_only_applies_before_terminal_delivery() {
        // A cancel/turn-stop candidate that died before delivering its turn is
        // suppressed (no restart handoff; historically no notice either).
        assert!(cancel_suppression_applies_to_watcher_death(true, false));
        // Once a terminal turn was delivered, a later pane death is a real
        // lifecycle event for that turn — stale cancel suppression must NOT apply.
        assert!(!cancel_suppression_applies_to_watcher_death(true, true));
        // No cancel candidate → never suppressed.
        assert!(!cancel_suppression_applies_to_watcher_death(false, false));
        assert!(!cancel_suppression_applies_to_watcher_death(false, true));
    }

    // #3898 — the false-positive fix. A tmux pane that exits *after* delivering
    // its turn (normal idle / cleanup / force-kill, which leave no
    // normal-completion marker → `is_normal_completion == false`) must NOT
    // surface any lifecycle signal: the removed notice never fires, and the
    // restart handoff is gated off by `terminal_delivery_observed`. This is the
    // noise the issue reported — a normal idle exit emitting a spurious notice.
    #[test]
    fn delivered_then_idle_death_surfaces_no_lifecycle_signal() {
        let cancel_induced = cancel_suppression_applies_to_watcher_death(false, true);
        assert!(!cancel_induced);
        // Even with an "abnormal" exit reason (no normal-completion marker), a
        // delivered turn means this is a normal idle/cleanup/force-kill teardown.
        assert!(!tmux_death_should_attempt_restart_handoff(
            cancel_induced,
            /* prompt_too_long_killed */ false,
            /* terminal_delivery_observed */ true,
            /* is_normal_completion */ false,
        ));
        // The same holds when the pane DID exit through a normal-completion path.
        assert!(!tmux_death_should_attempt_restart_handoff(
            cancel_induced,
            false,
            true,
            true,
        ));
    }

    // #3898 — a genuinely abnormal mid-turn pane crash (turn aborted BEFORE
    // terminal delivery, no normal-completion marker) STILL surfaces a signal:
    // the resume-aborted restart handoff fires. This is the case the removed
    // notice never covered (it required `terminal_delivery_observed`), so
    // removing the notice does not lose any genuine-crash signal.
    #[test]
    fn genuine_mid_turn_crash_triggers_restart_handoff() {
        assert!(tmux_death_should_attempt_restart_handoff(
            /* cancel_induced */ false, /* prompt_too_long_killed */ false,
            /* terminal_delivery_observed */ false, /* is_normal_completion */ false,
        ));
        // Suppressed when the user canceled the turn themselves …
        assert!(!tmux_death_should_attempt_restart_handoff(
            true, false, false, false
        ));
        // … when the prompt-too-long teardown already handled it …
        assert!(!tmux_death_should_attempt_restart_handoff(
            false, true, false, false
        ));
        // … or when the pane exited through a normal-completion path.
        assert!(!tmux_death_should_attempt_restart_handoff(
            false, false, false, true
        ));
    }

    #[test]
    fn same_tmux_different_output_path_replaces_watcher() {
        let watchers = TmuxWatcherRegistry::new();
        let channel_a = ChannelId::new(1485506232256168134);
        let channel_b = ChannelId::new(1485506232256168135);
        let tmux_name = "AgentDesk-codex-adk-cdx-path-change";

        let initial = test_watcher_handle(tmux_name, "/tmp/prelaunch-wrapper.jsonl");
        let initial_cancel = initial.cancel.clone();
        assert!(try_claim_watcher(&watchers, channel_a, initial));

        let outcome = claim_or_reuse_watcher(
            &watchers,
            channel_b,
            test_watcher_handle(tmux_name, "/tmp/provider-runtime.jsonl"),
            &ProviderKind::Codex,
            "unit-test-output-path-change",
        );

        assert_eq!(outcome.action, WatcherClaimAction::SpawnReplacedStale);
        assert_eq!(outcome.owner_channel_id(), channel_b);
        assert!(initial_cancel.load(Ordering::Relaxed));
        let watcher = watchers.get(&channel_b).expect("replacement watcher");
        assert_eq!(watcher.output_path, "/tmp/provider-runtime.jsonl");
        assert!(!watchers.contains_key(&channel_a));
    }

    /// #4455: a crossed-provider-turn Codex rebind must replace even a live
    /// same-session/same-output incumbent. Reuse would leave its stale
    /// `current_msg_id` render seed and converter generation in authority.
    #[test]
    fn forced_rebind_replaces_live_same_output_incumbent() {
        let watchers = TmuxWatcherRegistry::new();
        let owner = ChannelId::new(1_485_506_232_256_168_136);
        let dispatch = ChannelId::new(1_485_506_232_256_168_137);
        let tmux_name = "AgentDesk-codex-adk-cdx-4455-forced";
        let output = "/tmp/codex-4455-normalized.jsonl";
        let incumbent = test_watcher_handle(tmux_name, output);
        let incumbent_cancel = incumbent.cancel.clone();
        assert!(try_claim_watcher(&watchers, owner, incumbent));

        let outcome = claim_or_replace_watcher(
            &watchers,
            dispatch,
            test_watcher_handle(tmux_name, output),
            &ProviderKind::Codex,
            "recovery_restore_inflight_crossed_codex_turn",
        );

        assert_eq!(outcome.action, WatcherClaimAction::SpawnReplacedForced);
        assert!(outcome.should_spawn() && outcome.replaced_existing());
        assert_eq!(outcome.owner_channel_id(), dispatch);
        assert!(incumbent_cancel.load(Ordering::Relaxed));
        assert!(!watchers.contains_key(&owner));
        assert_eq!(
            watchers
                .get(&dispatch)
                .expect("forced replacement watcher")
                .output_path,
            output
        );
    }

    /// #3277 verify-2 truth table for the `recovery_restore_inflight` claim: a
    /// same-session incumbent is REPLACED only when it provably cannot own the
    /// relay — cancelled, heartbeat-stale (the Defect D hung-watcher subcase;
    /// `find_watcher_by_tmux_session` folds `heartbeat_stale()` into its
    /// replace predicate), paused, or bound to a different output path. A
    /// genuinely-live fresh-heartbeat unpaused same-output incumbent is REUSED
    /// untouched (never a duplicate-relay vector) and keeps the EXISTING owner
    /// channel (owner ≠ dispatch).
    #[test]
    fn recovery_restore_claim_replaces_dead_incumbent_only() {
        let tmux_name = "AgentDesk-claude-adk-cc-recovery-claim";
        let output = "/tmp/recovery-claim.jsonl";
        let owner = ChannelId::new(1_500_000_000_000_000_001);
        let dispatch = ChannelId::new(2_600_000_000_000_000_002);
        let claim = |incumbent: TmuxWatcherHandle, requested_output: &str| {
            let watchers = TmuxWatcherRegistry::new();
            assert!(try_claim_watcher(&watchers, owner, incumbent));
            claim_or_reuse_watcher(
                &watchers,
                dispatch,
                test_watcher_handle(tmux_name, requested_output),
                &ProviderKind::Claude,
                "recovery_restore_inflight",
            )
        };

        // Live fresh unpaused same-output → REUSED, owner channel preserved.
        let reused = claim(test_watcher_handle(tmux_name, output), output);
        assert_eq!(reused.as_str(), "reuse_existing");
        assert!(!reused.should_spawn());
        assert_eq!(reused.owner_channel_id(), owner);

        // Heartbeat-stale (NOT cancelled) → replaced: the Defect D hung watcher.
        let stale = test_watcher_handle(tmux_name, output);
        stale.last_heartbeat_ts_ms.store(1, Ordering::Release);
        let outcome = claim(stale, output);
        assert!(outcome.should_spawn() && outcome.replaced_existing());

        // Cancelled → replaced.
        let cancelled = test_watcher_handle(tmux_name, output);
        cancelled.cancel.store(true, Ordering::Relaxed);
        let outcome = claim(cancelled, output);
        assert!(outcome.should_spawn() && outcome.replaced_existing());

        // Paused (recovery source is not a turn-start) → replaced.
        let paused = test_watcher_handle(tmux_name, output);
        paused.paused.store(true, Ordering::Release);
        let outcome = claim(paused, output);
        assert!(outcome.should_spawn() && outcome.replaced_existing());

        // Different output path → replaced.
        let outcome = claim(
            test_watcher_handle(tmux_name, output),
            "/tmp/recovery-claim-other.jsonl",
        );
        assert!(outcome.should_spawn() && outcome.replaced_existing());
    }

    #[test]
    fn liveness_probe_clears_stale_marker_when_pane_alive() {
        assert_eq!(
            evaluate_liveness_probe(true, true),
            LivenessProbeOutcome::StaleMarkerClearAndAlive
        );
    }

    #[test]
    fn liveness_probe_honors_marker_when_pane_dead() {
        assert_eq!(
            evaluate_liveness_probe(true, false),
            LivenessProbeOutcome::MarkerHonoredDead
        );
    }

    #[test]
    fn liveness_probe_uses_pane_check_when_no_marker() {
        assert_eq!(
            evaluate_liveness_probe(false, true),
            LivenessProbeOutcome::PaneCheckOnly { alive: true }
        );
        assert_eq!(
            evaluate_liveness_probe(false, false),
            LivenessProbeOutcome::PaneCheckOnly { alive: false }
        );
    }

    #[test]
    fn restore_scan_only_skips_same_live_output_path() {
        assert!(restore_scan_should_skip_existing_watcher(
            false,
            false,
            "/tmp/wrapper.jsonl",
            "/tmp/wrapper.jsonl"
        ));
        assert!(!restore_scan_should_skip_existing_watcher(
            false,
            false,
            "/tmp/prelaunch.jsonl",
            "/tmp/restored.jsonl"
        ));
        assert!(!restore_scan_should_skip_existing_watcher(
            true,
            false,
            "/tmp/wrapper.jsonl",
            "/tmp/wrapper.jsonl"
        ));
        assert!(!restore_scan_should_skip_existing_watcher(
            false,
            true,
            "/tmp/wrapper.jsonl",
            "/tmp/wrapper.jsonl"
        ));
    }

    #[test]
    fn post_work_ready_evidence_ignores_task_notification_only_turns() {
        let tool_state = WatcherToolState::new();

        assert!(
            !watcher_has_post_work_ready_evidence(
                "",
                &tool_state,
                Some(TaskNotificationKind::Background),
            ),
            "a task notification alone can be older pane state and must not prove this turn finished"
        );
    }

    #[test]
    fn post_work_ready_evidence_accepts_response_or_tool_output() {
        let tool_state = WatcherToolState::new();
        assert!(watcher_has_post_work_ready_evidence(
            "done",
            &tool_state,
            None
        ));

        let mut tool_state = WatcherToolState::new();
        tool_state.any_tool_used = true;
        assert!(watcher_has_post_work_ready_evidence(
            "",
            &tool_state,
            Some(TaskNotificationKind::Subagent),
        ));
    }
}

pub(super) fn normalize_human_alert_target(channel: &str) -> Option<String> {
    let channel = channel.trim();
    if channel.is_empty() {
        return None;
    }
    Some(if channel.starts_with("channel:") {
        channel.to_string()
    } else {
        format!("channel:{channel}")
    })
}

pub(super) fn load_human_alert_target(shared: &SharedData) -> Option<String> {
    if let Some(pool) = shared.pg_pool.as_ref() {
        return crate::utils::async_bridge::block_on_pg_result(
            pool,
            |pool| async move {
                sqlx::query_scalar::<_, String>(
                    "SELECT value FROM kv_meta WHERE key = 'kanban_human_alert_channel_id'",
                )
                .fetch_optional(&pool)
                .await
                .map_err(|error| format!("load postgres human alert target: {error}"))
            },
            |message| message,
        )
        .ok()
        .flatten()
        .and_then(|channel| normalize_human_alert_target(&channel));
    }

    let _ = shared;
    None
}

pub(super) fn merge_card_label_metadata(existing_metadata: Option<&str>, label: &str) -> String {
    let mut metadata = existing_metadata
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(raw).ok())
        .and_then(|value| value.as_object().cloned())
        .unwrap_or_default();

    let mut labels = metadata
        .get("labels")
        .and_then(|value| value.as_str())
        .map(|value| {
            value
                .split(',')
                .map(str::trim)
                .filter(|item| !item.is_empty())
                .map(str::to_string)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    if !labels.iter().any(|existing| existing == label) {
        labels.push(label.to_string());
    }
    metadata.insert(
        "labels".to_string(),
        serde_json::Value::String(labels.join(",")),
    );

    serde_json::Value::Object(metadata).to_string()
}

pub(super) async fn update_card_ready_failure_marker_pg(
    pool: &sqlx::PgPool,
    card_id: &str,
    reason: &str,
) -> Result<bool, String> {
    let existing_metadata = sqlx::query_scalar::<_, Option<String>>(
        "SELECT metadata::text FROM kanban_cards WHERE id = $1",
    )
    .bind(card_id)
    .fetch_optional(pool)
    .await
    .map_err(|error| format!("load postgres card metadata for {card_id}: {error}"))?
    .flatten();
    let metadata_json =
        merge_card_label_metadata(existing_metadata.as_deref(), READY_FOR_INPUT_STUCK_LABEL);
    let updated = sqlx::query(
        "UPDATE kanban_cards
         SET metadata = $1::jsonb,
             blocked_reason = $2,
             updated_at = NOW()
         WHERE id = $3",
    )
    .bind(metadata_json)
    .bind(reason)
    .bind(card_id)
    .execute(pool)
    .await
    .map_err(|error| format!("update postgres ready marker for {card_id}: {error}"))?
    .rows_affected();
    Ok(updated > 0)
}

pub(super) fn load_dispatch_card_id(shared: &SharedData, dispatch_id: &str) -> Option<String> {
    if let Some(pool) = shared.pg_pool.as_ref() {
        let dispatch_id = dispatch_id.to_string();
        return crate::utils::async_bridge::block_on_pg_result(
            pool,
            move |pool| async move {
                sqlx::query_scalar::<_, String>(
                    "SELECT kanban_card_id FROM task_dispatches WHERE id = $1",
                )
                .bind(dispatch_id)
                .fetch_optional(&pool)
                .await
                .map_err(|error| format!("load postgres dispatch card id: {error}"))
            },
            |message| message,
        )
        .ok()
        .flatten();
    }

    let _ = (shared, dispatch_id);
    None
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(in crate::services::discord) struct ReadyForInputFailureResult {
    pub dispatch_failed: bool,
    pub card_id: Option<String>,
    pub card_marked: bool,
    pub human_alert_sent: bool,
}

pub(in crate::services::discord) async fn fail_dispatch_for_ready_for_input_stall(
    shared: &Arc<SharedData>,
    dispatch_id: &str,
    tmux_session_name: &str,
) -> Result<ReadyForInputFailureResult, String> {
    let payload = serde_json::json!({
        "reason": READY_FOR_INPUT_STUCK_REASON,
        "failure_kind": READY_FOR_INPUT_STUCK_LABEL,
        "tmux_session_name": tmux_session_name,
    });
    let changed = crate::dispatch::set_dispatch_status_with_backends(
        shared.pg_pool.as_ref(),
        dispatch_id,
        "failed",
        Some(&payload),
        "tmux_ready_for_input_stuck",
        Some(&["pending", "dispatched"]),
        false,
    )
    .map_err(|error| format!("mark dispatch {dispatch_id} failed for ready stall: {error}"))?;

    let card_id = load_dispatch_card_id(shared.as_ref(), dispatch_id);
    let mut card_marked = false;
    if let Some(card_id_ref) = card_id.as_deref() {
        card_marked = if let Some(pool) = shared.pg_pool.as_ref() {
            update_card_ready_failure_marker_pg(pool, card_id_ref, READY_FOR_INPUT_STUCK_REASON)
                .await?
        } else {
            false
        };
    }

    let human_alert_sent = if changed > 0 {
        load_human_alert_target(shared.as_ref()).is_some_and(|target| {
            let card_label = card_id.as_deref().unwrap_or("-");
            let content = format!(
                "자동큐 safety-net 발동: dispatch {dispatch_id} / card {card_label} / session {tmux_session_name} / {READY_FOR_INPUT_STUCK_REASON}"
            );
            enqueue_lifecycle_notification_best_effort(
                shared.pg_pool.as_ref(),
                &target,
                Some(dispatch_id),
                "dispatch.stuck_at_ready",
                &content,
            )
        })
    } else {
        false
    };

    Ok(ReadyForInputFailureResult {
        dispatch_failed: changed > 0,
        card_id,
        card_marked,
        human_alert_sent,
    })
}

pub(in crate::services::discord) fn recovery_handled_channel_exists(
    shared: &SharedData,
    channel_id: u64,
) -> bool {
    let key = recovery_handled_channel_key(channel_id);

    if let Ok(value) = super::super::internal_api::get_kv_value(&key) {
        return value.is_some();
    }

    if let Some(pg_pool) = shared.pg_pool.as_ref() {
        return crate::utils::async_bridge::block_on_pg_result(
            pg_pool,
            move |pool| async move {
                sqlx::query_scalar::<_, bool>(
                    "SELECT EXISTS(
                         SELECT 1
                         FROM kv_meta
                         WHERE key = $1
                           AND (expires_at IS NULL OR expires_at > NOW())
                     )",
                )
                .bind(&key)
                .fetch_one(&pool)
                .await
                .map_err(|error| format!("load recovery handled marker {key}: {error}"))
            },
            |message| message,
        )
        .unwrap_or(false);
    }

    let _ = (shared, key);
    false
}

pub(in crate::services::discord) async fn store_recovery_handled_channels(
    shared: &SharedData,
    channel_ids: &[u64],
) {
    if channel_ids.is_empty() {
        return;
    }

    let marker_value = chrono::Utc::now().timestamp().to_string();
    let mut stored_via_internal_api = true;
    for channel_id in channel_ids {
        let key = recovery_handled_channel_key(*channel_id);
        if let Err(error) = super::super::internal_api::set_kv_value(&key, &marker_value) {
            tracing::debug!(
                "recovery handled marker fallback for {key}: direct runtime API unavailable: {error}"
            );
            stored_via_internal_api = false;
            break;
        }
    }
    if stored_via_internal_api {
        return;
    }

    if let Some(pg_pool) = shared.pg_pool.as_ref() {
        match pg_pool.begin().await {
            Ok(mut tx) => {
                for channel_id in channel_ids {
                    let key = recovery_handled_channel_key(*channel_id);
                    if let Err(error) = sqlx::query(
                        "INSERT INTO kv_meta (key, value, expires_at)
                         VALUES ($1, $2, NULL)
                         ON CONFLICT (key) DO UPDATE
                         SET value = EXCLUDED.value,
                             expires_at = EXCLUDED.expires_at",
                    )
                    .bind(&key)
                    .bind(&marker_value)
                    .execute(&mut *tx)
                    .await
                    {
                        tracing::warn!(
                            "failed to persist recovery handled marker {key} in postgres: {error}"
                        );
                        return;
                    }
                }
                if let Err(error) = tx.commit().await {
                    tracing::warn!("failed to commit recovery handled marker tx: {error}");
                }
            }
            Err(error) => {
                tracing::warn!("failed to begin recovery handled marker tx: {error}");
            }
        }
        return;
    }

    let _ = shared;
}

pub(in crate::services::discord) async fn clear_recovery_handled_channels(shared: &SharedData) {
    if let Err(error) = super::super::internal_api::clear_kv_prefix("recovery_handled_channel:") {
        tracing::debug!(
            "recovery handled marker clear fallback: direct runtime API unavailable: {error}"
        );
    } else {
        return;
    }

    if let Some(pg_pool) = shared.pg_pool.as_ref() {
        if let Err(error) =
            sqlx::query("DELETE FROM kv_meta WHERE key LIKE 'recovery_handled_channel:%'")
                .execute(pg_pool)
                .await
        {
            tracing::warn!("failed to clear recovery handled markers in postgres: {error}");
        }
        return;
    }

    let _ = shared;
}

pub(super) async fn clear_provider_session_for_retry(
    shared: &Arc<SharedData>,
    channel_id: ChannelId,
    tmux_session_name: &str,
    fallback_session_id: Option<&str>,
) {
    let stale_sid = {
        let mut data = shared.core.lock().await;
        let old = data
            .sessions
            .get(&channel_id)
            .and_then(|session| session.session_id.clone())
            .or_else(|| fallback_session_id.map(ToString::to_string));
        if let Some(session) = data.sessions.get_mut(&channel_id) {
            session.clear_provider_session();
        }
        old
    };

    let session_key = format!(
        "{}:{}",
        crate::services::platform::hostname_short(),
        tmux_session_name
    );
    super::super::adk_session::clear_provider_session_id(&session_key, shared.api_port).await;

    if let Some(sid) = stale_sid {
        let _ = super::super::internal_api::clear_stale_session_id(&sid).await;
    }
}

pub(super) async fn resolve_watcher_dispatch_id(
    shared: &Arc<SharedData>,
    channel_id: ChannelId,
    inflight_state: Option<&super::super::inflight::InflightTurnState>,
) -> Option<String> {
    inflight_state
        .and_then(|state| state.dispatch_id.clone())
        .or_else(|| {
            inflight_state
                .and_then(|state| super::super::adk_session::parse_dispatch_id(&state.user_text))
        })
        .or(
            super::super::adk_session::lookup_pending_dispatch_for_thread(
                shared.api_port,
                channel_id.get(),
            )
            .await,
        )
        .or_else(|| {
            resolve_dispatched_thread_dispatch_from_db(shared.pg_pool.as_ref(), channel_id.get())
        })
}

pub(super) fn should_suppress_terminal_output_after_recent_stop(
    has_assistant_response: bool,
    inflight_missing: bool,
    recent_turn_stop: bool,
) -> bool {
    should_suppress_streaming_placeholder_after_recent_stop(
        has_assistant_response,
        inflight_missing,
        recent_turn_stop,
    )
}

pub(super) fn should_suppress_streaming_placeholder_after_recent_stop(
    has_assistant_response: bool,
    inflight_missing: bool,
    recent_turn_stop: bool,
) -> bool {
    has_assistant_response && inflight_missing && recent_turn_stop
}

pub(super) fn should_skip_streaming_placeholder_without_inflight(
    inflight_missing: bool,
    pane_actively_streaming: bool,
) -> bool {
    // #3107: a live agentic TUI turn can lose its inflight mid-turn (a momentary
    // idle observation between tool calls commits and clears it). When the pane
    // is still actively producing assistant output, the missing inflight is a
    // self-heal opportunity, NOT a signal to suppress — dropping the edit here
    // is exactly the relay-degradation bug. Only suppress when inflight is
    // missing AND the pane looks finished/idle (genuine post-finish ghost noise
    // like provider-selector chrome).
    inflight_missing && !pane_actively_streaming
}

#[allow(clippy::too_many_arguments)]
pub(super) fn should_suppress_post_terminal_output_without_inflight(
    terminal_success_seen: bool,
    inflight_missing: bool,
    ssh_direct_prompt_pending: bool,
    external_input_lease_present: bool,
    assistant_continuation_present: bool,
    pane_actively_streaming: bool,
    pending_synthetic_start_present: bool,
) -> bool {
    // SSH-direct prompts never create an inflight (they bypass the Discord
    // message path), so the (terminal + no-inflight) shape alone is not enough
    // to call new output "ghost noise" — a pending prompt anchor or
    // ExternalInput relay lease signals a legitimate user turn whose response
    // we must still relay even when notification/anchor creation failed.
    // Likewise, another assistant event after an early terminal relay means
    // the provider turn continued with tool calls or final text; do not drop it.
    // #3107: and if the pane is still actively producing assistant output, the
    // turn is live and merely lost its inflight — relay (and re-acquire) rather
    // than suppress.
    // #3154: while a deferred synthetic turn-start is pending for this channel,
    // the worker has not yet saved the matching inflight. Suppressing here (or
    // advancing the confirmed offset) would EAT the wait window and drop the
    // wakeup turn's response batch. Keep the bytes buffered until the worker
    // claims (its inflight save then takes over the relay).
    terminal_success_seen
        && inflight_missing
        && !ssh_direct_prompt_pending
        && !external_input_lease_present
        && !assistant_continuation_present
        && !pane_actively_streaming
        && !pending_synthetic_start_present
}

#[cfg(test)]
mod post_terminal_output_tests {
    use super::{
        should_skip_streaming_placeholder_without_inflight,
        should_suppress_post_terminal_output_without_inflight,
    };

    #[test]
    fn post_terminal_output_without_inflight_is_suppressed() {
        assert!(should_suppress_post_terminal_output_without_inflight(
            true, true, false, false, false, false, false
        ));
        assert!(
            !should_suppress_post_terminal_output_without_inflight(
                false, true, false, false, false, false, false
            ),
            "pre-terminal output still belongs to the active watcher turn"
        );
        assert!(
            !should_suppress_post_terminal_output_without_inflight(
                true, false, false, false, false, false, false
            ),
            "a newly active inflight owns subsequent output"
        );
        assert!(
            !should_suppress_post_terminal_output_without_inflight(
                true, true, true, false, false, false, false
            ),
            "SSH-direct prompt anchor present: output is a real direct-input response"
        );
        assert!(
            !should_suppress_post_terminal_output_without_inflight(
                true, true, false, true, false, false, false
            ),
            "ExternalInput lease present: notification failure must not suppress response output"
        );
        assert!(
            !should_suppress_post_terminal_output_without_inflight(
                true, true, false, false, true, false, false
            ),
            "assistant continuation after early terminal relay still belongs to the provider turn"
        );
    }

    #[test]
    fn post_terminal_hard_result_after_committed_turn_requires_direct_input_evidence() {
        assert!(
            should_suppress_post_terminal_output_without_inflight(
                true, true, false, false, false, false, false
            ),
            "a late hard_result envelope after a committed Discord turn must not relay again"
        );
        assert!(
            !should_suppress_post_terminal_output_without_inflight(
                true, true, true, false, false, false, false
            ),
            "a pending SSH-direct prompt is explicit evidence of a fresh direct-input turn"
        );
        assert!(
            !should_suppress_post_terminal_output_without_inflight(
                true, true, false, true, false, false, false
            ),
            "an ExternalInput lease is explicit evidence of a fresh direct-input turn"
        );
        assert!(
            should_suppress_post_terminal_output_without_inflight(
                true, true, false, false, false, false, false
            ),
            "a result-only duplicate without assistant continuation stays suppressed"
        );
    }

    #[test]
    fn post_terminal_output_with_actively_streaming_pane_is_not_suppressed() {
        // #3107: the (terminal + no-inflight) shape that would otherwise be
        // suppressed must still relay when the pane is actively producing —
        // the turn is live and merely lost its inflight.
        assert!(
            !should_suppress_post_terminal_output_without_inflight(
                true, true, false, false, false, true, false
            ),
            "an actively-streaming pane means the turn is live: relay, do not suppress"
        );
        // Asymmetry: with a finished/idle pane the same shape is still genuine
        // post-finish ghost noise and stays suppressed.
        assert!(
            should_suppress_post_terminal_output_without_inflight(
                true, true, false, false, false, false, false
            ),
            "a finished pane with missing inflight is real ghost noise: still suppressed"
        );
    }

    #[test]
    fn post_terminal_output_with_pending_synthetic_start_is_not_suppressed() {
        // #3154: while a deferred synthetic turn-start is pending for this
        // channel (the per-channel worker has not yet saved the matching
        // inflight), the (terminal + no-inflight) shape that would otherwise be
        // suppressed must keep its bytes buffered — suppressing here would EAT
        // the wait window and drop the wakeup turn's response batch.
        assert!(
            !should_suppress_post_terminal_output_without_inflight(
                true, true, false, false, false, false, true
            ),
            "a pending synthetic turn-start must keep post-terminal bytes buffered, not suppress"
        );
        // Without the pending start the same shape is genuine ghost noise.
        assert!(
            should_suppress_post_terminal_output_without_inflight(
                true, true, false, false, false, false, false
            ),
            "no pending synthetic start: real ghost noise stays suppressed"
        );
    }

    #[test]
    fn streaming_placeholder_without_inflight_is_skipped() {
        // Genuine ghost noise: inflight missing AND pane idle/finished.
        assert!(should_skip_streaming_placeholder_without_inflight(
            true, false
        ));
        assert!(!should_skip_streaming_placeholder_without_inflight(
            false, false
        ));
        // #3107 asymmetry: inflight missing but pane actively streaming → the
        // live turn lost its inflight; do NOT skip the streaming edit.
        assert!(
            !should_skip_streaming_placeholder_without_inflight(true, true),
            "an actively-streaming pane with missing inflight is a live turn: relay"
        );
        // A present inflight is never skipped regardless of pane state.
        assert!(!should_skip_streaming_placeholder_without_inflight(
            false, true
        ));
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WatcherClaimAction {
    SpawnFresh,
    SpawnReplacedStale,
    SpawnReplacedDifferentSession,
    SpawnReplacedForced,
    ReuseExisting,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct WatcherClaimOutcome {
    action: WatcherClaimAction,
    owner_channel_id: ChannelId,
}

impl WatcherClaimOutcome {
    fn new(action: WatcherClaimAction, owner_channel_id: ChannelId) -> Self {
        Self {
            action,
            owner_channel_id,
        }
    }

    pub(crate) fn owner_channel_id(self) -> ChannelId {
        self.owner_channel_id
    }

    pub(crate) fn should_spawn(self) -> bool {
        matches!(
            self.action,
            WatcherClaimAction::SpawnFresh
                | WatcherClaimAction::SpawnReplacedStale
                | WatcherClaimAction::SpawnReplacedDifferentSession
                | WatcherClaimAction::SpawnReplacedForced
        )
    }

    pub(crate) fn replaced_existing(self) -> bool {
        matches!(
            self.action,
            WatcherClaimAction::SpawnReplacedStale
                | WatcherClaimAction::SpawnReplacedDifferentSession
                | WatcherClaimAction::SpawnReplacedForced
        )
    }

    pub(crate) fn as_str(self) -> &'static str {
        match self.action {
            WatcherClaimAction::SpawnFresh => "spawn_fresh",
            WatcherClaimAction::SpawnReplacedStale => "spawn_replaced_stale",
            WatcherClaimAction::SpawnReplacedDifferentSession => "spawn_replaced_different_session",
            WatcherClaimAction::SpawnReplacedForced => "spawn_replaced_forced",
            WatcherClaimAction::ReuseExisting => "reuse_existing",
        }
    }
}

pub(super) fn find_watcher_by_tmux_session(
    watchers: &TmuxWatcherRegistry,
    tmux_session_name: &str,
) -> Option<(ChannelId, bool, bool, String)> {
    let owner = watchers.owner_channel_for_tmux_session(tmux_session_name)?;
    let entry = watchers.by_tmux_session.get(tmux_session_name)?;
    Some((
        owner,
        entry.heartbeat_stale() || entry.cancel.load(std::sync::atomic::Ordering::Relaxed),
        entry.paused.load(std::sync::atomic::Ordering::Relaxed),
        entry.output_path.clone(),
    ))
}

fn restore_scan_should_skip_existing_watcher(
    cancelled: bool,
    paused: bool,
    existing_output_path: &str,
    restored_output_path: &str,
) -> bool {
    !cancelled && !paused && existing_output_path == restored_output_path
}

/// #226/#1170: Atomically claim a tmux session for watcher creation.
/// Returns true if the claim succeeded (caller should spawn the watcher).
/// Returns false if a watcher already exists (caller should skip).
pub(in crate::services::discord) fn try_claim_watcher(
    watchers: &TmuxWatcherRegistry,
    channel_id: ChannelId,
    handle: TmuxWatcherHandle,
) -> bool {
    let guard = lock_tmux_watcher_registry();
    let requested_tmux = handle.tmux_session_name.clone();
    let requested_output_path = handle.output_path.clone();
    if let Some(existing) = find_watcher_by_tmux_session(watchers, &requested_tmux) {
        if existing.1 || existing.2 || existing.3 != requested_output_path {
            if let Some((_, existing_handle)) =
                watchers.remove_tmux_session_locked(&guard, &requested_tmux)
            {
                existing_handle
                    .cancel
                    .store(true, std::sync::atomic::Ordering::Relaxed);
            }
        } else {
            record_watcher_invariant(
                true,
                None,
                channel_id,
                "watcher_one_per_tmux_session",
                "src/services/discord/tmux.rs:try_claim_watcher",
                "same tmux session must reuse the live watcher slot",
                serde_json::json!({
                    "existing_channel_id": existing.0.get(),
                    "tmux_session_name": requested_tmux,
                    "output_path": requested_output_path,
                    "watcher_slots": watchers.len(),
                }),
            );
            return false;
        }
    }
    let claimed = if watchers.contains_key(&channel_id) {
        false
    } else {
        watchers.insert_locked(&guard, channel_id, handle);
        true
    };
    let slot_present = watchers.contains_key(&channel_id);
    record_watcher_invariant(
        slot_present,
        None,
        channel_id,
        "watcher_one_per_channel",
        "src/services/discord/tmux.rs:try_claim_watcher",
        "watcher claim must leave a single channel-owned watcher slot",
        serde_json::json!({
            "claimed": claimed,
            "watcher_slots": watchers.len(),
        }),
    );
    debug_assert!(
        slot_present,
        "watcher claim must leave a channel-owned watcher slot"
    );
    claimed
}

/// Claim a channel for watcher creation with the #1135 single-watcher policy.
///
/// Same tmux session:
/// - live incumbent: reuse it and do not spawn another watcher;
/// - cancelled incumbent: remove it and spawn the requested watcher.
///
/// Same channel but a different tmux session still replaces the incumbent. That
/// preserves the existing new-turn recovery behavior without allowing two
/// owners for one tmux session.
pub(in crate::services::discord) fn claim_or_reuse_watcher(
    watchers: &TmuxWatcherRegistry,
    channel_id: ChannelId,
    handle: TmuxWatcherHandle,
    provider: &ProviderKind,
    source: &str,
) -> WatcherClaimOutcome {
    claim_watcher(watchers, channel_id, handle, provider, source, false)
}

/// Force a fresh watcher/converter generation even when a live same-session
/// incumbent watches the same output path. Recovery uses this only after it
/// proves that the persisted Codex render seed belongs to an earlier provider
/// turn: reusing that incumbent would keep the stale Discord anchor alive.
pub(in crate::services::discord) fn claim_or_replace_watcher(
    watchers: &TmuxWatcherRegistry,
    channel_id: ChannelId,
    handle: TmuxWatcherHandle,
    provider: &ProviderKind,
    source: &str,
) -> WatcherClaimOutcome {
    claim_watcher(watchers, channel_id, handle, provider, source, true)
}

pub(super) fn claim_watcher(
    watchers: &TmuxWatcherRegistry,
    channel_id: ChannelId,
    handle: TmuxWatcherHandle,
    provider: &ProviderKind,
    source: &str,
    force_replace_live_same_tmux: bool,
) -> WatcherClaimOutcome {
    let guard = lock_tmux_watcher_registry();
    let requested_tmux = handle.tmux_session_name.clone();
    let requested_output_path = handle.output_path.clone();
    let mut removed_stale_same_tmux = false;

    if let Some((existing_channel_id, existing_cancelled, existing_paused, existing_output_path)) =
        find_watcher_by_tmux_session(watchers, &requested_tmux)
    {
        let replace_paused_incumbent =
            existing_paused && !matches!(source, "turn_start_message" | "turn_start_headless");
        if force_replace_live_same_tmux
            || existing_cancelled
            || replace_paused_incumbent
            || existing_output_path != requested_output_path
        {
            if let Some((_, existing_handle)) =
                watchers.remove_tmux_session_locked(&guard, &requested_tmux)
            {
                existing_handle
                    .cancel
                    .store(true, std::sync::atomic::Ordering::Relaxed);
                // #3277 (Defect B): this cancel+remove was completely silent —
                // in the incident the replaced incumbent's later "stopped" log
                // was misattributed to the replacement watcher. Log the claim.
                tracing::info!(
                    source,
                    tmux_session = %requested_tmux,
                    existing_channel = existing_channel_id.get(),
                    existing_cancelled,
                    force_replace_live_same_tmux,
                    replace_paused_incumbent,
                    output_path_changed = existing_output_path != requested_output_path,
                    "watcher claim cancelled same-tmux incumbent before spawning replacement"
                );
            }
            removed_stale_same_tmux = true;
        } else {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] ⏭ watcher reuse for channel {} — tmux {} is already watched by channel {}",
                channel_id,
                requested_tmux,
                existing_channel_id
            );
            record_watcher_invariant(
                true,
                Some(provider),
                channel_id,
                "watcher_one_per_tmux_session",
                "src/services/discord/tmux.rs:claim_or_reuse_watcher",
                "same tmux session must reuse the live watcher slot",
                serde_json::json!({
                    "source": source,
                    "existing_channel_id": existing_channel_id.get(),
                    "tmux_session_name": requested_tmux,
                    "output_path": requested_output_path,
                    "watcher_slots": watchers.len(),
                }),
            );
            return WatcherClaimOutcome::new(
                WatcherClaimAction::ReuseExisting,
                existing_channel_id,
            );
        }
    }

    let outcome = if let Some(entry) = watchers.get(&channel_id) {
        let previous_tmux = entry.tmux_session_name.clone();
        let same_tmux = previous_tmux == requested_tmux;
        entry
            .cancel
            .store(true, std::sync::atomic::Ordering::Relaxed);
        let stale_cancelled = entry.cancel.load(std::sync::atomic::Ordering::Relaxed);
        record_watcher_invariant(
            stale_cancelled,
            Some(provider),
            channel_id,
            "watcher_replacement_cancels_stale",
            "src/services/discord/tmux.rs:claim_or_reuse_watcher",
            "replacing a watcher must cancel the stale watcher before installing the new handle",
            serde_json::json!({
                "source": source,
                "same_tmux": same_tmux,
                "previous_tmux_session_name": previous_tmux,
                "tmux_session_name": requested_tmux.as_str(),
            }),
        );
        debug_assert!(
            stale_cancelled,
            "stale watcher must be cancelled before replacement"
        );
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::info!(
            "  [{ts}] ♻ watcher replaced for channel {} — cancelled stale watcher",
            channel_id
        );
        drop(entry);
        watchers.insert_locked(&guard, channel_id, handle);
        crate::services::observability::emit_watcher_replaced(
            provider.as_str(),
            channel_id.get(),
            source,
        );
        if force_replace_live_same_tmux && same_tmux {
            WatcherClaimOutcome::new(WatcherClaimAction::SpawnReplacedForced, channel_id)
        } else if same_tmux {
            WatcherClaimOutcome::new(WatcherClaimAction::SpawnReplacedStale, channel_id)
        } else {
            WatcherClaimOutcome::new(
                WatcherClaimAction::SpawnReplacedDifferentSession,
                channel_id,
            )
        }
    } else {
        watchers.insert_locked(&guard, channel_id, handle);
        if force_replace_live_same_tmux && removed_stale_same_tmux {
            WatcherClaimOutcome::new(WatcherClaimAction::SpawnReplacedForced, channel_id)
        } else if removed_stale_same_tmux {
            WatcherClaimOutcome::new(WatcherClaimAction::SpawnReplacedStale, channel_id)
        } else {
            WatcherClaimOutcome::new(WatcherClaimAction::SpawnFresh, channel_id)
        }
    };
    let slot_present = watchers.contains_key(&channel_id);
    record_watcher_invariant(
        slot_present,
        Some(provider),
        channel_id,
        "watcher_one_per_channel",
        "src/services/discord/tmux.rs:claim_or_reuse_watcher",
        "watcher replacement must leave exactly one channel-owned watcher slot",
        serde_json::json!({
            "outcome": outcome.as_str(),
            "source": source,
            "watcher_slots": watchers.len(),
        }),
    );
    debug_assert!(
        slot_present,
        "watcher replacement must leave a channel-owned watcher slot"
    );
    outcome
}

use crate::services::tmux_common::{current_tmux_owner_marker, tmux_owner_path};

pub(in crate::services::discord) fn session_belongs_to_current_runtime(
    session_name: &str,
    current_owner_marker: &str,
) -> bool {
    std::fs::read_to_string(tmux_owner_path(session_name))
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .map(|value| value == current_owner_marker)
        .unwrap_or(false)
}

/// On startup, scan for surviving tmux sessions (AgentDesk-*) and restore watchers.
/// This handles the case where AgentDesk was restarted but tmux sessions are still alive.
pub(in crate::services::discord) async fn restore_tmux_watchers(
    http: &Arc<serenity::Http>,
    shared: &Arc<SharedData>,
) {
    let settings_snapshot = { shared.settings.read().await.clone() };
    let provider = settings_snapshot.provider.clone();

    // List tmux sessions matching our naming convention
    let output = match tokio::time::timeout(
        std::time::Duration::from_secs(10),
        tokio::task::spawn_blocking(crate::services::platform::tmux::list_session_names),
    )
    .await
    {
        Ok(Ok(Ok(names))) => names,
        _ => return, // No tmux, timeout, or no sessions
    };

    let agent_sessions: Vec<&str> = output
        .iter()
        .map(|l| l.trim())
        .filter(|l| {
            parse_provider_and_channel_from_tmux_name(l)
                .map(|(session_provider, _)| session_provider == provider)
                .unwrap_or(false)
        })
        .collect();

    if agent_sessions.is_empty() {
        return;
    }

    // Build channel name → ChannelId map from Discord API (sessions map may be empty after restart)
    let mut name_to_channel: std::collections::HashMap<String, (ChannelId, String)> =
        std::collections::HashMap::new();

    // Try from in-memory sessions first
    {
        let data = shared.core.lock().await;
        for (&ch_id, session) in &data.sessions {
            if let Some(ref ch_name) = session.channel_name {
                let tmux_name = provider.build_tmux_session_name(ch_name);
                name_to_channel.insert(tmux_name, (ch_id, ch_name.clone()));
            }
        }
    }

    // If in-memory sessions don't cover all tmux sessions, fetch from Discord API
    let unresolved: Vec<&&str> = agent_sessions
        .iter()
        .filter(|s| !name_to_channel.contains_key(**s))
        .collect();

    if !unresolved.is_empty() {
        // Fetch guild channels via Discord API
        if let Ok(guilds) = http.get_guilds(None, None).await {
            for guild_info in &guilds {
                if let Ok(channels) = guild_info.id.channels(http).await {
                    for (ch_id, channel) in &channels {
                        let role_binding = resolve_role_binding(*ch_id, Some(&channel.name));
                        if !channel_supports_provider(
                            &provider,
                            Some(&channel.name),
                            false,
                            role_binding.as_ref(),
                        ) {
                            continue;
                        }
                        let tmux_name = provider.build_tmux_session_name(&channel.name);
                        name_to_channel
                            .entry(tmux_name)
                            .or_insert((*ch_id, channel.name.clone()));
                    }
                }
            }
        }

        // Fallback for thread sessions: guild.channels() doesn't return threads.
        // Extract thread_id from the channel name suffix (-t{id}) and use it
        // as the channel_id directly, since Discord thread IDs are channel IDs.
        let still_unresolved: Vec<&&str> = agent_sessions
            .iter()
            .filter(|s| !name_to_channel.contains_key(**s))
            .collect();
        for session_name in &still_unresolved {
            if let Some((_, ch_name)) = parse_provider_and_channel_from_tmux_name(session_name) {
                if let Some(pos) = ch_name.rfind("-t") {
                    let suffix = &ch_name[pos + 2..];
                    if !suffix.is_empty() && suffix.chars().all(|c| c.is_ascii_digit()) {
                        if let Ok(thread_id) = suffix.parse::<u64>() {
                            let channel_id = ChannelId::new(thread_id);
                            name_to_channel
                                .entry(session_name.to_string())
                                .or_insert((channel_id, ch_name.clone()));
                        }
                    }
                }
            }
        }
    }

    // Collect sessions to restore
    struct PendingWatcher {
        channel_id: ChannelId,
        output_path: String,
        session_name: String,
        initial_offset: u64,
        restored_turn: Option<RestoredWatcherTurn>,
        codex_direct_resume_fallback: Option<codex_restore::DirectResumeFallback>,
    }

    // Dead sessions that need DB cleanup (idle status report + tmux kill)
    struct DeadSessionCleanup {
        channel_id: u64,
        channel_name: String,
        session_name: String,
    }

    let mut pending: Vec<PendingWatcher> = Vec::new();
    let mut dead_cleanups: Vec<DeadSessionCleanup> = Vec::new();
    let mut owned_sessions: std::collections::HashMap<ChannelId, String> =
        std::collections::HashMap::new();
    let mut restore_claimed_claude_tui_transcripts: std::collections::HashSet<std::path::PathBuf> =
        std::collections::HashSet::new();

    for session_name in &agent_sessions {
        let Some((channel_id, channel_name)) = name_to_channel.get(*session_name) else {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] ⏭ watcher skip for {} — channel mapping not found",
                session_name
            );
            continue;
        };

        // #148: Do NOT register in owned_sessions yet — QUARANTINE check below may
        // skip this session. Registering early blocks new session creation for the channel.
        let is_dm = matches!(
            channel_id.to_channel(http.as_ref()).await,
            Ok(serenity::model::channel::Channel::Private(_))
        );
        // Resolve thread parent so validation uses the same semantics
        // as normal message routing (router.rs).
        let (allowlist_channel_id, provider_channel_name) = if let Some((pid, pname)) =
            super::super::resolve_thread_parent(http, *channel_id).await
        {
            (pid, pname.unwrap_or_else(|| channel_name.clone()))
        } else {
            (*channel_id, channel_name.clone())
        };
        if let Err(reason) = validate_bot_channel_routing_with_provider_channel(
            &settings_snapshot,
            &provider,
            allowlist_channel_id,
            Some(&channel_name),
            Some(&provider_channel_name),
            is_dm,
        ) {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] ⏭ watcher skip for {} — {reason} for channel {}",
                session_name,
                channel_id
            );
            continue;
        }

        if let Some(started) = super::super::mailbox_snapshot(&shared, *channel_id)
            .await
            .recovery_started_at
        {
            // #2443 — `recovery_done.wait()` is the deterministic graduation
            // signal for this skip. `restore_tmux_watchers` is a one-shot
            // caller (the loop body simply `continue`s and the upper
            // restore-loop tick reruns later), so we cannot block here for
            // ~60s. Instead, we race a *short* `recovery_done.wait()` against
            // a near-zero timeout: if recovery has already completed (latch
            // set), we proceed immediately; otherwise we fall through to the
            // legacy 60s skip / stale-cleanup heuristic which acts as the
            // hook-miss safety net the issue body asked us to retain.
            //
            // The 100ms grace window catches the common case where recovery
            // completed *just before* the watcher loop reached this check
            // (the producer in `mailbox_clear_recovery_marker` / `finish_turn`
            // calls `mark_done()` *after* clearing `recovery_started_at`, so
            // a clean completion already short-circuits via the snapshot
            // being `None` — this branch only runs when the snapshot still
            // sees a started marker, i.e. we *just* missed the wake-up).
            let recovery_done =
                crate::services::turn_orchestrator::ChannelMailboxRegistry::global_recovery_done(
                    *channel_id,
                );
            let recovery_completed = if let Some(signal) = recovery_done.as_ref() {
                tokio::time::timeout(std::time::Duration::from_millis(100), signal.wait())
                    .await
                    .is_ok()
            } else {
                false
            };

            if recovery_completed {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::info!(
                    "  [{ts}] ✅ recovery_done signal observed for {} — proceeding with watcher restore",
                    session_name
                );
                super::super::mailbox_clear_recovery_marker(&shared, *channel_id).await;
            } else if started.elapsed() < std::time::Duration::from_secs(60) {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::info!(
                    "  [{ts}] ⏳ watcher skip for {} — recovery in progress ({:.0}s ago, hook-miss fallback)",
                    session_name,
                    started.elapsed().as_secs_f64()
                );
                continue;
            } else {
                // Stale recovery — remove marker and proceed with watcher.
                // Reaching this branch means the 60s hook-miss fallback
                // tripped; track it so we can monitor `recovery_done`
                // signal coverage in the field.
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::warn!(
                    "  [{ts}] ⚠ clearing stale recovery marker for {} ({:.0}s elapsed) — recovery_done hook missed",
                    session_name,
                    started.elapsed().as_secs_f64()
                );
                super::super::mailbox_clear_recovery_marker(&shared, *channel_id).await;
            }
        }

        // Accept either the new persistent location or the legacy /tmp
        // location — older wrappers still write to /tmp, and a dcserver
        // restart that lost /tmp files should not falsely flag a live
        // session as "no output file". See issue #892.
        //
        // #2795: codex_tui writes its rollout transcript directly to
        // `~/.codex/sessions/...` and never lands a JSONL at the AgentDesk
        // resolve path. When a dcserver restart happens mid-turn (agent ran
        // deploy from inside its own turn), the inflight row is preserved
        // but the AgentDesk relay JSONL is absent. Fall back to the actual
        // codex rollout looked up by the inflight `session_id` so the
        // restore loop can still attach a watcher and keep the live pane
        // relayed.
        let configured_workspace =
            super::super::settings::resolve_workspace(*channel_id, Some(channel_name.as_str()));
        let session_keys = super::super::adk_session::build_session_key_candidates(
            &shared.token_hash,
            &provider,
            session_name,
        );
        let restored_cwd =
            load_restored_session_cwd(shared.pg_pool.as_ref(), &session_keys, channel_id.get());

        let mut selected_claude_tui_fallback_transcript: Option<std::path::PathBuf> = None;
        let mut codex_direct_resume_fallback = None;
        let output_path =
            match crate::services::tmux_common::resolve_session_temp_path(session_name, "jsonl") {
                Some(path) => path,
                None => {
                    if let Some(path) =
                        codex_restore::rollout_fallback_for_session(&provider, *channel_id)
                    {
                        let ts = chrono::Local::now().format("%H:%M:%S");
                        tracing::info!(
                            "  [{ts}] ↻ watcher restore for {} — codex rollout fallback {}",
                            session_name,
                            path
                        );
                        path
                    } else if let Some(path) =
                        codex_restore::rollout_fallback_for_live_direct_resume(
                            &provider,
                            session_name,
                            *channel_id,
                        )
                    {
                        let output_path = path.output_path().to_string();
                        codex_direct_resume_fallback = Some(path);
                        output_path
                    } else if let Some(path) = claude_tui_transcript_fallback_path(
                        &provider,
                        session_name,
                        configured_workspace.as_deref(),
                        restored_cwd.as_deref(),
                        shared,
                        None,
                        &restore_claimed_claude_tui_transcripts,
                    ) {
                        // #2853: claude_tui never lands the wrapper JSONL, so
                        // recover the watcher onto the freshest safe Claude
                        // rollout transcript for the actual launched cwd,
                        // bounded by launch time and other live-session claims.
                        selected_claude_tui_fallback_transcript =
                            Some(std::path::PathBuf::from(&path));
                        let ts = chrono::Local::now().format("%H:%M:%S");
                        tracing::info!(
                            "  [{ts}] ↻ watcher restore for {} — claude transcript fallback {}",
                            session_name,
                            path
                        );
                        path
                    } else {
                        let ts = chrono::Local::now().format("%H:%M:%S");
                        tracing::info!(
                            "  [{ts}] ⏭ watcher skip for {} — no output file",
                            session_name
                        );
                        continue;
                    }
                }
            };

        if let Some((owner_channel_id, cancelled, paused, existing_output_path)) =
            find_watcher_by_tmux_session(&shared.tmux_watchers, session_name)
        {
            if restore_scan_should_skip_existing_watcher(
                cancelled,
                paused,
                &existing_output_path,
                &output_path,
            ) {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::info!(
                    "  [{ts}] ⏭ watcher skip for {} — tmux session already watched by channel {}",
                    session_name,
                    owner_channel_id
                );
                continue;
            }
            if !cancelled {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::info!(
                    "  [{ts}] ↻ watcher replace for {} — existing output path {} differs from restored output path {}",
                    session_name,
                    existing_output_path,
                    output_path
                );
            }
        }

        // Old-gen sessions: adopt instead of killing.
        // The tmux session and Claude CLI process are still alive from the
        // previous dcserver — just update the generation marker and re-attach
        // a watcher. Auto-retry handles stale Claude session IDs if needed.
        let gen_marker_path =
            crate::services::tmux_common::session_temp_path(session_name, "generation");
        let session_gen = std::fs::read_to_string(&gen_marker_path)
            .ok()
            .and_then(|s| s.trim().parse::<u64>().ok())
            .unwrap_or(0);
        let current_gen = super::super::runtime_store::load_generation();
        if session_gen < current_gen && current_gen > 0 {
            // Skip sessions belonging to other runtimes
            let current_owner_marker = current_tmux_owner_marker();
            if !session_belongs_to_current_runtime(session_name, &current_owner_marker) {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::info!(
                    "  [{ts}] ⏭ watcher skip for {} — owned by other runtime",
                    session_name
                );
                continue;
            }
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] ↻ Adopting old-gen session {} (gen {} → {})",
                session_name,
                session_gen,
                current_gen
            );
            // Update generation marker to current gen, preserving the
            // existing mtime.
            //
            // #1275 P2 #1: the `.generation` mtime is the wrapper-identity
            // signal used by `watermark_after_output_regression`. Adoption
            // does NOT respawn the wrapper (the tmux session and Claude CLI
            // process are still alive from the previous dcserver), so the
            // mtime must stay pinned to its original value. Otherwise a
            // restored watcher with `last_watcher_relayed_generation_mtime_ns`
            // captured before the dcserver restart will mismatch the freshly
            // touched `.generation` mtime, the regression check classifies
            // as fresh wrapper, clears `last_relayed_offset`, and a rotated
            // jsonl re-relays surviving content.
            preserve_mtime_after_write(
                &gen_marker_path,
                current_gen.to_string().as_bytes(),
                "adoption_marker_rewrite",
            );
        }

        if !probe_tmux_session_liveness(session_name).await {
            let ts = chrono::Local::now().format("%H:%M:%S");
            if let Some(diag) = build_tmux_death_diagnostic(session_name, Some(&output_path)) {
                tracing::info!(
                    "  [{ts}] ⏭ watcher skip for {} — tmux pane dead ({diag})",
                    session_name
                );
            } else {
                tracing::info!(
                    "  [{ts}] ⏭ watcher skip for {} — tmux pane dead",
                    session_name
                );
            }
            // Schedule DB cleanup + tmux kill for this dead session
            dead_cleanups.push(DeadSessionCleanup {
                channel_id: channel_id.get(),
                channel_name: channel_name.clone(),
                session_name: session_name.to_string(),
            });
            continue;
        }

        // #148: Only register in owned_sessions after passing QUARANTINE + live-pane checks.
        // Earlier registration blocked new session creation for quarantined/dead channels.
        owned_sessions
            .entry(*channel_id)
            .or_insert_with(|| channel_name.clone());

        let mut restored_turn = None;
        let initial_offset = if let Some(state) =
            super::super::inflight::load_inflight_state(&provider, channel_id.get())
        {
            if let Some(restored_tmux) =
                restored_watcher_turn_from_inflight(&state, session_name, false)
            {
                let finish_mailbox_on_completion =
                    super::super::recovery::reregister_active_turn_from_inflight(&shared, &state)
                        .await;
                restored_turn = Some(RestoredWatcherTurn {
                    finish_mailbox_on_completion,
                    ..restored_tmux
                });
                let file_len = std::fs::metadata(&output_path)
                    .map(|m| m.len())
                    .unwrap_or(0);
                if file_len >= state.last_offset {
                    state.last_offset
                } else {
                    0
                }
            } else {
                std::fs::metadata(&output_path)
                    .map(|m| m.len())
                    .unwrap_or(0)
            }
        } else {
            std::fs::metadata(&output_path)
                .map(|m| m.len())
                .unwrap_or(0)
        };

        pending.push(PendingWatcher {
            channel_id: *channel_id,
            output_path,
            session_name: session_name.to_string(),
            initial_offset,
            restored_turn,
            codex_direct_resume_fallback,
        });
        if let Some(path) = selected_claude_tui_fallback_transcript {
            restore_claimed_claude_tui_transcripts.insert(path);
        }
    }

    // Register sessions in CoreState so cleanup_orphan_tmux_sessions recognizes them
    // and message handlers find an active session with current_path
    if !owned_sessions.is_empty() {
        let mut data = shared.core.lock().await;
        for (channel_id, channel_name) in &owned_sessions {
            let persisted_path = load_last_session_path(
                shared.pg_pool.as_ref(),
                &shared.token_hash,
                channel_id.get(),
            );
            let persisted_session_id = load_restored_provider_session_id(
                shared.pg_pool.as_ref(),
                &shared.token_hash,
                &provider,
                channel_name,
            );
            let configured_path =
                super::super::settings::resolve_workspace(*channel_id, Some(channel_name.as_str()));
            let tmux_name = provider.build_tmux_session_name(channel_name);
            let session_keys = super::super::adk_session::build_session_key_candidates(
                &shared.token_hash,
                &provider,
                &tmux_name,
            );
            let db_cwd =
                load_restored_session_cwd(shared.pg_pool.as_ref(), &session_keys, channel_id.get());

            let session =
                data.sessions
                    .entry(*channel_id)
                    .or_insert_with(|| super::super::DiscordSession {
                        session_id: persisted_session_id.clone(),
                        memento_context_loaded:
                            super::super::session_runtime::restored_memento_context_loaded(
                                false,
                                None,
                                persisted_session_id.as_deref(),
                            ),
                        memento_reflected: false,
                        current_path: None,
                        history: Vec::new(),
                        pending_uploads: Vec::new(),
                        cleared: false,
                        channel_name: Some(channel_name.clone()),
                        category_name: None,
                        remote_profile_name: None,
                        channel_id: Some(channel_id.get()),

                        last_active: tokio::time::Instant::now(),
                        worktree: None,

                        born_generation: super::super::runtime_store::load_generation(),
                    });

            if session.session_id.is_none() && persisted_session_id.is_some() {
                session.restore_provider_session(persisted_session_id.clone());
            }

            // Restore current_path: DB cwd (worktree-aware) > last_sessions (yaml, main workspace)
            if session.current_path.is_none() {
                // #3219: prefer the channel's own reusable managed worktree over
                // the configured base; only log "ignoring" when it is NOT reused.
                let reusable_worktree = super::super::session_runtime::db_cwd_is_reusable_worktree(
                    configured_path.as_deref(),
                    db_cwd.as_deref(),
                );
                if let (Some(configured), Some(restored)) =
                    (configured_path.as_ref(), db_cwd.as_ref())
                {
                    if configured != restored && !reusable_worktree {
                        let ts = chrono::Local::now().format("%H:%M:%S");
                        tracing::info!(
                            "  [{ts}] ⚠ Ignoring restored DB cwd for channel {}: {} (configured workspace: {})",
                            channel_id,
                            restored,
                            configured
                        );
                    }
                }
                let effective_path = super::super::select_restored_session_path(
                    configured_path,
                    db_cwd,
                    persisted_path,
                    reusable_worktree,
                );
                if let Some(path) = effective_path {
                    session.current_path = Some(path);
                }
            }
        }
    }

    // Spawn watchers
    // #226: Use try_claim_watcher for atomic check-and-insert. The pending list
    // was built during the scan phase, which includes async Discord API calls.
    // A normal turn may have created a watcher in the meantime.
    for pw in pending {
        // #226: Skip channels that recovery already handled — their watchers may have
        // ended quickly (session died), removing themselves from the DashMap, but we
        // should not create a second watcher because recovery already processed the turn.
        let recovery_handled =
            recovery_handled_channel_exists(shared.as_ref(), pw.channel_id.get());
        if recovery_handled {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] ⏭ watcher skip for {} — recovery already handled this channel",
                pw.session_name
            );
            continue;
        }

        if pw.restored_turn.is_none() {
            reconcile_orphan_suppressed_placeholder_for_restored_watcher(
                http,
                shared,
                &provider,
                pw.channel_id,
                &pw.session_name,
            )
            .await;
        }

        let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let paused = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let resume_offset = Arc::new(std::sync::Mutex::new(None::<u64>));
        let pause_epoch = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let turn_delivered = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let last_heartbeat_ts_ms = Arc::new(std::sync::atomic::AtomicI64::new(
            super::super::tmux_watcher_now_ms(),
        ));

        let handle = TmuxWatcherHandle {
            tmux_session_name: pw.session_name.clone(),
            output_path: pw.output_path.clone(),
            paused: paused.clone(),
            resume_offset: resume_offset.clone(),
            cancel: cancel.clone(),
            pause_epoch: pause_epoch.clone(),
            turn_delivered: turn_delivered.clone(),
            last_heartbeat_ts_ms: last_heartbeat_ts_ms.clone(),
        };
        if !try_claim_watcher(&shared.tmux_watchers, pw.channel_id, handle) {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] ⏭ watcher skip for {} — already watching (created during scan)",
                pw.session_name
            );
            continue;
        }
        if let Some(fallback) = pw.codex_direct_resume_fallback {
            codex_restore::commit_live_direct_resume_fallback(
                &pw.session_name,
                pw.channel_id,
                fallback,
            );
        }

        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::info!(
            "  [{ts}] ↻ Restoring tmux watcher for {} (offset {})",
            pw.session_name,
            pw.initial_offset
        );

        shared.record_tmux_watcher_reconnect(pw.channel_id);
        super::super::task_supervisor::spawn_observed_tmux_watcher(
            "watchers_lifecycle_tmux_output_watcher_with_restore",
            shared.clone(),
            pw.session_name.clone(),
            cancel.clone(),
            tmux_output_watcher_with_restore(
                pw.channel_id,
                http.clone(),
                shared.clone(),
                pw.output_path,
                pw.session_name,
                pw.initial_offset,
                cancel,
                paused,
                resume_offset,
                pause_epoch,
                turn_delivered,
                last_heartbeat_ts_ms,
                pw.restored_turn,
            ),
        );
    }

    // Clean up dead sessions: report idle to DB and kill tmux sessions
    if !dead_cleanups.is_empty() {
        let api_port = shared.api_port;
        let provider = shared.settings.read().await.provider.clone();

        let mut cleaned_dead_sessions = 0usize;
        for dc in &dead_cleanups {
            let dispatch_protection =
                super::super::tmux_lifecycle::resolve_dispatch_tmux_protection(
                    shared.pg_pool.as_ref(),
                    &shared.token_hash,
                    &provider,
                    &dc.session_name,
                    Some(&dc.channel_name),
                );
            let dispatch_failed_for_dead_session =
                if let Some(protection) = dispatch_protection.as_ref() {
                    super::super::tmux_lifecycle::fail_active_dispatch_for_dead_tmux_session(
                        api_port,
                        protection,
                        &dc.session_name,
                        "tmux_startup",
                    )
                    .await
                } else {
                    false
                };
            let cleanup_plan = dead_session_cleanup_plan(
                dispatch_protection.is_some() && !dispatch_failed_for_dead_session,
            );

            if let Some(protection) = dispatch_protection {
                let ts = chrono::Local::now().format("%H:%M:%S");
                if dispatch_failed_for_dead_session {
                    tracing::warn!(
                        "  [{ts}] tmux startup: failed active dispatch for dead session {} — {}",
                        dc.session_name,
                        protection.log_reason()
                    );
                } else {
                    tracing::info!(
                        "  [{ts}] ♻ tmux startup: preserving dispatch session {} — {}",
                        dc.session_name,
                        protection.log_reason()
                    );
                }
            }

            let tmux_name = provider.build_tmux_session_name(&dc.channel_name);
            let thread_channel_id =
                super::super::adk_session::parse_thread_channel_id_from_name(&dc.channel_name);
            let session_key = super::super::adk_session::build_namespaced_session_key(
                &shared.token_hash,
                &provider,
                &tmux_name,
            );
            let agent_id =
                resolve_role_binding(ChannelId::new(dc.channel_id), Some(&dc.channel_name))
                    .map(|binding| binding.role_id);

            if cleanup_plan.report_idle_status {
                super::super::adk_session::post_adk_session_status(
                    Some(&session_key),
                    Some(&dc.channel_name),
                    None,
                    "idle",
                    &provider,
                    None,
                    None,
                    None,
                    None,
                    thread_channel_id,
                    Some(ChannelId::new(dc.channel_id)),
                    agent_id.as_deref(),
                    api_port,
                )
                .await;
            }

            if cleanup_plan.preserve_tmux_session {
                continue;
            }

            // Kill the dead tmux session
            let sess = dc.session_name.clone();
            let _ = tokio::task::spawn_blocking(move || {
                crate::services::termination_audit::record_termination_for_tmux(
                    &sess,
                    None,
                    "tmux_startup",
                    "startup_dead_session",
                    Some("startup cleanup: dead session"),
                    None,
                );
                record_tmux_exit_reason(&sess, "startup cleanup: dead session");
                crate::services::platform::tmux::kill_session(
                    &sess,
                    "startup cleanup: dead session",
                );
            })
            .await;
            cleaned_dead_sessions += 1;
        }

        if cleaned_dead_sessions > 0 {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] 🧹 Cleaned {} dead tmux session(s) on startup",
                cleaned_dead_sessions
            );
        }

        // Sweep orphan session temp files (no matching tmux session AND
        // owner marker older than the threshold). Conservative: skip the
        // legacy /tmp directory (those files may still be held open by
        // pre-migration wrappers) — we only clean the new persistent
        // directory. See issue #892.
        sweep_orphan_session_files().await;
    }
}

#[cfg(test)]
mod restored_session_cwd_channel_isolation_tests {
    //! #3207 (part 2) P0-b: watcher restart recovery resolves a restored
    //! session's cwd via `load_restored_session_cwd` and injects it into the
    //! restored runtime state (`session.current_path` via
    //! `select_restored_session_path`). The lookup must be scoped by the unique
    //! Discord channel id: two channels whose sanitized/truncated names collide
    //! share one `session_key`, and without the `channel_id = $2` predicate the
    //! recovering channel would recover into the OTHER channel's working tree.
    //! RED before the predicate was added, GREEN after.
    use super::load_restored_session_cwd;
    use crate::db::auto_queue::test_support::TestPostgresDb;
    use crate::services::discord::adk_session::build_namespaced_session_key;
    use crate::services::provider::ProviderKind;

    async fn seed_session(
        pool: &sqlx::PgPool,
        session_key: &str,
        channel_id: Option<&str>,
        cwd: &str,
    ) {
        sqlx::query(
            "INSERT INTO sessions (session_key, provider, status, cwd, channel_id, last_heartbeat)
             VALUES ($1, 'claude', 'idle', $2, $3, NOW())",
        )
        .bind(session_key)
        .bind(cwd)
        .bind(channel_id)
        .execute(pool)
        .await
        .expect("seed sessions row");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn watcher_recovery_cwd_does_not_cross_channels() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;

        let provider = ProviderKind::Claude;
        let token_hash = "tok-3207-watcher";
        let collide_name = "shared-watcher-name";
        let tmux_name = provider.build_tmux_session_name(collide_name);
        let session_key = build_namespaced_session_key(token_hash, &provider, &tmux_name);
        let session_keys = vec![session_key.clone()];
        let channel_a: u64 = 777_777_777_777_777_777;
        let channel_b: u64 = 888_888_888_888_888_888;

        // `load_restored_session_cwd` only returns a path that exists on disk
        // (`is_dir()`), so seed the owner's cwd as a real temp directory.
        let owner_dir =
            std::env::temp_dir().join(format!("adk-3207-watcher-{}", std::process::id()));
        std::fs::create_dir_all(&owner_dir).expect("create owner cwd dir");
        let owner_cwd = owner_dir.to_string_lossy().to_string();

        seed_session(
            &pool,
            &session_key,
            Some(&channel_a.to_string()),
            &owner_cwd,
        )
        .await;

        // Owner channel recovers its own cwd.
        let owner = load_restored_session_cwd(Some(&pool), &session_keys, channel_a);
        assert_eq!(
            owner.as_deref(),
            Some(owner_cwd.as_str()),
            "the owning channel must recover its own persisted cwd"
        );

        // The colliding (different-id) channel must NOT recover channel A's cwd
        // (RED before the P0-b `channel_id = $2` fix).
        let cross = load_restored_session_cwd(Some(&pool), &session_keys, channel_b);
        assert_eq!(
            cross, None,
            "a different channel sharing the same session_key must NOT recover \
             another channel's working tree"
        );

        let _ = std::fs::remove_dir_all(&owner_dir);
        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn watcher_recovery_cwd_legacy_null_channel_id_not_reused() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;

        let provider = ProviderKind::Claude;
        let token_hash = "tok-3207-watcher-legacy";
        let channel_name = "legacy-watcher-chan";
        let tmux_name = provider.build_tmux_session_name(channel_name);
        let session_key = build_namespaced_session_key(token_hash, &provider, &tmux_name);
        let session_keys = vec![session_key.clone()];
        let channel_id: u64 = 999_999_999_999_999_999;

        let dir =
            std::env::temp_dir().join(format!("adk-3207-watcher-legacy-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create legacy cwd dir");
        let cwd = dir.to_string_lossy().to_string();

        // A row written before the channel_id column existed has NULL channel_id.
        seed_session(&pool, &session_key, None, &cwd).await;

        let resolved = load_restored_session_cwd(Some(&pool), &session_keys, channel_id);
        assert_eq!(
            resolved, None,
            "a legacy NULL-channel_id row must not be reused for watcher recovery"
        );

        let _ = std::fs::remove_dir_all(&dir);
        pool.close().await;
        pg_db.drop().await;
    }
}
