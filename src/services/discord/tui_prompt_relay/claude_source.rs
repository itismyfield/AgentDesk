//! Finds and publishes Claude sources without changing a running body reader.
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

/// The launch cwd locates transcripts; its mtime excludes older sessions sharing that cwd.
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
    let bound_exists = bound_path.exists();
    // A headless SDK transcript in the same project dir is never this pane's TUI transcript.
    let bound_is_headless = bound_exists
        && crate::services::claude_tui::transcript_tail::claude_transcript_is_headless_sdk(
            &bound_path,
        );
    if bound_exists && !bound_is_headless {
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
            || !super::claude_idle_tail::local_tmux_pane_live(tmux_session_name)
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
    if bound_is_headless {
        return super::headless::claude_tui_transcript_replacing_headless(
            shared,
            tmux_session_name,
            &bound_path,
        );
    }
    // Bound transcript is gone: take the freshest project transcript no other live session claims,
    // unless a restore bound this exact path before it existed and only that file may bind.
    if crate::services::tui_prompt_dedupe::pending::awaits_exact_path(tmux_session_name, binding) {
        return None;
    }
    super::headless::freshest_unclaimed_claude_transcript(shared, tmux_session_name)
}

/// Register the source at the caller's scan position without consuming its prompt.
#[cfg(unix)]
pub(super) fn refresh_claude_runtime_binding(
    tmux_session_name: &str,
    channel_id: ChannelId,
    transcript_path: &Path,
    session_id: Option<String>,
    initial_scan_offset: u64,
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
            last_offset: initial_scan_offset,
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
    resolved_claude_idle_relay_transcript_with_scan_offset(
        shared,
        tmux_session_name,
        channel_id,
        binding,
        |path| Some(claude_tui_rehydrate_start_offset(path)),
    )
}

#[cfg(unix)]
fn resolved_claude_idle_relay_transcript_with_scan_offset(
    shared: &Arc<SharedData>,
    tmux_session_name: &str,
    channel_id: ChannelId,
    binding: &crate::services::tui_prompt_dedupe::TuiRuntimeBinding,
    initial_scan_offset: impl FnOnce(&Path) -> Option<u64>,
) -> Option<PathBuf> {
    let (transcript_path, resolved_session_id) =
        freshest_claude_transcript_for_session(shared, tmux_session_name, binding).or_else(
            || {
                let bound_path = PathBuf::from(&binding.output_path);
                // With no TUI transcript to move to, a headless SDK binding is not tailed at all.
                (!crate::services::claude_tui::transcript_tail::claude_transcript_is_headless_sdk(
                    &bound_path,
                ))
                .then(|| (bound_path, binding.session_id.clone()))
            },
        )?;

    if Path::new(&binding.output_path) != transcript_path {
        refresh_claude_runtime_binding(
            tmux_session_name,
            channel_id,
            &transcript_path,
            resolved_session_id,
            initial_scan_offset(&transcript_path)?,
        );
    } else if transcript_recent_enough_for_binding_refresh(&transcript_path) {
        crate::services::tui_prompt_dedupe::refresh_tmux_runtime_binding_activity(
            tmux_session_name,
            &binding.output_path,
        );
    }
    Some(transcript_path)
}

/// Resolve and register the source; only a watcher reading that source suppresses its idle tail.
#[cfg(unix)]
pub(super) fn resolve_idle_relay_transcript(
    shared: &Arc<SharedData>,
    tmux_session_name: &str,
    channel_id: ChannelId,
    binding: &crate::services::tui_prompt_dedupe::TuiRuntimeBinding,
    allow_watcher_suppression: bool,
) -> Option<PathBuf> {
    resolve_idle_relay_transcript_with_scan_offset(
        shared,
        tmux_session_name,
        channel_id,
        binding,
        allow_watcher_suppression,
        |path| Some(claude_tui_rehydrate_start_offset(path)),
    )
}

#[cfg(unix)]
pub(super) fn resolve_idle_relay_transcript_with_scan_offset(
    shared: &Arc<SharedData>,
    tmux_session_name: &str,
    channel_id: ChannelId,
    binding: &crate::services::tui_prompt_dedupe::TuiRuntimeBinding,
    allow_watcher_suppression: bool,
    initial_scan_offset: impl FnOnce(&Path) -> Option<u64>,
) -> Option<PathBuf> {
    let transcript_path = resolved_claude_idle_relay_transcript_with_scan_offset(
        shared,
        tmux_session_name,
        channel_id,
        binding,
        initial_scan_offset,
    )?;

    if !allow_watcher_suppression {
        return Some(transcript_path);
    }

    // Registration does not retarget a running watcher: compare its own output path.
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
#[path = "claude_source_tests.rs"]
mod tests;
