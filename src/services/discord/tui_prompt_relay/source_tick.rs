//! Source discovery runs even when Legacy body delivery cannot run.
use super::*;
use crate::services::tui_prompt_dedupe::TuiRuntimeBinding;

pub(super) const CLAUDE_IDLE_FRESH_TRANSCRIPT_LOOKBACK_BYTES: u64 = 65_536;

pub(super) struct ClaudeLegacyBodyCandidate {
    pub tmux_session_name: String,
    pub channel_id: ChannelId,
    pub binding: TuiRuntimeBinding,
    pub transcript_path: Option<PathBuf>,
}

pub(super) struct CodexResolvedSource {
    pub tmux_session_name: String,
    pub channel_id: ChannelId,
    pub binding: TuiRuntimeBinding,
    pub rollout_path: PathBuf,
    pub legacy_body: bool,
}

pub(super) async fn claude_rehydrate_source_phase(
    shared: &Arc<SharedData>,
    next_rehydrate: &mut tokio::time::Instant,
) {
    let now = tokio::time::Instant::now();
    if now >= *next_rehydrate {
        // Pane probes block; keep the existing pass on the blocking pool.
        let shared_for_rehydrate = shared.clone();
        let rehydrate_result = tokio::task::spawn_blocking(move || {
            rehydrate_existing_claude_tui_bindings(&shared_for_rehydrate);
        })
        .await;
        if let Err(error) = rehydrate_result {
            tracing::warn!(
                error = %error,
                "Claude TUI binding rehydrate task panicked or was cancelled"
            );
        }
        *next_rehydrate = now + CLAUDE_IDLE_REHYDRATE_POLL_INTERVAL;
    }
}

pub(super) async fn claude_source_phase(
    shared: &Arc<SharedData>,
) -> Vec<ClaudeLegacyBodyCandidate> {
    let mut candidates = Vec::new();
    for (tmux_session_name, binding) in
        super::background_child_completion::claude_bindings_after_child_completions(shared).await
    {
        #[cfg(test)]
        note_source(&tmux_session_name);
        let Some(channel_id) = owner_channel_for_tmux_session(
            shared,
            &ProviderKind::Claude,
            &tmux_session_name,
            RelayEmissionKind::Poll,
        ) else {
            // #3018/#3306/#3656: registry miss ⇒ drop; chokepoint repairs.
            continue;
        };
        let transcript_turns =
            crate::services::tui_o::turn_mode::transcript_turns(channel_id.get());
        // Source publication keeps the existing EOF or bounded last-prompt boundary.
        let transcript_path = super::claude_source::resolve_idle_relay_transcript_with_scan_offset(
            shared,
            &tmux_session_name,
            channel_id,
            &binding,
            !session_bound_discord_delivery_enabled(),
            |path| {
                let eof = claude_tui_rehydrate_start_offset(path);
                if transcript_turns {
                    return Some(eof);
                }
                match scan_claude_idle_transcript_for_last_prompt(
                    path,
                    eof.saturating_sub(CLAUDE_IDLE_FRESH_TRANSCRIPT_LOOKBACK_BYTES),
                ) {
                    Ok(ClaudeIdleTranscriptScan::Prompt {
                        prompt_start_offset,
                        ..
                    }) => Some(prompt_start_offset),
                    Ok(
                        ClaudeIdleTranscriptScan::NoPrompt { offset }
                        | ClaudeIdleTranscriptScan::CompactionReanchor { offset },
                    ) => Some(offset),
                    Err(error) => {
                        tracing::debug!(%error, transcript_path = %path.display(),
                            "deferred Claude source registration until its initial prompt scan succeeds");
                        None
                    }
                }
            },
        );
        if !transcript_turns {
            candidates.push(ClaudeLegacyBodyCandidate {
                tmux_session_name,
                channel_id,
                binding,
                transcript_path,
            });
        }
    }
    candidates
}

pub(super) async fn codex_source_phase(
    shared: &Arc<SharedData>,
    next_rehydrate: &mut tokio::time::Instant,
) -> Vec<CodexResolvedSource> {
    let now = tokio::time::Instant::now();
    if now >= *next_rehydrate {
        let shared_for_rehydrate = shared.clone();
        super::codex_idle_rollout::codex_discovery_pass(shared, move |observer| {
            #[cfg(test)]
            super::codex_idle_rollout::note_relay_target(
                super::codex_idle_rollout::RelayUse::Discovery,
                &shared_for_rehydrate,
            );
            crate::services::tui_prompt_dedupe::resolve_codex_claims();
            rehydrate_existing_codex_tui_bindings(&shared_for_rehydrate, observer)
        })
        .await;
        *next_rehydrate = now + CLAUDE_IDLE_REHYDRATE_POLL_INTERVAL;
    }

    let mut candidates = Vec::new();
    for (tmux_session_name, _) in
        crate::services::tui_prompt_dedupe::runtime_bindings_for_kind(RuntimeHandoffKind::CodexTui)
    {
        #[cfg(test)]
        note_source(&tmux_session_name);
        #[cfg(test)]
        super::codex_idle_rollout::note_poll(
            &tmux_session_name,
            super::codex_idle_rollout::PollNote::Visit,
        );
        #[cfg(test)]
        super::codex_idle_rollout::note_relay_target(
            super::codex_idle_rollout::RelayUse::Owner,
            shared,
        );
        let Some(channel_id) = owner_channel_for_tmux_session(
            shared,
            &ProviderKind::Codex,
            &tmux_session_name,
            RelayEmissionKind::Poll,
        ) else {
            // #3018/#3306/#3656: registry miss ⇒ drop; Codex repair-ineligible.
            continue;
        };
        let Some(binding) = resolved_codex_idle_relay_binding(&tmux_session_name, channel_id)
        else {
            continue;
        };
        let rollout_path = PathBuf::from(&binding.output_path);
        if !rollout_path.exists() {
            tracing::debug!(
                tmux_session_name = %tmux_session_name,
                rollout_path = %rollout_path.display(),
                "codex idle rollout relay skipped missing rollout path"
            );
            continue;
        }
        candidates.push(CodexResolvedSource {
            tmux_session_name,
            channel_id,
            binding,
            rollout_path,
            legacy_body: !crate::services::tui_o::turn_mode::transcript_turns(channel_id.get()),
        });
    }
    candidates
}

// Per-session witnesses keep the task and cadence live while a test disables only body work.
#[cfg(test)]
pub(super) static PHASE_PROBE: Mutex<std::collections::BTreeMap<String, (usize, usize, bool)>> =
    Mutex::new(std::collections::BTreeMap::new());

#[cfg(test)]
fn note_source(tmux: &str) {
    if let Some(counts) = PHASE_PROBE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get_mut(tmux)
    {
        counts.0 += 1;
    }
}

#[cfg(test)]
pub(super) fn skip_body_for_test(tmux: &str) -> bool {
    PHASE_PROBE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get_mut(tmux)
        .is_some_and(|counts| {
            counts.1 += 1;
            counts.2
        })
}
