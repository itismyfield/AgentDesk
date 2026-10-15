#[cfg(unix)]
use super::super::recovery_engine::recover_idle_partial_response;
use super::*;

/// #2843: when the background idle relay loop discovers that a session's
/// transcript path changed, scan this many bytes back from EOF (not from EOF
/// itself) so a prompt already written to the freshly-resolved transcript is
/// still observed and its response relayed.
const CLAUDE_IDLE_FRESH_TRANSCRIPT_LOOKBACK_BYTES: u64 = 65_536;

#[cfg(unix)]
pub(super) fn spawn_claude_idle_transcript_relay(shared: Arc<SharedData>) {
    if CLAUDE_IDLE_TRANSCRIPT_RELAY_STARTED.swap(true, Ordering::AcqRel) {
        return;
    }
    super::super::task_supervisor::spawn_observed(
        "claude_idle_transcript_relay",
        async move {
            let mut next_rehydrate = tokio::time::Instant::now();
            loop {
                let now = tokio::time::Instant::now();
                if now >= next_rehydrate {
                    // #3105 (codex P2): `rehydrate_existing_claude_tui_bindings` is a
                    // fully BLOCKING pass (synchronous `tmux` subprocess calls + a
                    // `std::thread::sleep` between multi-sample pane probes); running it
                    // inline would stall the executor for samples×delay plus tmux latency.
                    // Move it onto the blocking pool via `spawn_blocking` (the sync core
                    // and its unit-testable logic are unchanged).
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
                    next_rehydrate = now + CLAUDE_IDLE_REHYDRATE_POLL_INTERVAL;
                }
                // #5188 (R2): the tick's two phases, dispatched in the declared
                // order. See `CLAUDE_IDLE_TICK_PHASES` for why the order is an
                // invariant rather than a coincidence of statement placement.
                for phase in CLAUDE_IDLE_TICK_PHASES {
                    match phase {
                        ClaudeIdleTickPhase::SettleSessionRotations => {
                            settle_claude_session_rotations(&shared).await;
                        }
                        ClaudeIdleTickPhase::RelayIdleBindings => {
                            relay_idle_claude_bindings(&shared).await;
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
/// #5188 (R2): the ordered phases of one Claude idle tick.
///
/// The ORDER is a correctness invariant, not an accident of statement placement.
/// [`ClaudeIdleTickPhase::RelayIdleBindings`] skips any pane that still holds an
/// inflight row, and a pane stranded by a session rotation is exactly a pane
/// holding an inflight row that can never be finalized. Settling second would
/// therefore make every tick observe the wedged pane, skip it, and only then
/// unwedge it — so the pane that the tick was looking at is never the pane the
/// tick can help.
///
/// Declared as data so the ordering is assertable; the dispatch in
/// `spawn_claude_idle_transcript_relay` is the only place either phase runs.
#[cfg(unix)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ClaudeIdleTickPhase {
    /// Drain/settle whatever a Claude session rotation left behind.
    SettleSessionRotations,
    /// Relay the idle transcript tail of every live ClaudeTui binding.
    RelayIdleBindings,
}

#[cfg(unix)]
pub(super) const CLAUDE_IDLE_TICK_PHASES: [ClaudeIdleTickPhase; 2] = [
    ClaudeIdleTickPhase::SettleSessionRotations,
    ClaudeIdleTickPhase::RelayIdleBindings,
];

/// The [`ClaudeIdleTickPhase::RelayIdleBindings`] phase: walk every ClaudeTui
/// runtime binding, discover its source, then relay its idle transcript tail.
///
/// Moved verbatim out of the tick body so the two phases are separately named
/// and their order is expressible as data. A live inflight row skips body
/// scanning after discovery, so the preceding settle phase remains load-bearing.
#[cfg(unix)]
pub(super) async fn relay_idle_claude_bindings(shared: &Arc<SharedData>) {
    for (tmux_session_name, binding) in
        super::background_child_completion::claude_bindings_after_child_completions(shared).await
    {
        let Some(channel_id) = owner_channel_for_tmux_session(
            &shared,
            &ProviderKind::Claude,
            &tmux_session_name,
            RelayEmissionKind::Poll,
        ) else {
            // #3018/#3306/#3656: registry miss ⇒ drop; chokepoint repairs.
            continue;
        };
        let transcript_turns =
            crate::services::tui_o::turn_mode::transcript_turns(channel_id.get());
        // Keep an unconsumed Legacy prompt available after the original inflight finishes.
        let transcript_path = super::claude_source::resolve_idle_relay_transcript_with_scan_offset(
            &shared,
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
        if let Some(row) =
            super::super::inflight::load_inflight_state(&ProviderKind::Claude, channel_id.get())
            && (!transcript_turns || !super::super::inflight::is_synthetic_create_state(&row))
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
                if transcript_turns {
                    continue;
                }
                let observed_at = chrono::Utc::now();
                // Row uuid and `promptId` suppress replay; otherwise use the content window.
                let observation =
                    crate::services::tui_prompt_dedupe::observe_prompt_by_tmux_with_row_ids_at(
                        ProviderKind::Claude.as_str(),
                        &tmux_session_name,
                        &prompt,
                        entry_id.as_deref(),
                        prompt_id.as_deref(),
                        observed_at,
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
                let lease = record_external_turn_lease_for_output(
                    &shared,
                    &ProviderKind::Claude,
                    channel_id,
                    &tmux_session_name,
                    binding.runtime_kind,
                    &transcript_path,
                    observed_at,
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

    /// #5188 (R2). The settle phase must lead the tick.
    ///
    /// `RelayIdleBindings` `continue`s past any pane holding an inflight row,
    /// and a rotation-stranded pane holds an inflight row that can never be
    /// finalized. Settling afterwards still unwedges the pane eventually, which
    /// is why a purely positional version of this rule changed no observable
    /// behaviour and no test noticed when the call moved — but it means the tick
    /// that SEES the wedge is never the tick that clears it, and the relay work
    /// for that pane is deferred a full poll every time.
    #[test]
    fn the_idle_tick_settles_session_rotations_before_relaying_bindings() {
        assert_eq!(
            CLAUDE_IDLE_TICK_PHASES,
            [
                ClaudeIdleTickPhase::SettleSessionRotations,
                ClaudeIdleTickPhase::RelayIdleBindings,
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
            .position(|phase| *phase == ClaudeIdleTickPhase::RelayIdleBindings)
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
