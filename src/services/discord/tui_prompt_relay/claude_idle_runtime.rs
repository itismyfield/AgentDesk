#[cfg(unix)]
use super::super::recovery_engine::recover_idle_partial_response;
use super::*;

use super::source_tick::CLAUDE_IDLE_FRESH_TRANSCRIPT_LOOKBACK_BYTES;

#[cfg(unix)]
pub(super) fn spawn_claude_idle_transcript_relay(shared: Arc<SharedData>) {
    if CLAUDE_IDLE_TRANSCRIPT_RELAY_STARTED.swap(true, Ordering::AcqRel) {
        return;
    }
    #[cfg(test)]
    RELAY_SPAWNS.fetch_add(1, Ordering::AcqRel);
    super::super::task_supervisor::spawn_observed(
        "claude_idle_transcript_relay",
        async move {
            let mut next_rehydrate = tokio::time::Instant::now();
            loop {
                super::source_tick::claude_rehydrate_source_phase(&shared, &mut next_rehydrate)
                    .await;
                #[cfg(test)]
                RELAY_TICKS.fetch_add(1, Ordering::AcqRel);
                let mut candidates = Vec::new();
                for phase in CLAUDE_IDLE_TICK_PHASES {
                    match phase {
                        ClaudeIdleTickPhase::SettleSessionRotations => {
                            settle_claude_session_rotations(&shared).await;
                        }
                        ClaudeIdleTickPhase::Source => {
                            candidates = super::source_tick::claude_source_phase(&shared).await;
                        }
                        ClaudeIdleTickPhase::LegacyBody => {
                            relay_legacy_claude_bodies(&shared, std::mem::take(&mut candidates))
                                .await;
                        }
                    }
                }

                tokio::time::sleep(CODEX_IDLE_ROLLOUT_POLL_INTERVAL).await;
            }
        }
        .instrument(tracing::info_span!(
            "claude_idle_transcript_relay",
            provider = ProviderKind::Claude.as_str(),
            runtime_kind = RuntimeHandoffKind::ClaudeTui.as_str(),
        )),
    );
}
/// Settle rotations before discovery and inflight recovery so a stranded row cannot block source work.
#[cfg(unix)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ClaudeIdleTickPhase {
    /// Drain/settle whatever a Claude session rotation left behind.
    SettleSessionRotations,
    Source,
    LegacyBody,
}

#[cfg(unix)]
pub(super) const CLAUDE_IDLE_TICK_PHASES: [ClaudeIdleTickPhase; 3] = [
    ClaudeIdleTickPhase::SettleSessionRotations,
    ClaudeIdleTickPhase::Source,
    ClaudeIdleTickPhase::LegacyBody,
];

#[cfg(test)]
pub(super) async fn relay_idle_claude_bindings(shared: &Arc<SharedData>) {
    let candidates = super::source_tick::claude_source_phase(shared).await;
    relay_legacy_claude_bodies(shared, candidates).await;
}

#[cfg(unix)]
async fn relay_legacy_claude_bodies(
    shared: &Arc<SharedData>,
    candidates: Vec<super::source_tick::ClaudeLegacyBodyCandidate>,
) {
    for super::source_tick::ClaudeLegacyBodyCandidate {
        tmux_session_name,
        channel_id,
        binding,
        transcript_path,
    } in candidates
    {
        #[cfg(test)]
        if super::source_tick::skip_body_for_test(&tmux_session_name) {
            continue;
        }
        if let Some(row) =
            super::super::inflight::load_inflight_state(&ProviderKind::Claude, channel_id.get())
        {
            // Recovery offsets belong to the retained row's source, even after rebinding.
            let Some(source) = row.output_path.as_deref().map(Path::new) else {
                continue;
            };
            if let Some(lease) =
                super::synthetic_start::bridge_handoff::resume_unpublished(&shared, &row, source)
                    .await
            {
                spawn_claude_idle_response_tail_once(
                    shared.clone(),
                    tmux_session_name.clone(),
                    channel_id,
                    source.to_path_buf(),
                    row.turn_start_offset.unwrap_or(row.last_offset),
                    row.user_text.clone(),
                    lease,
                );
            } else if let Some(http) = shared.serenity_http_or_token_fallback() {
                recover_idle_partial_response(&http, &shared, &row, source).await;
            }
            continue;
        }

        let Some(transcript_path) = transcript_path else {
            continue;
        };
        let context_path = super::launch_script::claude_launch_observation(&tmux_session_name).1;
        let original_context =
            crate::services::tui_prompt_dedupe::hosted_source::scan_context_from_path(
                "claude",
                &tmux_session_name,
                channel_id.get(),
                context_path.as_deref(),
            );
        // #3402: restore footer slots a restart wiped while tasks kept
        // running (one-shot per channel+session; footer-mode gated inside).
        shared
            .ui
            .placeholder_live_events
            .rehydrate_slots_once_for_session(
                channel_id,
                binding.session_id.as_deref(),
                &transcript_path,
            );
        let path_changed = Path::new(&binding.output_path) != transcript_path;
        let scan_offset = if path_changed {
            // #2843 (codex P1): path changed — scan a bounded lookback
            // instead of starting at EOF so a prompt already written to
            // the freshly-resolved transcript is still found.
            claude_tui_rehydrate_start_offset(&transcript_path)
                .saturating_sub(CLAUDE_IDLE_FRESH_TRANSCRIPT_LOOKBACK_BYTES)
        } else {
            binding.last_offset
        };
        let transcript_eof = std::fs::metadata(&transcript_path)
            .ok()
            .map(|metadata| metadata.len());
        // #4549/#4841: `/compact` rewrites the same UUID/path in place.
        // Re-anchor only to a complete JSONL boundary: a mid-write EOF falls
        // back to zero so the next poll re-reads the entire partial line.
        // Durable same-generation evidence preserves restart detection; a
        // real rotation still uses the bounded newest-prompt lookback below.
        let mut observed_durable_frontier_end = None;
        let compaction_reanchor = transcript_eof.and_then(|eof| {
            let reanchor_offset = claude_idle_safe_reanchor_offset(&transcript_path, eof).ok()?;
            observed_durable_frontier_end = dr::delivered_frontier_exceeding_current_eof(
                &ProviderKind::Claude,
                channel_id,
                &tmux_session_name,
                eof,
            );
            claude_idle_compaction_reanchor(
                path_changed,
                binding.last_offset,
                reanchor_offset,
                observed_durable_frontier_end.is_some(),
            )
        });
        // #2843 (codex round-2 P1): the lookback can hold several finished
        // turns — on a path change select the NEWEST prompt (the just-typed
        // one); unchanged-path tailing keeps first-prompt semantics.
        let scan_result = if let Some(scan) = compaction_reanchor {
            Ok(scan)
        } else if path_changed {
            scan_claude_idle_transcript_for_last_prompt(&transcript_path, scan_offset)
        } else {
            scan_claude_idle_transcript_for_prompt(&transcript_path, scan_offset)
        };
        let scan = match scan_result {
            Ok(scan) => scan,
            Err(error) => {
                tracing::debug!(
                    tmux_session_name = %tmux_session_name,
                    transcript_path = %transcript_path.display(),
                    error = %error,
                    "Claude idle transcript relay scan skipped"
                );
                continue;
            }
        };

        match scan {
            ClaudeIdleTranscriptScan::NoPrompt { offset } => {
                if offset != scan_offset {
                    advance_claude_tmux_runtime_binding_offset(
                        &tmux_session_name,
                        &transcript_path,
                        offset,
                    );
                }
            }
            ClaudeIdleTranscriptScan::CompactionReanchor { offset } => {
                let durable_reanchored = if let Some(expected_frontier_end) =
                    observed_durable_frontier_end
                {
                    match dr::reanchor_current_generation_frontier(
                        &ProviderKind::Claude,
                        channel_id,
                        &tmux_session_name,
                        expected_frontier_end,
                        offset,
                    ) {
                        Ok(true) => true,
                        Ok(false) => {
                            tracing::debug!(
                                tmux_session_name = %tmux_session_name,
                                channel_id = channel_id.get(),
                                transcript_path = %transcript_path.display(),
                                reanchored_offset = offset,
                                expected_frontier_end,
                                "Claude idle transcript relay deferred compaction reanchor after durable frontier changed"
                            );
                            continue;
                        }
                        Err(error) => {
                            tracing::warn!(
                                tmux_session_name = %tmux_session_name,
                                channel_id = channel_id.get(),
                                transcript_path = %transcript_path.display(),
                                reanchored_offset = offset,
                                expected_frontier_end,
                                error = %error,
                                "Claude idle transcript relay deferred compaction reanchor after durable frontier write failed"
                            );
                            continue;
                        }
                    }
                } else {
                    false
                };
                super::super::tmux::reset_stale_relay_watermark_if_output_regressed(
                    shared.as_ref(),
                    channel_id,
                    &tmux_session_name,
                    offset,
                    "claude_idle_compaction_reanchor",
                );
                advance_claude_tmux_runtime_binding_offset(
                    &tmux_session_name,
                    &transcript_path,
                    offset,
                );
                tracing::info!(
                    tmux_session_name = %tmux_session_name,
                    channel_id = channel_id.get(),
                    transcript_path = %transcript_path.display(),
                    previous_offset = binding.last_offset,
                    reanchored_offset = offset,
                    durable_reanchored,
                    "Claude idle transcript relay fast-forwarded compacted history to a complete JSONL boundary"
                );
            }
            ClaudeIdleTranscriptScan::Prompt {
                prompt,
                prompt_start_offset,
                line_end_offset,
                entry_id,
                prompt_id,
                ..
            } => {
                advance_claude_tmux_runtime_binding_offset(
                    &tmux_session_name,
                    &transcript_path,
                    line_end_offset,
                );
                let observed_at = chrono::Utc::now();
                // Row uuid and `promptId` suppress replay; otherwise use the content window.
                let pin = if context_path.is_some() && original_context.is_none() {
                    None
                } else {
                    crate::services::tui_prompt_dedupe::hosted_source::capture_scanned(
                        "claude",
                        &tmux_session_name,
                        channel_id.get(),
                        &transcript_path,
                        original_context.as_ref(),
                        crate::services::tui_prompt_dedupe::hosted_source::claude_record_created_at(
                            &transcript_path,
                            prompt_start_offset,
                        ),
                    )
                };
                let observation =
                    crate::services::tui_prompt_dedupe::hosted_source::observe_claude_record_at(
                        &tmux_session_name,
                        &prompt,
                        (entry_id.as_deref(), prompt_id.as_deref()),
                        observed_at,
                        pin.clone(),
                    );
                tracing::info!(
                    tmux_session_name = %tmux_session_name,
                    channel_id = channel_id.get(),
                    observation = ?observation,
                    entry_id = entry_id.as_deref().unwrap_or(""),
                    prompt_id = prompt_id.as_deref().unwrap_or(""),
                    "Claude idle transcript relay observed prompt"
                );
                if !claude_idle_prompt_observation_should_tail_response(observation) {
                    continue;
                }
                // #3305/#4033/#4082: use the same injected-prompt decision
                // that renders the observer note before selecting an
                // external owner. Local-only slash echoes and neutral compact
                // continuation records never start a model turn, so they must
                // not wait for / create a TUI-direct synthetic inflight.
                let relay_prompt_decision = relay_observed_prompt_injected_prompt_decision(&prompt);
                if !relay_prompt_decision.starts_external_turn_lifecycle() {
                    tracing::info!(
                        tmux_session_name = %tmux_session_name,
                        channel_id = channel_id.get(),
                        injected_class = ?relay_prompt_decision.injected_class,
                        slash_command_kind = relay_prompt_decision.slash_command_kind.as_deref().unwrap_or(""),
                        local_only_slash = relay_prompt_decision.local_only_slash,
                        "Claude idle transcript relay skipped injected prompt with no external-turn lifecycle (no external turn owner / synthetic claim / response tail)"
                    );
                    continue;
                }
                let lease = super::relay_ownership::record_source_external_turn_lease_for_output(
                    &shared,
                    &ProviderKind::Claude,
                    channel_id,
                    &tmux_session_name,
                    binding.runtime_kind,
                    (&transcript_path, observed_at, pin),
                );
                tracing::info!(
                    tmux_session_name = %tmux_session_name,
                    channel_id = channel_id.get(),
                    turn_id = lease.turn_id.as_deref().unwrap_or(""),
                    session_key = lease.session_key.as_deref().unwrap_or(""),
                    relay_owner = lease.relay_owner.as_str(),
                    runtime_kind = lease.runtime_kind.map(RuntimeHandoffKind::as_str).unwrap_or("unknown"),
                    "Claude idle transcript relay selected external turn owner"
                );
                if wait_for_tui_direct_synthetic_non_bridge_claim(
                    &shared.tmux_watchers,
                    &ProviderKind::Claude,
                    channel_id,
                    &tmux_session_name,
                )
                .await
                {
                    tracing::info!(
                        tmux_session_name = %tmux_session_name,
                        channel_id = channel_id.get(),
                        turn_id = lease.turn_id.as_deref().unwrap_or(""),
                        session_key = lease.session_key.as_deref().unwrap_or(""),
                        "Claude idle transcript relay yielded to resolved TUI-direct synthetic non-bridge owner"
                    );
                    continue;
                }
                if bridge_adapter_owns_external_turn(lease.relay_owner) {
                    let tail_spawned = spawn_claude_idle_response_tail_once(
                        shared.clone(),
                        tmux_session_name.clone(),
                        channel_id,
                        transcript_path,
                        line_end_offset,
                        prompt,
                        lease.clone(),
                    );
                    if !tail_spawned {
                        clear_external_input_bridge_lease_if_current(
                            &ProviderKind::Claude,
                            &tmux_session_name,
                            channel_id,
                            &lease,
                        );
                    }
                } else {
                    tracing::debug!(
                        tmux_session_name = %tmux_session_name,
                        channel_id = channel_id.get(),
                        observation = ?observation,
                        relay_owner = lease.relay_owner.as_str(),
                        "Claude idle transcript relay yielded response tail"
                    );
                }
            }
        }
    }
}

/// #3105 (codex P2): the eviction in `evict_dead_orphaned_claude_tui_mirrors` is
/// destructive (it tombstones the dedupe mirror and so removes self-heal), so the
/// liveness check that gates it must be conservative against a TRANSIENT
/// pane-probe flake. We require the "no live pane" verdict to hold across multiple
/// samples (with a short delay between them) — a single negative read must never
/// declare a session dead. `1` would reproduce the original single-sample bug.
#[cfg(unix)]
pub(super) const DEAD_ORPHANED_PANE_PROBE_SAMPLES: usize = 3;

/// Delay between consecutive pane probes. A genuinely-live session that briefly
/// flaked recovers within one of these windows; a genuinely-gone session stays
/// dead across all of them. Kept small so the (rare) eviction path adds at most
/// a few hundred ms to a single rehydrate pass that runs every 5s.
#[cfg(unix)]
pub(super) const DEAD_ORPHANED_PANE_PROBE_DELAY: Duration = Duration::from_millis(75);

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    // Rotation settle must run before source and Legacy inflight work.
    #[test]
    fn the_idle_tick_settles_session_rotations_before_relaying_bindings() {
        assert_eq!(
            CLAUDE_IDLE_TICK_PHASES,
            [
                ClaudeIdleTickPhase::SettleSessionRotations,
                ClaudeIdleTickPhase::Source,
                ClaudeIdleTickPhase::LegacyBody,
            ],
            "the rotation settle phase must run FIRST; the binding relay phase \
             skips panes with a live inflight row, which is exactly the state a \
             session rotation strands"
        );
        let settle = CLAUDE_IDLE_TICK_PHASES
            .iter()
            .position(|phase| *phase == ClaudeIdleTickPhase::SettleSessionRotations)
            .expect("settle phase is dispatched");
        let relay = CLAUDE_IDLE_TICK_PHASES
            .iter()
            .position(|phase| *phase == ClaudeIdleTickPhase::LegacyBody)
            .expect("relay phase is dispatched");
        assert!(
            settle < relay,
            "settle={settle} relay={relay}: reordering these is the #5188 wedge"
        );
    }
}

#[cfg(unix)]
pub(in crate::services::discord) fn resolve_rehydrated_claude_tmux_channel_id(
    tmux_session_name: &str,
) -> Option<u64> {
    resolve_rehydrated_tmux_channel_id(&ProviderKind::Claude, tmux_session_name)
}

/// Attribute panes using existing durable, watcher, configured-name and runtime views.
/// This observes ownership for census and reset guards without publishing a binding.
#[cfg(unix)]
pub(in crate::services::discord) fn resolve_known_tmux_channel_id(
    shared: &SharedData,
    tmux_session_name: &str,
) -> Option<u64> {
    let durable = crate::services::tmux_common::read_tmux_channel_binding(tmux_session_name);
    #[cfg(test)]
    if std::env::var("AGENTDESK_BO_MUTANT").is_ok_and(|value| value == "pane_channel_file_only") {
        return durable;
    }
    durable
        .or_else(|| {
            shared
                .tmux_watchers
                .owner_channel_for_tmux_session(tmux_session_name)
                .map(|channel| channel.get())
        })
        .or_else(|| {
            let (provider, _) =
                crate::services::provider::parse_provider_and_channel_from_tmux_name(
                    tmux_session_name,
                )?;
            resolve_rehydrated_tmux_channel_id(&provider, tmux_session_name)
        })
        .or_else(|| {
            crate::services::tui_prompt_dedupe::owner_channel_for_tmux_session(tmux_session_name)
        })
        .filter(|channel| *channel != 0)
}

#[cfg(unix)]
pub(super) fn resolve_rehydrated_tmux_channel_id(
    provider: &ProviderKind,
    tmux_session_name: &str,
) -> Option<u64> {
    let mut matched: Option<u64> = None;
    for binding in super::super::settings::list_registered_channel_bindings() {
        if &binding.owner_provider != provider {
            continue;
        }
        let channel_id_text = binding.channel_id.to_string();
        let mut segments = vec![channel_id_text.as_str()];
        if let Some(fallback_name) = binding
            .fallback_name
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            segments.push(fallback_name);
        }
        for segment in segments {
            let Some(candidate_channel_id) = rehydrated_channel_id_for_segment(
                provider,
                tmux_session_name,
                segment,
                binding.channel_id,
            ) else {
                continue;
            };
            if matched.is_some_and(|existing| existing != candidate_channel_id) {
                tracing::warn!(
                    tmux_session_name,
                    provider = provider.as_str(),
                    channel_id = candidate_channel_id,
                    existing_channel_id = matched.unwrap_or_default(),
                    "TUI rehydrate skipped ambiguous exact session-name match"
                );
                return None;
            }
            matched = Some(candidate_channel_id);
        }
    }
    matched
}

#[cfg(all(unix, test))]
pub(super) fn rehydrated_claude_channel_id_for_segment(
    tmux_session_name: &str,
    segment: &str,
    parent_channel_id: u64,
) -> Option<u64> {
    rehydrated_channel_id_for_segment(
        &ProviderKind::Claude,
        tmux_session_name,
        segment,
        parent_channel_id,
    )
}

#[cfg(unix)]
pub(super) fn rehydrated_channel_id_for_segment(
    provider: &ProviderKind,
    tmux_session_name: &str,
    segment: &str,
    parent_channel_id: u64,
) -> Option<u64> {
    let base_session_name = provider.build_tmux_session_name(segment);
    if base_session_name == tmux_session_name {
        return Some(parent_channel_id);
    }

    let (session_provider, session_segment) =
        crate::services::provider::parse_provider_and_channel_from_tmux_name(tmux_session_name)?;
    if &session_provider != provider {
        return None;
    }
    let (_base_provider, base_segment) =
        crate::services::provider::parse_provider_and_channel_from_tmux_name(&base_session_name)?;
    let thread_suffix = session_segment
        .strip_prefix(&base_segment)?
        .strip_prefix("-t")?;
    if thread_suffix.is_empty() || !thread_suffix.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    thread_suffix.parse::<u64>().ok()
}

#[cfg(unix)]
pub(super) fn claude_tui_rehydrate_start_offset(transcript_path: &Path) -> u64 {
    std::fs::metadata(transcript_path)
        .map(|metadata| metadata.len())
        .unwrap_or(0)
}

#[cfg(unix)]
pub(super) fn advance_claude_tmux_runtime_binding_offset(
    tmux_session_name: &str,
    transcript_path: &Path,
    offset: u64,
) -> bool {
    crate::services::tui_prompt_dedupe::advance_tmux_runtime_binding_offset(
        tmux_session_name,
        transcript_path.to_str().unwrap_or_default(),
        offset,
    )
}

#[cfg(all(test, unix))]
#[path = "claude_source_idle_tests.rs"]
mod source_idle_tests;

#[cfg(test)]
pub(super) static RELAY_TICKS: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);
#[cfg(test)]
pub(super) static RELAY_SPAWNS: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);
