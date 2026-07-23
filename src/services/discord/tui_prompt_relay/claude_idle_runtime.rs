use super::*;

#[cfg(unix)]
const CLAUDE_CONTINUATION_BOUND_STALE_SECS: u64 = 10 * 60;

#[cfg(unix)]
#[derive(Clone, Debug)]
struct ClaudeContinuationGrowthObservation {
    candidate_path: PathBuf,
    length: u64,
    observed_at: std::time::Instant,
}

#[cfg(unix)]
const CLAUDE_CONTINUATION_GROWTH_WINDOW: Duration = Duration::from_secs(30);

#[cfg(unix)]
static CLAUDE_CONTINUATION_GROWTH: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashMap<String, ClaudeContinuationGrowthObservation>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));

#[cfg(unix)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct ClaudeContinuationAdoptionFacts {
    pub(super) bound_stale_for_threshold: bool,
    pub(super) pane_alive: bool,
    pub(super) candidate_newer_than_bound: bool,
    pub(super) candidate_grew_across_samples: bool,
    pub(super) candidate_starts_after_bound_activity: bool,
}

#[cfg(unix)]
pub(super) fn should_adopt_newer_claude_transcript(facts: ClaudeContinuationAdoptionFacts) -> bool {
    facts.bound_stale_for_threshold
        && facts.pane_alive
        && facts.candidate_newer_than_bound
        && facts.candidate_grew_across_samples
        && facts.candidate_starts_after_bound_activity
}

#[cfg(unix)]
fn observe_claude_continuation_candidate_growth(
    tmux_session_name: &str,
    candidate_path: &Path,
) -> bool {
    let Ok(length) = std::fs::metadata(candidate_path).map(|metadata| metadata.len()) else {
        return false;
    };
    let mut observations = CLAUDE_CONTINUATION_GROWTH
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let now = std::time::Instant::now();
    observations.retain(|_, observation| {
        now.duration_since(observation.observed_at)
            <= CLAUDE_CONTINUATION_GROWTH_WINDOW.saturating_mul(2)
    });
    let grew = observations.get(tmux_session_name).is_some_and(|previous| {
        previous.candidate_path == candidate_path
            && length > previous.length
            && now.duration_since(previous.observed_at) <= CLAUDE_CONTINUATION_GROWTH_WINDOW
    });
    observations.insert(
        tmux_session_name.to_string(),
        ClaudeContinuationGrowthObservation {
            candidate_path: candidate_path.to_path_buf(),
            length,
            observed_at: now,
        },
    );
    grew
}

#[cfg(unix)]
fn clear_claude_continuation_candidate_growth(tmux_session_name: &str) {
    CLAUDE_CONTINUATION_GROWTH
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .remove(tmux_session_name);
}

#[cfg(unix)]
pub(super) fn spawn_claude_idle_transcript_relay(shared: Arc<SharedData>) {
    if CLAUDE_IDLE_TRANSCRIPT_RELAY_STARTED.swap(true, Ordering::AcqRel) {
        return;
    }
    super::super::task_supervisor::spawn_observed("claude_idle_transcript_relay", async move {
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
            for (tmux_session_name, binding) in
                crate::services::tui_prompt_dedupe::runtime_bindings_for_kind(
                    RuntimeHandoffKind::ClaudeTui,
                )
            {
                let Some(channel_id) =
                    owner_channel_for_tmux_session(&shared, &ProviderKind::Claude, &tmux_session_name)
                else {
                    // #3018/#3306/#3656: registry miss ⇒ drop; chokepoint repairs.
                    continue;
                };
                if super::super::inflight::load_inflight_state(&ProviderKind::Claude, channel_id.get())
                    .is_some()
                {
                    continue;
                }

                // #2843: resolve the freshest transcript (re-register stale bound
                // paths) + corrected watcher guard — heartbeat misses stale files.
                let Some(transcript_path) = resolve_idle_relay_transcript(
                    &shared,
                    &tmux_session_name,
                    channel_id,
                    &binding,
                    !session_bound_discord_delivery_enabled(),
                ) else {
                    continue;
                };
                // #3402: restore footer slots a restart wiped while tasks kept
                // running (one-shot per channel+session; footer-mode gated inside).
                shared.ui.placeholder_live_events.rehydrate_slots_once_for_session(
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
                // #4549: `/compact` rewrites the same UUID/path in place. When the
                // current-generation durable frontier and binding cursor are both
                // beyond the new EOF, re-anchor at EOF instead of scanning the
                // compacted historical snapshot from zero. A real rotation changes
                // path/session identity and stays on the bounded newest-prompt
                // lookback below, so fresh replacement-file content is preserved.
                let compaction_reanchor = transcript_eof.and_then(|eof| {
                    claude_idle_compaction_reanchor(
                        path_changed,
                        binding.last_offset,
                        eof,
                        dr::delivered_frontier_exceeds_current_eof(
                            &ProviderKind::Claude,
                            channel_id,
                            &tmux_session_name,
                            eof,
                        ),
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
                            "Claude idle transcript relay fast-forwarded compacted history to current EOF"
                        );
                    }
                    ClaudeIdleTranscriptScan::Prompt {
                        prompt,
                        line_end_offset,
                        entry_id,
                        ..
                    } => {
                        let observed_at = chrono::Utc::now();
                        // #3540: pass the entry's STABLE identity so an
                        // already-relayed prompt re-encountered after a watermark
                        // reset / jsonl head rotation is suppressed by identity
                        // (`SuppressedReplayedEntry`) and never mints a phantom
                        // synthetic inflight. `entry_id == None` falls back to the
                        // content-keyed 30s recent-observed dedup (pre-#3540).
                        let observation =
                            crate::services::tui_prompt_dedupe::observe_prompt_by_tmux_with_entry_id_at(
                                ProviderKind::Claude.as_str(),
                                &tmux_session_name,
                                &prompt,
                                entry_id.as_deref(),
                                observed_at,
                            );
                        tracing::info!(
                            tmux_session_name = %tmux_session_name,
                            channel_id = channel_id.get(),
                            observation = ?observation,
                            entry_id = entry_id.as_deref().unwrap_or(""),
                            "Claude idle transcript relay observed prompt"
                        );
                        advance_claude_tmux_runtime_binding_offset(
                            &tmux_session_name,
                            &transcript_path,
                            line_end_offset,
                        );
                        if !claude_idle_prompt_observation_should_tail_response(observation) {
                            continue;
                        }
                        // #3305/#4033/#4082: use the same injected-prompt decision
                        // that renders the observer note before selecting an
                        // external owner. Local-only slash echoes and neutral compact
                        // continuation records never start a model turn, so they must
                        // not wait for / create a TUI-direct synthetic inflight.
                        let relay_prompt_decision =
                            relay_observed_prompt_injected_prompt_decision(&prompt);
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

            tokio::time::sleep(CODEX_IDLE_ROLLOUT_POLL_INTERVAL).await;
        }
    }
    .instrument(tracing::info_span!(
        "claude_idle_transcript_relay",
        provider = ProviderKind::Claude.as_str(),
        runtime_kind = RuntimeHandoffKind::ClaudeTui.as_str(),
    )));
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

#[cfg(unix)]
pub(super) fn claude_tui_runtime_binding_matches_launch(
    existing: &crate::services::tui_prompt_dedupe::TuiRuntimeBinding,
    fresh: &crate::services::tui_prompt_dedupe::TuiRuntimeBinding,
) -> bool {
    existing.runtime_kind == RuntimeHandoffKind::ClaudeTui
        && existing.output_path == fresh.output_path
        && existing.session_id == fresh.session_id
}

#[cfg(unix)]
pub(super) fn claude_continuation_binding_supersedes_launch(
    existing: &crate::services::tui_prompt_dedupe::TuiRuntimeBinding,
    launch: &crate::services::tui_prompt_dedupe::TuiRuntimeBinding,
) -> bool {
    if existing.runtime_kind != RuntimeHandoffKind::ClaudeTui
        || launch.runtime_kind != RuntimeHandoffKind::ClaudeTui
        || existing.session_id.is_none()
        || launch.session_id.is_none()
        || existing.session_id == launch.session_id
    {
        return false;
    }
    let existing_path = Path::new(&existing.output_path);
    let launch_path = Path::new(&launch.output_path);
    if !existing_path.is_file()
        || !launch_path.is_file()
        || transcript_mtime(existing_path) <= transcript_mtime(launch_path)
    {
        return false;
    }
    let Ok(Some((existing_first, _))) =
        crate::services::claude_tui::transcript_tail::claude_transcript_timestamp_bounds(
            existing_path,
        )
    else {
        return false;
    };
    let Ok(Some((_, launch_last))) =
        crate::services::claude_tui::transcript_tail::claude_transcript_timestamp_bounds(
            launch_path,
        )
    else {
        return false;
    };
    existing_first > launch_last
}

#[cfg(unix)]
pub(super) fn transcript_mtime(path: &Path) -> std::time::SystemTime {
    std::fs::metadata(path)
        .and_then(|meta| meta.modified())
        .unwrap_or(std::time::SystemTime::UNIX_EPOCH)
}

/// #2843: the working directory and launch-script mtime of a Claude TUI session.
/// The working_dir locates the Claude project directory when the stored
/// binding's transcript path is stale; the launch mtime (session start proxy)
/// discriminates this session's transcripts from older sessions' that share the
/// same cwd.
#[cfg(unix)]
pub(in crate::services::discord) fn claude_tui_launch_context(
    tmux_session_name: &str,
) -> Option<(PathBuf, std::time::SystemTime)> {
    let launch_script_path = crate::services::tmux_common::resolve_session_temp_path(
        tmux_session_name,
        crate::services::tmux_common::CLAUDE_TUI_LAUNCH_SCRIPT_TEMP_EXT,
    )?;
    let launch_mtime = transcript_mtime(Path::new(&launch_script_path));
    let launch = parse_claude_tui_launch_script(Path::new(&launch_script_path)).ok()?;
    Some((launch.working_dir, launch_mtime))
}

/// #2843 multi-session fix: transcripts that authoritatively belong to OTHER
/// live Claude TUI tmux sessions (so the freshest scan never steals them).
/// Three sources, unioned:
///   1. The live watcher's `output_path` for each other session — the ground
///      truth of the transcript that session is *currently* tailing, including
///      after Claude rotated its session_id mid-session (the launch script then
///      holds a stale id, so this is the only source that captures the rotated
///      file). This is what fixes concurrent adk-cc threads swapping each
///      other's rotated transcripts.
///   2. Each other session's launch-script transcript — source of truth for
///      SSH-direct sessions that never register a runtime binding or spawn a
///      relay watcher.
///   3. Other sessions' registered runtime bindings — belt-and-suspenders.
#[cfg(unix)]
pub(in crate::services::discord) fn other_session_claimed_transcripts(
    shared: &Arc<SharedData>,
    tmux_session_name: &str,
) -> std::collections::HashSet<PathBuf> {
    let mut claimed: std::collections::HashSet<PathBuf> =
        crate::services::tui_prompt_dedupe::runtime_bindings_for_kind(
            RuntimeHandoffKind::ClaudeTui,
        )
        .into_iter()
        .filter(|(other_session, _)| other_session != tmux_session_name)
        .map(|(_, other_binding)| PathBuf::from(other_binding.output_path))
        .collect();
    for entry in shared.tmux_watchers.iter() {
        if entry.key() == tmux_session_name {
            continue;
        }
        let output_path = entry.value().output_path.clone();
        if !output_path.is_empty() {
            claimed.insert(PathBuf::from(output_path));
        }
    }
    if let Ok(sessions) = crate::services::platform::tmux::list_session_names() {
        for other_session in sessions {
            if other_session == tmux_session_name {
                continue;
            }
            if let Some(other_binding) =
                rehydrated_claude_tui_binding_for_tmux_session(&other_session)
            {
                claimed.insert(PathBuf::from(other_binding.output_path));
            }
        }
    }
    claimed
}

/// #2843: resolve the freshest active Claude transcript for a tmux session.
/// The stored runtime binding's `output_path` can be stale — an older session_id
/// the launch script still references, or a missing AgentDesk rollout jsonl —
/// while the live Claude TUI writes its transcript to a newer `<uuid>.jsonl`
/// under the project directory. Compare the bound path (if it exists) against
/// the newest transcript scanned from the launch-script working_dir and return
/// whichever is newest, plus the session_id (UUID stem) to re-register so future
/// Discord-turn recovery and offset advances reconcile against the right path.
#[cfg(unix)]
pub(super) fn freshest_claude_transcript_for_session(
    shared: &Arc<SharedData>,
    tmux_session_name: &str,
    binding: &crate::services::tui_prompt_dedupe::TuiRuntimeBinding,
) -> Option<(PathBuf, Option<String>)> {
    // Existing paths remain authoritative unless all continuation-cutover
    // evidence agrees. This preserves #2843's shared-cwd anti-steal rule while
    // closing #4423, where the old transcript remains on disk forever after
    // Claude compaction moves the live pane to a new UUID.
    let bound_path = PathBuf::from(&binding.output_path);
    if bound_path.exists() {
        // This function runs in the 500ms idle poll. Keep the ordinary
        // (<10-minute-stale) path to one local stat and return before the
        // blocking tmux/session inventory and project-directory scan.
        let now = std::time::SystemTime::now();
        let bound_mtime = transcript_mtime(&bound_path);
        let bound_stale_for_threshold = now
            .duration_since(bound_mtime)
            .is_ok_and(|age| age.as_secs() >= CLAUDE_CONTINUATION_BOUND_STALE_SECS);
        if !bound_stale_for_threshold {
            return Some((bound_path, binding.session_id.clone()));
        }
        let claimed_by_other_sessions =
            other_session_claimed_transcripts(shared, tmux_session_name);
        let candidate =
            claude_tui_launch_context(tmux_session_name).and_then(|(cwd, launch_mtime)| {
                crate::services::claude_tui::transcript_tail::latest_claude_transcript_for_cwd(
                    &cwd,
                    launch_mtime,
                    None,
                    &claimed_by_other_sessions,
                )
            });
        let Some(candidate_path) = candidate.filter(|candidate| candidate != &bound_path) else {
            return Some((bound_path, binding.session_id.clone()));
        };
        let candidate_mtime = transcript_mtime(&candidate_path);
        if candidate_mtime <= bound_mtime
            || !crate::services::tmux_diagnostics::tmux_session_has_live_pane(tmux_session_name)
        {
            return Some((bound_path, binding.session_id.clone()));
        }
        let candidate_grew_across_samples =
            observe_claude_continuation_candidate_growth(tmux_session_name, &candidate_path);
        if !candidate_grew_across_samples {
            return Some((bound_path, binding.session_id.clone()));
        }
        let candidate_starts_after_bound_activity = match (
            crate::services::claude_tui::transcript_tail::claude_transcript_timestamp_bounds(
                &bound_path,
            ),
            crate::services::claude_tui::transcript_tail::claude_transcript_timestamp_bounds(
                &candidate_path,
            ),
        ) {
            (Ok(Some((_, bound_last))), Ok(Some((candidate_first, _)))) => {
                candidate_first > bound_last
            }
            _ => false,
        };
        let facts = ClaudeContinuationAdoptionFacts {
            bound_stale_for_threshold,
            pane_alive: true,
            candidate_newer_than_bound: candidate_mtime > bound_mtime,
            candidate_grew_across_samples,
            candidate_starts_after_bound_activity,
        };
        if !should_adopt_newer_claude_transcript(facts) {
            return Some((bound_path, binding.session_id.clone()));
        }
        let session_id = candidate_path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .map(str::to_string);
        let Some(candidate_session_id) = session_id.as_deref() else {
            return Some((bound_path, binding.session_id.clone()));
        };
        let Some(bound_session_id) = binding.session_id.as_deref() else {
            return Some((bound_path, binding.session_id.clone()));
        };
        if let Err(error) =
            crate::services::claude_tui::session::persist_claude_continuation_session(
                tmux_session_name,
                candidate_session_id,
            )
        {
            tracing::error!(
                tmux_session_name,
                bound_session_id,
                candidate_session_id,
                error,
                "deferred Claude continuation adoption because durable artifact cutover failed"
            );
            return Some((bound_path, binding.session_id.clone()));
        }
        clear_claude_continuation_candidate_growth(tmux_session_name);
        tracing::warn!(
            tmux_session_name,
            bound_path = %bound_path.display(),
            candidate_path = %candidate_path.display(),
            bound_session_id = binding.session_id.as_deref().unwrap_or(""),
            candidate_session_id = session_id.as_deref().unwrap_or(""),
            bound_stale_secs = now.duration_since(bound_mtime).map(|age| age.as_secs()).unwrap_or(0),
            "adopted growing Claude continuation transcript after bounded two-sample proof"
        );
        return Some((candidate_path, session_id));
    }
    // Bound transcript is gone — fall back to the freshest project transcript,
    // excluding files that authoritatively belong to other live Claude TUI tmux
    // sessions (live watcher path + launch-script transcript + registered
    // binding) so we still never steal another session's transcript.
    let claimed_by_other_sessions = other_session_claimed_transcripts(shared, tmux_session_name);
    claude_tui_launch_context(tmux_session_name)
        .and_then(|(cwd, launch_mtime)| {
            crate::services::claude_tui::transcript_tail::latest_claude_transcript_for_cwd(
                &cwd,
                launch_mtime,
                None,
                &claimed_by_other_sessions,
            )
        })
        .map(|path| {
            let session_id = path
                .file_stem()
                .and_then(|stem| stem.to_str())
                .map(str::to_string);
            (path, session_id)
        })
}

/// #2843: re-register the runtime binding to a freshly-resolved transcript so
/// later reads, offset advances, and Discord-turn recovery all converge on it.
#[cfg(unix)]
pub(super) fn refresh_claude_runtime_binding(
    tmux_session_name: &str,
    channel_id: ChannelId,
    transcript_path: &Path,
    session_id: Option<String>,
) {
    crate::services::tui_prompt_dedupe::register_rehydrated_tmux_runtime_binding(
        ProviderKind::Claude.as_str(),
        tmux_session_name,
        channel_id.get(),
        crate::services::tui_prompt_dedupe::TuiRuntimeBinding {
            runtime_kind: RuntimeHandoffKind::ClaudeTui,
            output_path: transcript_path.display().to_string(),
            relay_output_path: None,
            input_fifo_path: None,
            session_id,
            last_offset: claude_tui_rehydrate_start_offset(transcript_path),
            relay_last_offset: None,
        },
    );
    tracing::info!(
        tmux_session_name = %tmux_session_name,
        channel_id = channel_id.get(),
        transcript_path = %transcript_path.display(),
        "refreshed Claude TUI relay binding to freshest active transcript (#2843)"
    );
}

#[cfg(unix)]
pub(super) fn resolved_claude_idle_relay_transcript_path(
    shared: &Arc<SharedData>,
    tmux_session_name: &str,
    channel_id: ChannelId,
    binding: &crate::services::tui_prompt_dedupe::TuiRuntimeBinding,
) -> Option<PathBuf> {
    let (transcript_path, resolved_session_id) =
        freshest_claude_transcript_for_session(shared, tmux_session_name, binding).unwrap_or_else(
            || {
                (
                    PathBuf::from(&binding.output_path),
                    binding.session_id.clone(),
                )
            },
        );

    if Path::new(&binding.output_path) != transcript_path {
        refresh_claude_runtime_binding(
            tmux_session_name,
            channel_id,
            &transcript_path,
            resolved_session_id,
        );
    } else if transcript_recent_enough_for_binding_refresh(&transcript_path) {
        crate::services::tui_prompt_dedupe::refresh_tmux_runtime_binding_activity(
            tmux_session_name,
            &binding.output_path,
        );
    }
    Some(transcript_path)
}

/// #2843: decide whether the Claude idle relay should tail this session and on
/// which transcript. Returns `Some(path)` to tail, or `None` to skip because a
/// heartbeat-fresh watcher already covers the current transcript. Side effect:
/// re-registers the binding when a fresher transcript is resolved.
///
/// `tmux_session_is_stale` checks only cancel/heartbeat, so a watcher pointed at
/// a missing/stale file reports non-stale and would wrongly suppress relay of
/// direct-TUI output. We only let a non-stale watcher suppress when the binding
/// points at the freshest existing transcript.
#[cfg(unix)]
pub(super) fn resolve_idle_relay_transcript(
    shared: &Arc<SharedData>,
    tmux_session_name: &str,
    channel_id: ChannelId,
    binding: &crate::services::tui_prompt_dedupe::TuiRuntimeBinding,
    allow_watcher_suppression: bool,
) -> Option<PathBuf> {
    let transcript_path =
        resolved_claude_idle_relay_transcript_path(shared, tmux_session_name, channel_id, binding)?;

    if !allow_watcher_suppression {
        return Some(transcript_path);
    }

    // #2843 (codex P0): a relay-live watcher may suppress the idle tail ONLY
    // when the watcher itself is tailing the freshest transcript. Comparing the
    // runtime binding's path is wrong — re-registering the binding does not
    // retarget the running watcher, so the binding can be fresh while the
    // watcher still tails a stale/missing file (then the idle tail would be
    // wrongly suppressed and direct-TUI output lost). Use the watcher's own
    // output path.
    let watcher_covers_current_transcript = shared
        .tmux_watchers
        .tmux_session_live_for_relay(tmux_session_name)
        .is_some_and(|live| live)
        && transcript_path.exists()
        && shared
            .tmux_watchers
            .watcher_output_path(tmux_session_name)
            .is_some_and(|watcher_path| Path::new(&watcher_path) == transcript_path);
    if watcher_covers_current_transcript {
        return None;
    }

    Some(transcript_path)
}

#[cfg(unix)]
fn transcript_recent_enough_for_binding_refresh(path: &Path) -> bool {
    let Ok(metadata) = std::fs::metadata(path) else {
        return false;
    };
    let Ok(modified) = metadata.modified() else {
        return false;
    };
    let age = std::time::SystemTime::now()
        .duration_since(modified)
        .unwrap_or_default();
    age.as_secs()
        < u64::try_from(crate::services::tui_turn_state::STALE_USER_SUBMITTED_RECLAIM_SECS)
            .unwrap_or(0)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn continuation_adoption_requires_every_safety_fact() {
        let all = ClaudeContinuationAdoptionFacts {
            bound_stale_for_threshold: true,
            pane_alive: true,
            candidate_newer_than_bound: true,
            candidate_grew_across_samples: true,
            candidate_starts_after_bound_activity: true,
        };
        assert!(should_adopt_newer_claude_transcript(all));

        for mutate in [
            |facts: &mut ClaudeContinuationAdoptionFacts| facts.bound_stale_for_threshold = false,
            |facts: &mut ClaudeContinuationAdoptionFacts| facts.pane_alive = false,
            |facts: &mut ClaudeContinuationAdoptionFacts| facts.candidate_newer_than_bound = false,
            |facts: &mut ClaudeContinuationAdoptionFacts| {
                facts.candidate_grew_across_samples = false
            },
            |facts: &mut ClaudeContinuationAdoptionFacts| {
                facts.candidate_starts_after_bound_activity = false
            },
        ] {
            let mut facts = all;
            mutate(&mut facts);
            assert!(!should_adopt_newer_claude_transcript(facts));
        }
    }

    #[test]
    fn continuation_candidate_requires_growth_across_two_samples() {
        let tmp = tempfile::tempdir().unwrap();
        let candidate = tmp.path().join("candidate.jsonl");
        std::fs::write(&candidate, b"first\n").unwrap();
        let tmux = format!("AgentDesk-claude-4423-growth-{}", std::process::id());

        assert!(!observe_claude_continuation_candidate_growth(
            &tmux, &candidate
        ));
        assert!(!observe_claude_continuation_candidate_growth(
            &tmux, &candidate
        ));
        use std::io::Write;
        std::fs::OpenOptions::new()
            .append(true)
            .open(&candidate)
            .unwrap()
            .write_all(b"second\n")
            .unwrap();
        assert!(observe_claude_continuation_candidate_growth(
            &tmux, &candidate
        ));
    }

    #[test]
    fn timestamp_continuation_binding_supersedes_stale_launch_binding() {
        let tmp = tempfile::tempdir().unwrap();
        let launch_path = tmp.path().join("old.jsonl");
        let continuation_path = tmp.path().join("new.jsonl");
        std::fs::write(
            &launch_path,
            concat!(
                "{\"timestamp\":\"2026-07-10T00:00:01Z\"}\n",
                "{\"timestamp\":\"2026-07-10T00:00:09Z\"}\n"
            ),
        )
        .unwrap();
        std::fs::write(
            &continuation_path,
            concat!(
                "{\"timestamp\":\"2026-07-10T00:00:10Z\"}\n",
                "{\"timestamp\":\"2026-07-10T00:00:11Z\"}\n"
            ),
        )
        .unwrap();
        filetime::set_file_mtime(
            &launch_path,
            filetime::FileTime::from_unix_time(1_700_000_000, 0),
        )
        .unwrap();
        filetime::set_file_mtime(
            &continuation_path,
            filetime::FileTime::from_unix_time(1_700_000_001, 0),
        )
        .unwrap();
        let binding =
            |path: &Path, session: &str| crate::services::tui_prompt_dedupe::TuiRuntimeBinding {
                runtime_kind: RuntimeHandoffKind::ClaudeTui,
                output_path: path.display().to_string(),
                relay_output_path: None,
                input_fifo_path: None,
                session_id: Some(session.to_string()),
                last_offset: 0,
                relay_last_offset: None,
            };
        let existing = binding(&continuation_path, "new");
        let launch = binding(&launch_path, "old");
        assert!(claude_continuation_binding_supersedes_launch(
            &existing, &launch
        ));

        std::fs::write(
            &continuation_path,
            "{\"timestamp\":\"2026-07-10T00:00:08Z\"}\n",
        )
        .unwrap();
        assert!(!claude_continuation_binding_supersedes_launch(
            &existing, &launch
        ));
    }

    #[test]
    fn transcript_binding_refresh_requires_recent_activity() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let transcript_path = tmp.path().join("claude.jsonl");
        std::fs::write(&transcript_path, b"old transcript\n").expect("write transcript");
        filetime::set_file_mtime(
            &transcript_path,
            filetime::FileTime::from_system_time(
                std::time::SystemTime::now()
                    - std::time::Duration::from_secs(
                        crate::services::tui_turn_state::STALE_USER_SUBMITTED_RECLAIM_SECS as u64
                            + 1,
                    ),
            ),
        )
        .expect("set stale transcript mtime");

        assert!(
            !transcript_recent_enough_for_binding_refresh(&transcript_path),
            "a dead-but-existing Claude transcript must not refresh binding TTL"
        );

        filetime::set_file_mtime(
            &transcript_path,
            filetime::FileTime::from_system_time(std::time::SystemTime::now()),
        )
        .expect("set fresh transcript mtime");
        assert!(transcript_recent_enough_for_binding_refresh(
            &transcript_path
        ));
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
