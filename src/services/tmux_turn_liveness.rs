use std::path::Path;

use crate::services::agent_protocol::RuntimeHandoffKind;
use crate::services::platform::tmux::{SessionPresence, session_presence};
use crate::services::provider::ProviderKind;

#[cfg(test)]
#[path = "tmux_turn_liveness/tests_pg.rs"]
mod tests_pg;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum IndependentTmuxReadiness {
    ReadyForInput,
    Missing,
    LiveOrAmbiguous,
}

/// Destructive idle cleanup needs evidence from the provider itself. Relay
/// offsets, missing inflight records and old heartbeat timestamps cannot prove
/// that a provider stopped working. Missing bindings/captures fail closed.
pub(crate) fn provider_session_is_proven_idle(tmux_session_name: &str) -> bool {
    crate::services::tmux_common::with_tmux_source_authority(tmux_session_name, |authority| {
        provider_session_is_proven_idle_under_authority(authority).is_ok()
    })
}

/// Keep the final provider probe and kill under the same source authority used
/// by runtime rebinding, so a replacement binding cannot race an idle decision.
/// Err names why the session was preserved; Ok(false) means the kill failed.
pub(crate) fn kill_proven_idle_provider_session(
    tmux_session_name: &str,
    reason: &str,
) -> Result<bool, &'static str> {
    crate::services::tmux_common::with_tmux_source_authority(tmux_session_name, |authority| {
        provider_session_is_proven_idle_under_authority(authority).map(|()| {
            crate::services::platform::tmux::kill_session_output_timeout(
                tmux_session_name,
                reason,
                std::time::Duration::from_secs(2),
            )
            .is_ok_and(|output| output.status.success())
        })
    })
}

fn provider_session_is_proven_idle_under_authority(
    authority: &crate::services::tmux_common::TmuxSourceAuthority<'_>,
) -> Result<(), &'static str> {
    let tmux_session_name = authority.session();
    let Some((provider, _)) =
        crate::services::provider::parse_provider_and_channel_from_tmux_name(tmux_session_name)
    else {
        return Err("provider_unknown");
    };
    let binding =
        crate::services::tui_prompt_dedupe::runtime_binding_for_tmux_session_under_source_authority(
            authority,
        );
    let native_path = binding
        .filter(|binding| {
            matches!(
                binding.runtime_kind,
                RuntimeHandoffKind::ClaudeTui | RuntimeHandoffKind::CodexTui
            )
        })
        .map(|binding| std::path::PathBuf::from(binding.output_path))
        .or_else(|| {
            matches!(provider, ProviderKind::Codex)
                .then(|| {
                    crate::services::codex_tui::session::read_codex_tui_rollout_marker(
                        tmux_session_name,
                    )
                    .map(|marker| marker.rollout_path)
                })
                .flatten()
        });
    let Some(native_path) = native_path else {
        return Err("transcript_unresolved");
    };
    let Some(pane) = crate::services::platform::tmux::capture_pane_timeout(
        tmux_session_name,
        -80,
        std::time::Duration::from_secs(2),
    ) else {
        return Err("pane_capture_failed");
    };
    provider_output_is_proven_idle(&provider, &native_path, &pane)
        .then_some(())
        .ok_or("provider_idle_not_proven")
}

fn provider_output_is_proven_idle(provider: &ProviderKind, native_path: &Path, pane: &str) -> bool {
    use std::io::{Read, Seek, SeekFrom};
    // An empty transcript is not evidence of a completed turn.
    let Ok(before) = std::fs::metadata(native_path) else {
        return false;
    };
    if !before.is_file() || before.len() == 0 {
        return false;
    }
    // The ordinary Claude state observer tolerates torn writes by falling
    // back to a previous record. Destructive cleanup must preserve that
    // ambiguity instead, including a new submission after an old idle result.
    let complete_tail = (|| {
        let mut file = std::fs::File::open(native_path).ok()?;
        file.seek(SeekFrom::Start(before.len().saturating_sub(64 * 1024)))
            .ok()?;
        let mut tail = String::new();
        file.take(64 * 1024).read_to_string(&mut tail).ok()?;
        let last = tail.lines().rev().find(|line| !line.trim().is_empty())?;
        serde_json::from_str::<serde_json::Value>(last).ok()
    })();
    if complete_tail.is_none() {
        return false;
    }
    let turn_state =
        crate::services::tui_turn_state::observe_provider_jsonl_turn_state(provider, native_path);
    provider_idle_evidence_agrees(provider, turn_state, pane)
        && std::fs::metadata(native_path).is_ok_and(|after| {
            after.len() == before.len()
                && after
                    .modified()
                    .ok()
                    .zip(before.modified().ok())
                    .is_some_and(|(after, before)| after == before)
        })
}

fn provider_idle_evidence_agrees(
    provider: &ProviderKind,
    state: crate::services::tui_turn_state::TuiTurnState,
    pane: &str,
) -> bool {
    use crate::services::tui_turn_state::TuiTurnState;
    state == TuiTurnState::Idle
        && crate::services::provider::tmux_capture_indicates_ready_for_input(pane, provider)
        // Permission selectors can coexist with an older idle transcript and
        // composer. This veto deliberately applies to either provider.
        && !crate::services::tmux_common::tmux_capture_indicates_claude_tui_interactive_modal(pane)
        // The ordinary readiness matcher accepts wrapper-ready banners and
        // only recognizes structurally framed interrupt chrome as busy.
        // Destructive cleanup must also veto a bare busy footer:
        // an old ready banner cannot outweigh current interruption controls.
        && !pane
            .lines()
            .rev()
            .filter(|line| !line.trim().is_empty())
            .take(crate::services::tmux_common::CLAUDE_TUI_READINESS_SCAN_LINES)
            .any(|line| line.to_ascii_lowercase().contains("esc to interrupt"))
        && (!matches!(provider, ProviderKind::Claude)
            || !crate::services::tmux_common::tmux_capture_indicates_claude_tui_busy(pane))
}

/// Keep background work and approval waits even if the foreground provider is
/// at a prompt. DB errors or missing rows are unknown, hence not cleanup proof.
pub(crate) async fn idle_cleanup_session_is_unoccupied(
    pool: &sqlx::PgPool,
    session_key: &str,
) -> bool {
    sqlx::query_scalar::<_, bool>(
        "SELECT status IN ('idle', 'disconnected', 'aborted')
             AND active_dispatch_id IS NULL
             AND COALESCE(active_children, 0) = 0
             AND NOT EXISTS (
                 SELECT 1 FROM sessions child
                 WHERE child.parent_session_id = sessions.id AND child.closed_at IS NULL
             )
         FROM sessions WHERE session_key = $1",
    )
    .bind(session_key)
    .fetch_optional(pool)
    .await
    .ok()
    .flatten()
    .unwrap_or(false)
}

#[cfg(test)]
mod idle_cleanup_tests {
    use super::*;
    use crate::services::tui_turn_state::TuiTurnState;

    const READY: &str = "Ready for input (type message + Enter)\n> ";

    #[test]
    fn idle_cleanup_preserves_busy_unknown_and_approval_waiting_provider() {
        for state in [
            TuiTurnState::Streaming,
            TuiTurnState::UserSubmitted,
            TuiTurnState::Unknown,
        ] {
            assert!(!provider_idle_evidence_agrees(
                &ProviderKind::Claude,
                state,
                READY
            ));
        }
        assert!(!provider_idle_evidence_agrees(
            &ProviderKind::Claude,
            TuiTurnState::Idle,
            "Ready for input (type message + Enter)\nAllow / Deny\nEnter to confirm",
        ));
        for provider in [ProviderKind::Claude, ProviderKind::Codex] {
            for footer in ["esc to interrupt", "Esc to interrupt · working"] {
                assert!(!provider_idle_evidence_agrees(
                    &provider,
                    TuiTurnState::Idle,
                    &format!("Ready for input (type message + Enter)\n{footer}"),
                ));
            }
        }
        assert!(!provider_idle_evidence_agrees(
            &ProviderKind::Claude,
            TuiTurnState::Idle,
            "",
        ));
        assert!(provider_idle_evidence_agrees(
            &ProviderKind::Claude,
            TuiTurnState::Idle,
            READY,
        ));
    }

    #[test]
    fn idle_cleanup_uses_native_provider_state_even_when_relay_is_absent_or_stale() {
        let dir = tempfile::tempdir().unwrap();
        let native = dir.path().join("native.jsonl");
        assert!(!provider_output_is_proven_idle(
            &ProviderKind::Codex,
            &native,
            READY
        ));
        std::fs::write(&native, "").unwrap();
        assert!(!provider_output_is_proven_idle(
            &ProviderKind::Codex,
            &native,
            READY
        ));
        let codex_ready = "›\n  gpt-5-codex · 100% left · ~/project";
        for body in [
            "{\"type\":\"event_msg\",\"payload\":{\"type\":\"task_started\"}}\n",
            "{\"type\":\"event_msg\",\"payload\":{\"type\":\"task_complete\"}}\n{\"type\":",
        ] {
            std::fs::write(&native, body).unwrap();
            let old = std::time::SystemTime::now() - std::time::Duration::from_secs(24 * 60 * 60);
            filetime::set_file_mtime(&native, filetime::FileTime::from_system_time(old)).unwrap();
            assert!(!provider_output_is_proven_idle(
                &ProviderKind::Codex,
                &native,
                codex_ready
            ));
        }
        // A genuinely completed provider can be reclaimed independently of a
        // relay file or persisted inflight. Claude-native turn_duration is
        // terminal evidence; the native assistant/tool envelope is not.
        std::fs::write(
            &native,
            "{\"type\":\"system\",\"subtype\":\"turn_duration\"}\n",
        )
        .unwrap();
        assert!(provider_output_is_proven_idle(
            &ProviderKind::Claude,
            &native,
            READY
        ));
        std::fs::write(
            &native,
            "{\"type\":\"system\",\"subtype\":\"turn_duration\"}\n{\"type\":",
        )
        .unwrap();
        assert!(!provider_output_is_proven_idle(
            &ProviderKind::Claude,
            &native,
            READY
        ));
        std::fs::write(
            &native,
            "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"tool_use\"}]}}\n",
        )
        .unwrap();
        assert!(!provider_output_is_proven_idle(
            &ProviderKind::Claude,
            &native,
            READY
        ));
    }
}

/// Probe one local tmux session without treating probe failure or ambiguous
/// provider output as terminal evidence. Structured JSONL state is authoritative;
/// pane scraping is used only when structured state is unavailable.
pub(crate) fn independent_tmux_readiness(
    tmux_session_name: &str,
    provider: &ProviderKind,
    runtime_kind: Option<RuntimeHandoffKind>,
    output_path: Option<&Path>,
    last_offset: Option<u64>,
) -> IndependentTmuxReadiness {
    match session_presence(tmux_session_name) {
        SessionPresence::Missing => return IndependentTmuxReadiness::Missing,
        SessionPresence::ProbeFailed => return IndependentTmuxReadiness::LiveOrAmbiguous,
        SessionPresence::Present => {}
    }

    let structured = output_path.and_then(|path| {
        crate::services::tui_turn_state::jsonl_ready_for_input(
            provider,
            runtime_kind,
            path,
            last_offset,
        )
    });
    let ready = structured.map(crate::services::tui_turn_state::TuiReadyState::is_ready);
    let ready = ready.or_else(|| {
        crate::services::provider::tmux_session_fallback_ready_for_input(
            tmux_session_name,
            provider,
            runtime_kind,
        )
        .map(crate::services::pane_readiness::FallbackPaneReadiness::is_ready)
    });

    if ready == Some(true) {
        IndependentTmuxReadiness::ReadyForInput
    } else {
        IndependentTmuxReadiness::LiveOrAmbiguous
    }
}
