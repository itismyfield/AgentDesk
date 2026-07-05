//! Manual-rebind recovery path (#3834 decompose split).
//!
//! Behavior-preserving extraction from `recovery_engine.rs`: the third recovery
//! path — manual rebind — i.e. `rebind_inflight_for_channel` and its private
//! support cluster (the `PendingCodexTuiRebindRelay` /
//! `PendingRebindInflightRollback` carriers and the `codex_tui_*` resume-offset /
//! replay-event helpers it drives). They depend only on the parent module's
//! re-exported types and helpers (`RebindOutcome`, `RebindError`, `ProviderKind`,
//! `SharedData`, the `inflight` / `tmux` / `runtime_store` modules, …), pulled in
//! via `use super::*`, so this cluster lives in a leaf module. `RebindOutcome` and
//! `RebindError` stay in the root module because the sibling
//! `recovery_engine/rebind_runtime.rs` and external callers
//! (`health` / `health::recovery`) reference them. `rebind_inflight_for_channel`
//! is re-exported by the root so `recovery_engine::rebind_inflight_for_channel`
//! stays valid for its `health` caller; the rest of the cluster is private to this
//! module. Moved verbatim — zero logic change.

use super::*;

struct PendingCodexTuiRebindRelay {
    rollout_path: String,
    raw_start_offset: u64,
    truncate_relay_output: bool,
    session_id: Option<String>,
    already_relayed_response: String,
    already_normalized_replay_events: Vec<serde_json::Value>,
}

enum PendingRebindInflightRollback {
    RestoreExistingAdoption {
        state: super::inflight::InflightTurnState,
        expected: super::inflight::InflightTurnIdentity,
        expected_turn_start_offset: Option<u64>,
        expected_last_offset_for_rebase: Option<u64>,
    },
    ClearRebindOrigin {
        provider: crate::services::provider::ProviderKind,
        channel_id: u64,
        expected: super::inflight::InflightTurnIdentity,
    },
}

impl PendingRebindInflightRollback {
    fn apply(self) -> String {
        match self {
            Self::RestoreExistingAdoption {
                state,
                expected,
                expected_turn_start_offset,
                expected_last_offset_for_rebase,
            } => {
                let outcome = if let Some(expected_last_offset) = expected_last_offset_for_rebase {
                    super::inflight::save_existing_inflight_rebind_adoption_with_offset_rebase_if_matches_identity(
                        &state,
                        &expected,
                        expected_turn_start_offset,
                        expected_last_offset,
                    )
                } else {
                    super::inflight::save_existing_inflight_rebind_adoption_if_matches_identity(
                        &state,
                        &expected,
                        expected_turn_start_offset,
                    )
                };
                format!("restore_existing_adoption:{outcome:?}")
            }
            Self::ClearRebindOrigin {
                provider,
                channel_id,
                expected,
            } => {
                let outcome =
                    super::inflight::clear_rebind_origin_inflight_state_if_matches_identity(
                        &provider, channel_id, &expected,
                    );
                format!("clear_rebind_origin:{outcome:?}")
            }
        }
    }
}

fn codex_tui_existing_normalized_relay_resume_path(
    tmux_session_name: &str,
    existing_inflight: Option<&super::inflight::InflightTurnState>,
) -> Option<String> {
    let existing = existing_inflight?;
    if existing.runtime_kind != Some(RuntimeHandoffKind::CodexTui) {
        return None;
    }
    let relay_path = crate::services::tmux_common::session_temp_path(tmux_session_name, "jsonl");
    if existing
        .output_path
        .as_deref()
        .is_none_or(|path| std::path::Path::new(path) != std::path::Path::new(&relay_path))
    {
        return None;
    }
    let relay_len = std::fs::metadata(&relay_path).ok()?.len();
    (relay_len > 0).then_some(relay_path)
}

fn codex_tui_rebind_raw_start_offset(
    tmux_session_name: &str,
    rollout_path: &str,
    codex_rollout_resume_offset: Option<u64>,
    codex_rollout_resume_offset_from_marker: bool,
    existing_inflight: Option<&super::inflight::InflightTurnState>,
    synthetic_initial_offset: u64,
    normalized_relay_prompt_replay_start_offset: Option<u64>,
) -> u64 {
    if let Some(existing) = existing_inflight {
        let existing_raw_cursor =
            codex_tui_existing_inflight_raw_cursor(tmux_session_name, rollout_path, existing);
        if codex_rollout_resume_offset_from_marker {
            let marker_offset = codex_rollout_resume_offset
                .or(existing_raw_cursor)
                .unwrap_or(0);
            if let Some(existing_raw_cursor) = existing_raw_cursor {
                return marker_offset.max(existing_raw_cursor);
            }
            return normalized_relay_prompt_replay_start_offset
                .map(|prompt_offset| marker_offset.max(prompt_offset))
                .unwrap_or(marker_offset);
        }
        if let Some(existing_raw_cursor) = existing_raw_cursor {
            return existing_raw_cursor;
        }
        if let Some(resume_offset) = codex_rollout_resume_offset {
            return normalized_relay_prompt_replay_start_offset
                .map(|prompt_offset| resume_offset.max(prompt_offset))
                .unwrap_or(resume_offset);
        }
        return normalized_relay_prompt_replay_start_offset.unwrap_or(0);
    }
    synthetic_initial_offset
}

fn codex_tui_existing_inflight_raw_cursor(
    tmux_session_name: &str,
    rollout_path: &str,
    existing: &super::inflight::InflightTurnState,
) -> Option<u64> {
    let normalized_relay =
        crate::services::tmux_common::session_temp_path(tmux_session_name, "jsonl");
    let output_path = existing.output_path.as_deref()?;
    if std::path::Path::new(output_path) == std::path::Path::new(&normalized_relay) {
        return None;
    }
    if std::path::Path::new(output_path) != std::path::Path::new(rollout_path) {
        return None;
    }
    Some(
        existing
            .last_offset
            .max(existing.turn_start_offset.unwrap_or(0)),
    )
}

fn codex_tui_rebind_prompt_replay_start_offset(
    rollout_path: &str,
    prompt_text: &str,
) -> Option<u64> {
    use std::io::BufRead;

    let prompt_text = prompt_text.trim();
    let file = std::fs::File::open(rollout_path).ok()?;
    let mut reader = std::io::BufReader::new(file);
    let mut offset = 0_u64;
    let mut latest_user_prompt_offset = None;
    let mut latest_matching_prompt_offset = None;
    loop {
        let mut line = Vec::new();
        let read = reader.read_until(b'\n', &mut line).ok()?;
        if read == 0 {
            break;
        }
        offset = offset.saturating_add(read as u64);
        let Ok(value) = serde_json::from_slice::<serde_json::Value>(&line) else {
            continue;
        };
        let Some((candidate, _entry_id)) =
            crate::services::tui_prompt_dedupe::extract_codex_rollout_user_prompt_with_entry_id(
                &value,
            )
        else {
            continue;
        };
        latest_user_prompt_offset = Some(offset);
        if !prompt_text.is_empty()
            && crate::services::tui_prompt_dedupe::prompts_match(prompt_text, &candidate)
        {
            latest_matching_prompt_offset = Some(offset);
        }
    }
    latest_matching_prompt_offset.or(latest_user_prompt_offset)
}

fn codex_tui_existing_inflight_cursor_is_raw_rollout(
    tmux_session_name: &str,
    existing: &super::inflight::InflightTurnState,
) -> bool {
    let normalized_relay =
        crate::services::tmux_common::session_temp_path(tmux_session_name, "jsonl");
    existing
        .output_path
        .as_deref()
        .is_none_or(|path| std::path::Path::new(path) != std::path::Path::new(&normalized_relay))
}

fn codex_tui_rebind_replays_existing_raw_bytes(
    raw_start_offset: u64,
    codex_rollout_resume_offset: Option<u64>,
    synthetic_initial_offset: u64,
) -> bool {
    let replay_boundary = match codex_rollout_resume_offset {
        Some(resume_offset) if resume_offset < raw_start_offset => synthetic_initial_offset,
        Some(resume_offset) => resume_offset,
        None => synthetic_initial_offset,
    };
    raw_start_offset < replay_boundary
}

fn codex_tui_rebind_should_load_existing_normalized_replay_events(
    raw_start_offset: u64,
    replays_existing_raw_bytes: bool,
    normalized_relay_prompt_replay_start_offset: Option<u64>,
    synthetic_initial_offset: u64,
) -> bool {
    if replays_existing_raw_bytes {
        return true;
    }
    if raw_start_offset >= synthetic_initial_offset {
        return false;
    }
    normalized_relay_prompt_replay_start_offset
        .map(|prompt_offset| raw_start_offset <= prompt_offset)
        .unwrap_or(raw_start_offset == 0)
}

fn codex_tui_rebind_already_relayed_response_prefix(
    tmux_session_name: &str,
    rollout_path: &str,
    existing_inflight: Option<&super::inflight::InflightTurnState>,
    raw_start_offset: u64,
    should_suppress_existing_normalized_replay: bool,
    normalized_replay_events_available: bool,
) -> String {
    let Some(existing) = existing_inflight else {
        return String::new();
    };
    if existing.full_response.is_empty() {
        return String::new();
    }

    if let Some(raw_cursor) =
        codex_tui_existing_inflight_raw_cursor(tmux_session_name, rollout_path, existing)
    {
        return if raw_start_offset < raw_cursor {
            existing.full_response.clone()
        } else {
            String::new()
        };
    }

    let normalized_relay =
        crate::services::tmux_common::session_temp_path(tmux_session_name, "jsonl");
    let tracks_normalized_relay = existing
        .output_path
        .as_deref()
        .is_some_and(|path| std::path::Path::new(path) == std::path::Path::new(&normalized_relay));
    if tracks_normalized_relay
        && should_suppress_existing_normalized_replay
        && !normalized_replay_events_available
    {
        return existing.full_response.clone();
    }

    String::new()
}

fn rebind_initial_offset_with_floor(
    initial_offset: u64,
    minimum_initial_offset: Option<u64>,
    output_len: u64,
) -> u64 {
    match minimum_initial_offset {
        Some(floor) if floor > initial_offset && floor <= output_len => floor,
        _ => initial_offset,
    }
}

fn codex_tui_existing_normalized_relay_replay_events(
    relay_path: &str,
    turn_start_offset: Option<u64>,
) -> Vec<serde_json::Value> {
    use std::io::{BufRead, Seek};

    let Some(turn_start_offset) = turn_start_offset else {
        return Vec::new();
    };
    let Ok(file) = std::fs::File::open(relay_path) else {
        return Vec::new();
    };
    let mut reader = std::io::BufReader::new(file);
    if reader
        .seek(std::io::SeekFrom::Start(turn_start_offset))
        .is_err()
    {
        return Vec::new();
    }
    reader
        .lines()
        .filter_map(|line| {
            let line = line.ok()?;
            let line = line.trim();
            (!line.is_empty())
                .then(|| serde_json::from_str::<serde_json::Value>(line).ok())
                .flatten()
        })
        .collect()
}

/// #896: Rebind a live tmux session to a freshly-created inflight state and
/// (re)spawn the output watcher — recovers orphan states whose tmux is alive
/// but whose inflight JSON was cleared, leaving output with no relay path.
///
/// Preconditions (enforced, typed error on violation): tmux session alive
/// (absent ⇒ force-kill + restart instead); no existing inflight for the
/// channel (caller clears first); channel role-map-bound to the provider.
///
/// Side effects on success: writes the provider/channel inflight JSON with
/// `last_offset` = current output size (only NEW output is relayed —
/// retroactive emission is out of scope); registers/refreshes the
/// `DiscordSession`; spawns a `tmux_output_watcher` via the single-watcher
/// claim policy (an existing live owner is reused, `watcher_spawned=false`,
/// and still picks up the new inflight — not an error).
pub(crate) async fn rebind_inflight_for_channel(
    http: &Arc<serenity::Http>,
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: u64,
    tmux_session_override: Option<String>,
) -> Result<RebindOutcome, RebindError> {
    rebind_inflight_for_channel_inner(
        http,
        shared,
        provider,
        channel_id,
        tmux_session_override,
        None,
    )
    .await
}

pub(crate) async fn rebind_inflight_for_channel_with_minimum_start_offset(
    http: &Arc<serenity::Http>,
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: u64,
    tmux_session_override: Option<String>,
    minimum_initial_offset: Option<u64>,
) -> Result<RebindOutcome, RebindError> {
    rebind_inflight_for_channel_inner(
        http,
        shared,
        provider,
        channel_id,
        tmux_session_override,
        minimum_initial_offset,
    )
    .await
}

async fn rebind_inflight_for_channel_inner(
    http: &Arc<serenity::Http>,
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: u64,
    tmux_session_override: Option<String>,
    minimum_initial_offset: Option<u64>,
) -> Result<RebindOutcome, RebindError> {
    let discord_channel_id = ChannelId::new(channel_id);

    // Preflight existence check — fast 409 before walking the validation /
    // tmux-liveness path. Advisory only; the AUTHORITATIVE guard is the atomic
    // `save_inflight_state_create_new` below (`O_CREAT | O_EXCL`), so a live turn
    // winning the race between here and the write cannot be clobbered.
    let existing_inflight = match super::inflight::load_inflight_state(provider, channel_id) {
        Some(existing) => match recovery_phase_for_existing_inflight_rebind(&existing) {
            RecoveryPhase::WatcherReattach => {
                super::inflight::clear_inflight_state(provider, channel_id);
                None
            }
            RecoveryPhase::InflightRestore => Some(existing),
            RecoveryPhase::Pending | RecoveryPhase::Done => {
                return Err(RebindError::InflightAlreadyExists);
            }
        },
        None => None,
    };
    let resuming_existing_inflight = existing_inflight.is_some();

    if resuming_existing_inflight {
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::info!(
            "  [{ts}] ♻ rebind resuming existing inflight turn for channel {} without overwriting canonical state",
            channel_id
        );
    }

    let existing_session_id = existing_inflight
        .as_ref()
        .and_then(|state| state.session_id.clone());
    let existing_saved_output_path = existing_inflight
        .as_ref()
        .and_then(|state| state.output_path.clone());

    // Resolve tmux session name + channel name from the request, falling back
    // to the in-memory session map when no override is provided.
    let (tmux_session_name, channel_name) = match tmux_session_override {
        Some(name) => {
            let ch_name =
                crate::services::provider::parse_provider_and_channel_from_tmux_name(&name)
                    .map(|(_, ch)| ch);
            (name, ch_name)
        }
        None => {
            let ch_name = {
                let data = shared.core.lock().await;
                data.sessions
                    .get(&discord_channel_id)
                    .and_then(|s| s.channel_name.clone())
            };
            let ch_name = match ch_name {
                Some(n) => n,
                None => return Err(RebindError::ChannelNameMissing),
            };
            let tmux = provider.build_tmux_session_name(&ch_name);
            (tmux, Some(ch_name))
        }
    };

    if !tmux_session_alive_with_retry(&tmux_session_name) {
        return Err(RebindError::TmuxNotAlive {
            tmux_session: tmux_session_name,
        });
    }

    // Validate provider↔channel binding against the settings snapshot,
    // mirroring what `restore_inflight_turns` requires for watcher revival.
    let settings_snapshot = shared.settings.read().await.clone();
    let channel_lookup_timeout = std::time::Duration::from_secs(5);
    let is_dm = matches!(
        tokio::time::timeout(channel_lookup_timeout, discord_channel_id.to_channel(http)).await,
        Ok(Ok(serenity::model::channel::Channel::Private(_)))
    );
    let (allowlist_channel_id, provider_channel_name) = match tokio::time::timeout(
        channel_lookup_timeout,
        super::resolve_thread_parent(http, discord_channel_id),
    )
    .await
    {
        Ok(Some((pid, pname))) => (pid, pname.or(channel_name.clone())),
        Ok(None) => (discord_channel_id, channel_name.clone()),
        Err(_) => {
            tracing::warn!(
                channel_id,
                provider = provider.as_str(),
                "rebind channel metadata lookup timed out; falling back to direct channel validation",
            );
            (discord_channel_id, channel_name.clone())
        }
    };
    if validate_bot_channel_routing_with_provider_channel(
        &settings_snapshot,
        provider,
        allowlist_channel_id,
        channel_name.as_deref(),
        provider_channel_name.as_deref(),
        is_dm,
    )
    .is_err()
    {
        return Err(RebindError::ChannelNotBound);
    }

    let runtime_state = resolve_rebind_runtime_state(
        provider,
        &tmux_session_name,
        existing_saved_output_path.as_deref(),
        existing_session_id.clone(),
    )?;
    let mut output_path = runtime_state.output_path;
    let mut synthetic_initial_offset = runtime_state.synthetic_initial_offset;
    let input_fifo_for_state = runtime_state.input_fifo_path;
    let runtime_kind_for_state = runtime_state.runtime_kind;
    let session_id_for_state = runtime_state.session_id;
    let mut force_initial_offset = runtime_state.force_initial_offset;
    let mut existing_offset_rebase_to_output: Option<u64> = runtime_state
        .rebase_existing_offsets_to_output
        .then_some(force_initial_offset.unwrap_or(synthetic_initial_offset));
    let mut pending_codex_tui_rebind_relay: Option<PendingCodexTuiRebindRelay> = None;
    if let Some(rollout_path) = runtime_state.codex_rollout_path.as_deref() {
        let normalized_relay_prompt_replay_start_offset = existing_inflight
            .as_ref()
            .filter(|existing| {
                !codex_tui_existing_inflight_cursor_is_raw_rollout(&tmux_session_name, existing)
            })
            .and_then(|existing| {
                codex_tui_rebind_prompt_replay_start_offset(rollout_path, &existing.user_text)
            });
        let raw_start_offset = codex_tui_rebind_raw_start_offset(
            &tmux_session_name,
            rollout_path,
            runtime_state.codex_rollout_resume_offset,
            runtime_state.codex_rollout_resume_offset_from_marker,
            existing_inflight.as_ref(),
            synthetic_initial_offset,
            normalized_relay_prompt_replay_start_offset,
        );
        let normalized_relay_resume_path = codex_tui_existing_normalized_relay_resume_path(
            &tmux_session_name,
            existing_inflight.as_ref(),
        );
        let replays_existing_raw_bytes = codex_tui_rebind_replays_existing_raw_bytes(
            raw_start_offset,
            runtime_state.codex_rollout_resume_offset,
            synthetic_initial_offset,
        );
        let should_load_existing_normalized_replay_events =
            codex_tui_rebind_should_load_existing_normalized_replay_events(
                raw_start_offset,
                replays_existing_raw_bytes,
                normalized_relay_prompt_replay_start_offset,
                synthetic_initial_offset,
            );
        let already_normalized_replay_events = normalized_relay_resume_path
            .as_deref()
            .filter(|_| should_load_existing_normalized_replay_events)
            .map(|relay_path| {
                codex_tui_existing_normalized_relay_replay_events(
                    relay_path,
                    existing_inflight
                        .as_ref()
                        .and_then(|state| state.turn_start_offset),
                )
            })
            .unwrap_or_default();
        let already_relayed_response = codex_tui_rebind_already_relayed_response_prefix(
            &tmux_session_name,
            rollout_path,
            existing_inflight.as_ref(),
            raw_start_offset,
            should_load_existing_normalized_replay_events,
            !already_normalized_replay_events.is_empty(),
        );
        if let Some(relay_path) = normalized_relay_resume_path {
            output_path = relay_path;
            force_initial_offset = None;
            existing_offset_rebase_to_output = None;
            pending_codex_tui_rebind_relay = Some(PendingCodexTuiRebindRelay {
                rollout_path: rollout_path.to_string(),
                raw_start_offset,
                truncate_relay_output: false,
                session_id: session_id_for_state.clone(),
                already_relayed_response,
                already_normalized_replay_events,
            });
        } else {
            output_path =
                crate::services::tmux_common::session_temp_path(&tmux_session_name, "jsonl");
            pending_codex_tui_rebind_relay = Some(PendingCodexTuiRebindRelay {
                rollout_path: rollout_path.to_string(),
                raw_start_offset,
                truncate_relay_output: true,
                session_id: session_id_for_state.clone(),
                already_relayed_response,
                already_normalized_replay_events: Vec::new(),
            });
            synthetic_initial_offset = 0;
            force_initial_offset = Some(0);
            existing_offset_rebase_to_output = Some(0);
        }
    }

    let initial_offset_without_floor = if let Some(offset) = force_initial_offset {
        offset
    } else if let Some(existing) = existing_inflight.as_ref() {
        let (resume_offset, current_len, truncated) =
            recovery_watcher_start_offset_for_state(&output_path, existing);
        if truncated {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] ↻ rebind restarting existing inflight watcher from 0 for {} (saved offset {}, file len {})",
                tmux_session_name,
                existing.last_offset,
                current_len
            );
        }
        if existing_saved_output_path.as_deref() != Some(output_path.as_str()) {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] ♻ rebind watcher adopted live output path for existing inflight {}: {:?} -> {}",
                tmux_session_name,
                existing_saved_output_path,
                output_path
            );
        }
        resume_offset
    } else {
        synthetic_initial_offset
    };
    let initial_offset = rebind_initial_offset_with_floor(
        initial_offset_without_floor,
        minimum_initial_offset,
        std::fs::metadata(&output_path)
            .map(|metadata| metadata.len())
            .unwrap_or(0),
    );
    if initial_offset != initial_offset_without_floor {
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::info!(
            "  [{ts}] ♻ rebind raised watcher start offset for {} from {} to {} using durable committed relay frontier",
            tmux_session_name,
            initial_offset_without_floor,
            initial_offset
        );
    }

    let mut inflight_rollback_on_relay_setup_failure: Option<PendingRebindInflightRollback>;
    let recovered_state_for_session = if let Some(mut existing) = existing_inflight.clone() {
        let rollback_state = existing.clone();
        let expected = super::inflight::InflightTurnIdentity::from_state(&existing);
        let expected_turn_start_offset = existing.turn_start_offset;
        let expected_last_offset_for_rebase = existing.last_offset;
        existing.tmux_session_name = Some(tmux_session_name.clone());
        existing.output_path = Some(output_path.clone());
        existing.input_fifo_path = input_fifo_for_state.clone();
        if let Some(rebased_last_offset) = existing_offset_rebase_to_output {
            existing.last_offset = rebased_last_offset;
            existing.turn_start_offset = Some(0);
            existing.last_watcher_relayed_offset = None;
            existing.last_watcher_relayed_generation_mtime_ns = None;
        }
        if let Some(runtime_kind) = runtime_kind_for_state {
            existing.runtime_kind = Some(runtime_kind);
        }
        if session_id_for_state.is_some() {
            existing.session_id = session_id_for_state.clone();
        }
        existing.set_relay_owner_kind(super::inflight::RelayOwnerKind::Watcher);
        let rollback_expected = super::inflight::InflightTurnIdentity::from_state(&existing);
        let rollback_expected_turn_start_offset = existing.turn_start_offset;
        let rollback_expected_last_offset_for_rebase =
            existing_offset_rebase_to_output.map(|_| existing.last_offset);
        let save_outcome = if existing_offset_rebase_to_output.is_some() {
            super::inflight::save_existing_inflight_rebind_adoption_with_offset_rebase_if_matches_identity(
                &existing,
                &expected,
                expected_turn_start_offset,
                expected_last_offset_for_rebase,
            )
        } else {
            super::inflight::save_existing_inflight_rebind_adoption_if_matches_identity(
                &existing,
                &expected,
                expected_turn_start_offset,
            )
        };
        if !matches!(save_outcome, super::inflight::GuardedSaveOutcome::Saved) {
            tracing::warn!(
                channel_id,
                tmux_session = %tmux_session_name,
                ?save_outcome,
                "rebind could not persist existing inflight watcher adoption",
            );
            return Err(RebindError::Internal(format!(
                "persist existing inflight watcher adoption for channel {channel_id}: {save_outcome:?}"
            )));
        }
        inflight_rollback_on_relay_setup_failure =
            Some(PendingRebindInflightRollback::RestoreExistingAdoption {
                state: rollback_state,
                expected: rollback_expected,
                expected_turn_start_offset: rollback_expected_turn_start_offset,
                expected_last_offset_for_rebase: rollback_expected_last_offset_for_rebase,
            });
        existing
    } else {
        // Build and persist the new inflight state. No request_owner / msg_ids
        // apply because this recovery has no originating Discord message.
        //
        // #897 counter-model re-review (round 2): flag this as `rebind_origin`
        // so routing / persistence code that keys off "is there a live
        // foreground turn" treats it as absent. This synthetic state exists only
        // to expose a recovered tmux session through inflight APIs; it must not
        // masquerade as a user-authored Discord turn.
        let mut state = super::inflight::InflightTurnState::new(
            provider.clone(),
            channel_id,
            channel_name.clone(),
            0, // request_owner_user_id — no originating Discord user
            0, // user_msg_id
            0, // current_msg_id (placeholder)
            String::from("/api/inflight/rebind"),
            None, // session_id
            Some(tmux_session_name.clone()),
            Some(output_path.clone()),
            input_fifo_for_state.clone(),
            initial_offset,
        );
        state.runtime_kind = runtime_kind_for_state;
        if session_id_for_state.is_some() {
            state.session_id = session_id_for_state.clone();
        }
        state.rebind_origin = true;
        // #2161 Part 2 / #2285 adoption: this synthetic inflight is born when
        // `POST /api/inflight/rebind` adopts a tmux session the operator
        // launched outside AgentDesk (e.g. `tmux new -s <expected>` + run
        // provider manually). Tag as `ExternalAdopted` so audit logs and
        // monitoring surfaces can distinguish "AgentDesk-launched" from
        // "AgentDesk-discovered" sessions. The session-bound relay (epic
        // #2285 E1–E5) routes both identically — this is pure audit
        // metadata.
        state.turn_source = super::inflight::TurnSource::ExternalAdopted;
        // #3582: bind the relay to the watcher we respawn below. The
        // STALL-WATCHDOG force-clean -> respawn path reaches this birth site with
        // `existing_inflight = None` (force-clean deleted the row first); without
        // this stamp the synthetic row defaults to `relay_owner_kind = None` and
        // every later idle-tail / panel / routing check runs the degraded
        // bridge-owned path even though the watcher owns the live tmux relay. The
        // monitor-auto-turn birth site (`tmux.rs`) already stamps Watcher the same
        // way; `rebind_origin` and `relay_owner_kind` are independent flags.
        state.set_relay_owner_kind(super::inflight::RelayOwnerKind::Watcher);
        // #3581: stamp the bounded-preservation fields so an unadopted,
        // never-progressed rebind-origin row can be reaped after a deadline (or
        // a boot-time generation mismatch) instead of becoming a permanent
        // orphan that wedges turn-start. Only this birth site stamps them; the
        // reap predicate (`should_reap_abandoned_rebind_origin`) still requires
        // the row to be owner-less / unadopted / never-progressed, so a row that
        // goes live before the deadline is never reaped.
        state.rebind_origin_created_at_unix = Some(super::inflight::now_unix());
        state.rebind_origin_deadline_secs =
            Some(super::inflight::rebind_origin_deadline_secs_env());
        state.rebind_origin_birth_generation = Some(super::runtime_store::load_generation());

        // Atomic create-or-fail: if a legitimate turn created its inflight file
        // between the preflight check above and this point, the write fails
        // with `AlreadyExists` and we return 409. Without this guard the
        // synthetic rebind state (user_msg_id=0, placeholder ids zeroed) would
        // overwrite the real turn's canonical state and break its completion
        // path — the exact race the #897 P2 #1 review flagged.
        match super::inflight::save_inflight_state_create_new(&state) {
            Ok(()) => {}
            Err(super::inflight::CreateNewInflightError::AlreadyExists) => {
                return Err(RebindError::InflightAlreadyExists);
            }
            Err(super::inflight::CreateNewInflightError::Internal(msg)) => {
                return Err(RebindError::Internal(msg));
            }
        }
        inflight_rollback_on_relay_setup_failure =
            Some(PendingRebindInflightRollback::ClearRebindOrigin {
                provider: provider.clone(),
                channel_id,
                expected: super::inflight::InflightTurnIdentity::from_state(&state),
            });
        state
    };

    if let Some(current_msg_id) = optional_message_id(recovered_state_for_session.current_msg_id) {
        footer_view_reconciler::note_footer_suppressed_for_message_takeover(
            discord_channel_id,
            current_msg_id,
        );
    }

    // Register / refresh the in-memory session so downstream handlers can
    // locate this channel after the rebind.
    {
        let mut data = shared.core.lock().await;
        let session = data
            .sessions
            .entry(discord_channel_id)
            .or_insert_with(|| DiscordSession {
                session_id: existing_session_id.clone(),
                memento_context_loaded: false,
                memento_reflected: false,
                current_path: None,
                history: Vec::new(),
                pending_uploads: Vec::new(),
                cleared: false,
                remote_profile_name: None,
                channel_id: Some(channel_id),
                channel_name: channel_name.clone(),
                category_name: None,
                last_active: tokio::time::Instant::now(),
                worktree: None,
                born_generation: super::runtime_store::load_generation(),
            });
        session.channel_id = Some(channel_id);
        session.last_active = tokio::time::Instant::now();
        if session.channel_name.is_none() {
            session.channel_name = channel_name.clone();
        }
        restore_recovered_session_worktree(session, &recovered_state_for_session);
    }

    let finish_mailbox_on_completion = if existing_inflight.is_some() {
        reregister_active_turn_from_inflight(shared, &recovered_state_for_session).await
    } else {
        false
    };

    // #1135: claim with the single-watcher policy. A live watcher for this
    // same tmux session is reused; a cancelled same-session handle or a
    // different-session channel incumbent is replaced so recovery is not
    // blocked by stale registry state.
    let (watcher_spawned, watcher_replaced) = {
        #[cfg(unix)]
        {
            let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let paused = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let resume_offset = std::sync::Arc::new(std::sync::Mutex::new(None::<u64>));
            let pause_epoch = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
            let turn_delivered = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let last_heartbeat_ts_ms = std::sync::Arc::new(std::sync::atomic::AtomicI64::new(
                super::tmux_watcher_now_ms(),
            ));
            let handle = TmuxWatcherHandle {
                tmux_session_name: tmux_session_name.clone(),
                output_path: output_path.clone(),
                paused: paused.clone(),
                resume_offset: resume_offset.clone(),
                cancel: cancel.clone(),
                pause_epoch: pause_epoch.clone(),
                turn_delivered: turn_delivered.clone(),
                last_heartbeat_ts_ms: last_heartbeat_ts_ms.clone(),
            };
            // `claim_or_reuse_watcher` reuses a live watcher for the same
            // tmux session and only spawns when it claimed or replaced a
            // stale/different-session slot.
            let claim = super::tmux::claim_or_reuse_watcher(
                &shared.tmux_watchers,
                discord_channel_id,
                handle,
                provider,
                "recovery_restore_inflight",
            );
            if claim.should_spawn() {
                if let Some(PendingCodexTuiRebindRelay {
                    rollout_path,
                    raw_start_offset,
                    truncate_relay_output,
                    session_id,
                    already_relayed_response,
                    already_normalized_replay_events,
                }) = pending_codex_tui_rebind_relay.take()
                {
                    let spawned_output_path = match spawn_codex_tui_rebind_relay_output(
                        &tmux_session_name,
                        &rollout_path,
                        raw_start_offset,
                        truncate_relay_output,
                        cancel.clone(),
                        session_id,
                        already_relayed_response,
                        already_normalized_replay_events,
                    ) {
                        Ok(path) => path,
                        Err(error) => {
                            let rolled_back =
                                shared.tmux_watchers.cancel_and_remove_channel_if_current(
                                    &discord_channel_id,
                                    &tmux_session_name,
                                    &output_path,
                                    &cancel,
                                );
                            let inflight_rollback = inflight_rollback_on_relay_setup_failure
                                .take()
                                .map(PendingRebindInflightRollback::apply)
                                .unwrap_or_else(|| "none".to_string());
                            tracing::warn!(
                                tmux_session = %tmux_session_name,
                                output_path = %output_path,
                                rolled_back,
                                inflight_rollback = %inflight_rollback,
                                error = %error,
                                "Codex TUI rebind relay setup failed after watcher claim"
                            );
                            return Err(error);
                        }
                    };
                    debug_assert_eq!(spawned_output_path, output_path);
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    tracing::info!(
                        "  [{ts}] ♻ rebind normalized Codex TUI rollout for {} from {} offset {} into {} ({})",
                        tmux_session_name,
                        rollout_path,
                        raw_start_offset,
                        spawned_output_path,
                        if truncate_relay_output {
                            "truncate"
                        } else {
                            "append"
                        }
                    );
                }
                shared.record_tmux_watcher_reconnect(discord_channel_id);
                let restored_turn = super::tmux::restored_watcher_turn_from_inflight(
                    &recovered_state_for_session,
                    &tmux_session_name,
                    finish_mailbox_on_completion,
                );
                super::task_supervisor::spawn_observed_tmux_watcher(
                    "recovery_restore_inflight_tmux_output_watcher",
                    shared.clone(),
                    tmux_session_name.clone(),
                    cancel.clone(),
                    super::tmux::tmux_output_watcher_with_restore(
                        discord_channel_id,
                        http.clone(),
                        shared.clone(),
                        output_path.clone(),
                        tmux_session_name.clone(),
                        initial_offset,
                        cancel,
                        paused,
                        resume_offset,
                        pause_epoch,
                        turn_delivered,
                        last_heartbeat_ts_ms,
                        restored_turn,
                    ),
                );
            }
            (claim.should_spawn(), claim.replaced_existing())
        }
        #[cfg(not(unix))]
        {
            (false, false)
        }
    };

    Ok(RebindOutcome {
        tmux_session: tmux_session_name,
        channel_id,
        initial_offset,
        watcher_spawned,
        watcher_replaced,
    })
}

#[cfg(test)]
mod post_work_evidence_tests {
    use super::*;
    use crate::services::provider::ProviderKind;
    use std::ffi::OsString;
    use std::path::Path;

    struct EnvGuard {
        key: &'static str,
        previous: Option<OsString>,
    }

    impl EnvGuard {
        fn set_path(key: &'static str, value: &Path) -> Self {
            let previous = std::env::var_os(key);
            unsafe { std::env::set_var(key, value) };
            Self { key, previous }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match &self.previous {
                Some(value) => unsafe { std::env::set_var(self.key, value) },
                None => unsafe { std::env::remove_var(self.key) },
            }
        }
    }

    fn lock_test_env() -> std::sync::MutexGuard<'static, ()> {
        crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }

    #[test]
    fn recovery_input_fifo_requirement_is_runtime_specific() {
        assert_eq!(
            recovery_input_fifo_for_runtime(RuntimeHandoffKind::ClaudeTui, None).unwrap(),
            None
        );
        assert_eq!(
            recovery_input_fifo_for_runtime(RuntimeHandoffKind::CodexTui, None).unwrap(),
            None
        );
        assert!(
            recovery_input_fifo_for_runtime(RuntimeHandoffKind::LegacyTmuxWrapper, None).is_err()
        );
        assert_eq!(
            recovery_input_fifo_for_runtime(
                RuntimeHandoffKind::LegacyTmuxWrapper,
                Some("/tmp/session.input".to_string())
            )
            .unwrap(),
            Some("/tmp/session.input".to_string())
        );
    }

    #[test]
    fn recovery_handoff_preserves_runtime_kind() {
        let handoff = runtime_handoff_for_recovery(
            RuntimeHandoffKind::ClaudeTui,
            "/tmp/claude-transcript.jsonl".to_string(),
            None,
            "AgentDesk-claude-adk".to_string(),
            Some("session-1".to_string()),
            42,
        );

        match handoff {
            RuntimeHandoff::ClaudeTui {
                transcript_path,
                tmux_session_name,
                last_offset,
            } => {
                assert_eq!(transcript_path, "/tmp/claude-transcript.jsonl");
                assert_eq!(tmux_session_name, "AgentDesk-claude-adk");
                assert_eq!(last_offset, 42);
            }
            other => panic!("expected ClaudeTui handoff, got {other:?}"),
        }
    }

    #[test]
    fn rebind_initial_offset_floor_uses_committed_frontier_within_output_len() {
        assert_eq!(
            rebind_initial_offset_with_floor(0, Some(13_400_000), 14_930_326),
            13_400_000,
            "force-clean respawn must not restart from zero when the durable frontier is in-file"
        );
        assert_eq!(
            rebind_initial_offset_with_floor(14_930_326, Some(13_400_000), 14_930_326),
            14_930_326,
            "the floor must never move an already-newer resume offset backward"
        );
        assert_eq!(
            rebind_initial_offset_with_floor(0, Some(13_400_000), 1024),
            0,
            "if the output file was truncated below the durable frontier, keep the boot-path safe restart behavior"
        );
        assert_eq!(rebind_initial_offset_with_floor(512, None, 4096), 512);
    }

    #[test]
    fn codex_tui_rebind_ignores_rollout_resume_offset_without_inflight() {
        assert_eq!(
            codex_tui_rebind_raw_start_offset(
                "AgentDesk-codex-adk-cdx",
                "/tmp/codex-rollout.jsonl",
                Some(12),
                true,
                None,
                128,
                Some(0),
            ),
            128,
            "without an inflight row, stale marker offsets must not replay old Codex output"
        );
    }

    #[test]
    fn codex_tui_rebind_uses_rollout_resume_offset_with_existing_inflight() {
        let _guard = lock_test_env();
        let tmp = tempfile::tempdir().expect("tempdir");
        let _root = EnvGuard::set_path("AGENTDESK_ROOT_DIR", tmp.path());
        let tmux_session_name = "AgentDesk-codex-adk-cdx-existing-inflight";
        let normalized_relay =
            crate::services::tmux_common::session_temp_path(tmux_session_name, "jsonl");

        let mut state = inflight::InflightTurnState::new(
            ProviderKind::Codex,
            42,
            Some("adk-cdx".to_string()),
            123,
            456,
            789,
            "continue deployment".to_string(),
            Some("session-1".to_string()),
            Some(tmux_session_name.to_string()),
            Some("/tmp/codex-rollout.jsonl".to_string()),
            None,
            64,
        );
        state.turn_start_offset = Some(32);

        assert_eq!(
            codex_tui_rebind_raw_start_offset(
                tmux_session_name,
                "/tmp/codex-rollout.jsonl",
                Some(12),
                true,
                Some(&state),
                128,
                None,
            ),
            64,
            "a stale marker must not replay bytes older than the active raw inflight cursor"
        );
        assert_eq!(
            codex_tui_rebind_raw_start_offset(
                tmux_session_name,
                "/tmp/codex-rollout.jsonl",
                Some(96),
                true,
                Some(&state),
                128,
                None,
            ),
            96,
            "a newer marker can still move the raw replay cursor forward"
        );
        assert_eq!(
            codex_tui_rebind_raw_start_offset(
                tmux_session_name,
                "/tmp/codex-rollout.jsonl",
                None,
                false,
                Some(&state),
                128,
                None,
            ),
            64,
            "without a marker offset, existing inflight resumes from its raw cursor candidate"
        );

        state.output_path = Some("/tmp/old-codex-rollout.jsonl".to_string());
        assert_eq!(
            codex_tui_rebind_raw_start_offset(
                tmux_session_name,
                "/tmp/codex-rollout.jsonl",
                Some(12),
                true,
                Some(&state),
                128,
                None,
            ),
            12,
            "raw cursors from a different rollout file must not clamp marker replay forward"
        );
        assert_eq!(
            codex_tui_rebind_raw_start_offset(
                tmux_session_name,
                "/tmp/codex-rollout.jsonl",
                Some(24),
                false,
                Some(&state),
                128,
                None,
            ),
            24,
            "a resolved cursor for the selected rollout remains usable when the persisted raw path changed"
        );
        assert_eq!(
            codex_tui_rebind_raw_start_offset(
                tmux_session_name,
                "/tmp/codex-rollout.jsonl",
                None,
                false,
                Some(&state),
                128,
                None,
            ),
            0,
            "without a cursor for the selected rollout, a stale persisted raw path must replay from the beginning"
        );

        state.output_path = Some(normalized_relay);
        assert_eq!(
            codex_tui_rebind_raw_start_offset(
                tmux_session_name,
                "/tmp/codex-rollout.jsonl",
                Some(12),
                true,
                Some(&state),
                256,
                Some(88),
            ),
            88,
            "a stale marker must not replay bytes older than the current normalized relay prompt boundary"
        );
        assert_eq!(
            codex_tui_rebind_raw_start_offset(
                tmux_session_name,
                "/tmp/codex-rollout.jsonl",
                Some(128),
                true,
                Some(&state),
                256,
                Some(88),
            ),
            128,
            "a marker newer than the prompt boundary remains the replay cursor"
        );
        assert_eq!(
            codex_tui_rebind_raw_start_offset(
                tmux_session_name,
                "/tmp/codex-rollout.jsonl",
                Some(128),
                false,
                Some(&state),
                256,
                None,
            ),
            128,
            "a resolved raw rollout cursor behind EOF must be used even when the inflight row tracks normalized relay bytes"
        );
        assert_eq!(
            codex_tui_rebind_raw_start_offset(
                tmux_session_name,
                "/tmp/codex-rollout.jsonl",
                Some(256),
                false,
                Some(&state),
                256,
                None,
            ),
            256,
            "a rehydrated runtime-binding EOF remains equivalent to the current raw EOF"
        );
        assert_eq!(
            codex_tui_rebind_raw_start_offset(
                tmux_session_name,
                "/tmp/codex-rollout.jsonl",
                None,
                false,
                Some(&state),
                256,
                Some(88),
            ),
            88,
            "legacy markers without raw cursors must replay from the prompt boundary instead of skipping to EOF"
        );
        assert_eq!(
            codex_tui_rebind_raw_start_offset(
                tmux_session_name,
                "/tmp/codex-rollout.jsonl",
                None,
                false,
                Some(&state),
                256,
                None,
            ),
            0,
            "if no prompt boundary can be recovered, replay from the beginning with normalized-event dedupe rather than skipping to EOF"
        );
    }

    #[test]
    fn codex_tui_rebind_prompt_replay_start_offset_prefers_matching_prompt() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let rollout = tmp.path().join("rollout.jsonl");
        let first = serde_json::json!({
            "type": "response_item",
            "payload": {
                "type": "message",
                "role": "user",
                "content": [{"type": "input_text", "text": "old prompt"}],
                "id": "old-user"
            }
        });
        let second = serde_json::json!({
            "type": "response_item",
            "payload": {
                "type": "message",
                "role": "user",
                "content": [{"type": "input_text", "text": "continue deployment"}],
                "id": "current-user"
            }
        });
        let first_line = format!("{first}\n");
        let second_line = format!("{second}\n");
        std::fs::write(&rollout, format!("{first_line}{second_line}")).expect("write rollout");

        assert_eq!(
            codex_tui_rebind_prompt_replay_start_offset(
                rollout.to_str().unwrap(),
                "continue deployment",
            ),
            Some((first_line.len() + second_line.len()) as u64)
        );
    }

    #[test]
    fn codex_tui_rebind_prompt_replay_start_offset_falls_back_to_latest_prompt() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let rollout = tmp.path().join("rollout.jsonl");
        let first = serde_json::json!({
            "type": "response_item",
            "payload": {
                "type": "message",
                "role": "user",
                "content": [{"type": "input_text", "text": "old prompt"}],
                "id": "old-user"
            }
        });
        let second = serde_json::json!({
            "type": "response_item",
            "payload": {
                "type": "message",
                "role": "user",
                "content": [{"type": "input_text", "text": "latest prompt"}],
                "id": "latest-user"
            }
        });
        let first_line = format!("{first}\n");
        let second_line = format!("{second}\n");
        std::fs::write(&rollout, format!("{first_line}{second_line}")).expect("write rollout");

        assert_eq!(
            codex_tui_rebind_prompt_replay_start_offset(rollout.to_str().unwrap(), "missing"),
            Some((first_line.len() + second_line.len()) as u64),
            "when the saved Discord text does not exactly match, the latest user prompt is safer than EOF"
        );
    }

    #[test]
    fn codex_tui_rebind_prefix_disabled_when_raw_cursor_already_skips_relayed_response() {
        let _guard = lock_test_env();
        let tmp = tempfile::tempdir().expect("tempdir");
        let _root = EnvGuard::set_path("AGENTDESK_ROOT_DIR", tmp.path());
        let tmux_session_name = "AgentDesk-codex-adk-cdx-raw-prefix";
        let mut state = inflight::InflightTurnState::new(
            ProviderKind::Codex,
            42,
            Some("adk-cdx".to_string()),
            123,
            456,
            789,
            "continue deployment".to_string(),
            Some("session-1".to_string()),
            Some(tmux_session_name.to_string()),
            Some("/tmp/codex-rollout.jsonl".to_string()),
            None,
            64,
        );
        state.turn_start_offset = Some(32);
        state.full_response = "already relayed".to_string();

        assert_eq!(
            codex_tui_rebind_already_relayed_response_prefix(
                tmux_session_name,
                "/tmp/codex-rollout.jsonl",
                Some(&state),
                64,
                false,
                false,
            ),
            "",
            "when raw tail resumes at the saved cursor, new post-restart output must not be filtered as replay"
        );
    }

    #[test]
    fn codex_tui_rebind_prefix_kept_when_raw_marker_replays_before_saved_cursor() {
        let _guard = lock_test_env();
        let tmp = tempfile::tempdir().expect("tempdir");
        let _root = EnvGuard::set_path("AGENTDESK_ROOT_DIR", tmp.path());
        let tmux_session_name = "AgentDesk-codex-adk-cdx-marker-prefix";
        let mut state = inflight::InflightTurnState::new(
            ProviderKind::Codex,
            42,
            Some("adk-cdx".to_string()),
            123,
            456,
            789,
            "continue deployment".to_string(),
            Some("session-1".to_string()),
            Some(tmux_session_name.to_string()),
            Some("/tmp/codex-rollout.jsonl".to_string()),
            None,
            128,
        );
        state.turn_start_offset = Some(32);
        state.full_response = "already relayed".to_string();

        assert_eq!(
            codex_tui_rebind_already_relayed_response_prefix(
                tmux_session_name,
                "/tmp/codex-rollout.jsonl",
                Some(&state),
                12,
                true,
                false,
            ),
            "already relayed",
            "a marker that restarts before the saved raw cursor must strip already-relayed replay text"
        );
    }

    #[test]
    fn codex_tui_rebind_prefix_disabled_when_raw_path_changed() {
        let _guard = lock_test_env();
        let tmp = tempfile::tempdir().expect("tempdir");
        let _root = EnvGuard::set_path("AGENTDESK_ROOT_DIR", tmp.path());
        let tmux_session_name = "AgentDesk-codex-adk-cdx-stale-raw-prefix";
        let mut state = inflight::InflightTurnState::new(
            ProviderKind::Codex,
            42,
            Some("adk-cdx".to_string()),
            123,
            456,
            789,
            "continue deployment".to_string(),
            Some("session-1".to_string()),
            Some(tmux_session_name.to_string()),
            Some("/tmp/old-codex-rollout.jsonl".to_string()),
            None,
            128,
        );
        state.turn_start_offset = Some(32);
        state.full_response = "already relayed".to_string();

        assert_eq!(
            codex_tui_rebind_already_relayed_response_prefix(
                tmux_session_name,
                "/tmp/codex-rollout.jsonl",
                Some(&state),
                12,
                true,
                false,
            ),
            "",
            "prefix stripping is only safe when the persisted raw cursor belongs to the selected rollout"
        );
    }

    #[test]
    fn codex_tui_rebind_prefix_disabled_when_normalized_relay_resumes_from_current_raw_cursor() {
        let _guard = lock_test_env();
        let tmp = tempfile::tempdir().expect("tempdir");
        let _root = EnvGuard::set_path("AGENTDESK_ROOT_DIR", tmp.path());
        let tmux_session_name = "AgentDesk-codex-adk-cdx-normalized-prefix";
        let normalized_relay =
            crate::services::tmux_common::session_temp_path(tmux_session_name, "jsonl");
        let mut state = inflight::InflightTurnState::new(
            ProviderKind::Codex,
            42,
            Some("adk-cdx".to_string()),
            123,
            456,
            789,
            "continue deployment".to_string(),
            Some("session-1".to_string()),
            Some(tmux_session_name.to_string()),
            Some(normalized_relay),
            None,
            128,
        );
        state.runtime_kind = Some(RuntimeHandoffKind::CodexTui);
        state.full_response = "already relayed".to_string();

        assert_eq!(
            codex_tui_rebind_already_relayed_response_prefix(
                tmux_session_name,
                "/tmp/codex-rollout.jsonl",
                Some(&state),
                512,
                false,
                false,
            ),
            "",
            "normalized relay offsets are not raw rollout cursors, so EOF/current-cursor resumes must not use prefix stripping"
        );
    }

    #[test]
    fn codex_tui_rebind_prefix_disabled_when_normalized_marker_replay_uses_event_dedupe() {
        let _guard = lock_test_env();
        let tmp = tempfile::tempdir().expect("tempdir");
        let _root = EnvGuard::set_path("AGENTDESK_ROOT_DIR", tmp.path());
        let tmux_session_name = "AgentDesk-codex-adk-cdx-normalized-marker-prefix";
        let normalized_relay =
            crate::services::tmux_common::session_temp_path(tmux_session_name, "jsonl");
        let mut state = inflight::InflightTurnState::new(
            ProviderKind::Codex,
            42,
            Some("adk-cdx".to_string()),
            123,
            456,
            789,
            "continue deployment".to_string(),
            Some("session-1".to_string()),
            Some(tmux_session_name.to_string()),
            Some(normalized_relay),
            None,
            128,
        );
        state.runtime_kind = Some(RuntimeHandoffKind::CodexTui);
        state.full_response = "already relayed".to_string();

        assert_eq!(
            codex_tui_rebind_already_relayed_response_prefix(
                tmux_session_name,
                "/tmp/codex-rollout.jsonl",
                Some(&state),
                12,
                true,
                true,
            ),
            "",
            "normalized marker replays must dedupe against existing normalized events, not the whole accumulated response"
        );
    }

    #[test]
    fn codex_tui_rebind_prefix_uses_full_response_when_normalized_replay_events_missing() {
        let _guard = lock_test_env();
        let tmp = tempfile::tempdir().expect("tempdir");
        let _root = EnvGuard::set_path("AGENTDESK_ROOT_DIR", tmp.path());
        let tmux_session_name = "AgentDesk-codex-adk-cdx-empty-normalized-prefix";
        let normalized_relay =
            crate::services::tmux_common::session_temp_path(tmux_session_name, "jsonl");
        let mut state = inflight::InflightTurnState::new(
            ProviderKind::Codex,
            42,
            Some("adk-cdx".to_string()),
            123,
            456,
            789,
            "continue deployment".to_string(),
            Some("session-1".to_string()),
            Some(tmux_session_name.to_string()),
            Some(normalized_relay),
            None,
            128,
        );
        state.runtime_kind = Some(RuntimeHandoffKind::CodexTui);
        state.full_response = "already relayed".to_string();

        assert_eq!(
            codex_tui_rebind_already_relayed_response_prefix(
                tmux_session_name,
                "/tmp/codex-rollout.jsonl",
                Some(&state),
                12,
                true,
                false,
            ),
            "already relayed",
            "when normalized replay events are unavailable, raw replay must strip persisted response text"
        );
    }

    #[test]
    fn codex_tui_rebind_replay_detection_uses_raw_resume_offset_when_available() {
        assert!(
            !codex_tui_rebind_replays_existing_raw_bytes(512, Some(512), 1024),
            "resuming exactly at the saved raw cursor only tails new post-restart bytes"
        );
        assert!(
            codex_tui_rebind_replays_existing_raw_bytes(128, Some(512), 1024),
            "starting before the saved raw cursor replays already-normalized raw bytes"
        );
        assert!(
            codex_tui_rebind_replays_existing_raw_bytes(88, Some(0), 256),
            "a stale marker clamped forward by the prompt boundary still replays existing raw bytes"
        );
        assert!(
            codex_tui_rebind_replays_existing_raw_bytes(128, None, 1024),
            "without a raw cursor, synthetic EOF remains the replay boundary"
        );
        assert!(
            !codex_tui_rebind_replays_existing_raw_bytes(1024, None, 1024),
            "starting at synthetic EOF only tails future bytes"
        );
    }

    #[test]
    fn codex_tui_rebind_loads_normalized_replay_events_for_turn_start_equality() {
        assert!(
            codex_tui_rebind_should_load_existing_normalized_replay_events(0, false, None, 256),
            "raw resume at zero can be a turn-start cursor, so existing normalized events must dedupe replay"
        );
        assert!(
            codex_tui_rebind_should_load_existing_normalized_replay_events(
                88,
                false,
                Some(88),
                256,
            ),
            "raw resume exactly at the prompt boundary can still replay already-normalized assistant output"
        );
        assert!(
            !codex_tui_rebind_should_load_existing_normalized_replay_events(
                512,
                false,
                Some(88),
                1024,
            ),
            "a raw cursor advanced past the prompt boundary should tail post-cursor bytes without old event dedupe"
        );
        assert!(
            !codex_tui_rebind_should_load_existing_normalized_replay_events(
                1024,
                false,
                Some(88),
                1024,
            ),
            "starting at raw EOF does not replay existing bytes"
        );
        assert!(
            codex_tui_rebind_should_load_existing_normalized_replay_events(
                128,
                true,
                Some(88),
                1024,
            ),
            "explicit replay detection always enables existing normalized event dedupe"
        );
    }

    #[test]
    fn codex_tui_existing_normalized_relay_replay_events_start_at_turn_offset() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let relay = tmp.path().join("relay.jsonl");
        let previous_turn = serde_json::json!({"type": "assistant", "content": "same"});
        let current_replay_same = serde_json::json!({"type": "assistant", "content": "same"});
        let current_replay_next = serde_json::json!({"type": "assistant", "content": "next"});
        let previous_line = format!("{previous_turn}\n");
        let current_same_line = format!("{current_replay_same}\n");
        let current_next_line = format!("{current_replay_next}\n");
        std::fs::write(
            &relay,
            format!("{previous_line}{current_same_line}{current_next_line}"),
        )
        .expect("write relay");

        assert_eq!(
            codex_tui_existing_normalized_relay_replay_events(
                relay.to_str().unwrap(),
                Some(previous_line.len() as u64),
            ),
            vec![current_replay_same, current_replay_next],
            "event dedupe must not consume identical events from previous turns"
        );
    }

    #[test]
    fn codex_tui_existing_normalized_relay_replay_events_disabled_without_turn_offset() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let relay = tmp.path().join("relay.jsonl");
        std::fs::write(&relay, "{\"type\":\"assistant\",\"content\":\"same\"}\n")
            .expect("write relay");

        assert!(
            codex_tui_existing_normalized_relay_replay_events(relay.to_str().unwrap(), None)
                .is_empty(),
            "legacy rows without a current-turn offset cannot safely scope normalized-event dedupe"
        );
    }

    #[test]
    fn codex_tui_rebind_reuses_existing_nonempty_normalized_relay_file() {
        let _guard = lock_test_env();
        let tmp = tempfile::tempdir().expect("tempdir");
        let _root = EnvGuard::set_path("AGENTDESK_ROOT_DIR", tmp.path());
        let tmux_session_name = "AgentDesk-codex-adk-cdx-existing-relay";
        let relay_path =
            crate::services::tmux_common::session_temp_path(tmux_session_name, "jsonl");
        std::fs::write(&relay_path, "{\"type\":\"assistant\"}\n").expect("write relay");

        let mut state = inflight::InflightTurnState::new(
            ProviderKind::Codex,
            42,
            Some("adk-cdx".to_string()),
            123,
            456,
            789,
            "continue deployment".to_string(),
            Some("session-1".to_string()),
            Some(tmux_session_name.to_string()),
            Some(relay_path.clone()),
            None,
            0,
        );
        state.runtime_kind = Some(RuntimeHandoffKind::CodexTui);

        assert_eq!(
            codex_tui_existing_normalized_relay_resume_path(tmux_session_name, Some(&state)),
            Some(relay_path),
            "a persisted normalized relay must be replayed instead of truncating and re-tailing raw rollout"
        );
    }

    #[test]
    fn codex_tui_rebind_does_not_reuse_empty_normalized_relay_file() {
        let _guard = lock_test_env();
        let tmp = tempfile::tempdir().expect("tempdir");
        let _root = EnvGuard::set_path("AGENTDESK_ROOT_DIR", tmp.path());
        let tmux_session_name = "AgentDesk-codex-adk-cdx-empty-relay";
        let relay_path =
            crate::services::tmux_common::session_temp_path(tmux_session_name, "jsonl");
        std::fs::write(&relay_path, "").expect("write empty relay");

        let mut state = inflight::InflightTurnState::new(
            ProviderKind::Codex,
            42,
            Some("adk-cdx".to_string()),
            123,
            456,
            789,
            "continue deployment".to_string(),
            Some("session-1".to_string()),
            Some(tmux_session_name.to_string()),
            Some(relay_path),
            None,
            0,
        );
        state.runtime_kind = Some(RuntimeHandoffKind::CodexTui);

        assert_eq!(
            codex_tui_existing_normalized_relay_resume_path(tmux_session_name, Some(&state)),
            None,
            "an empty relay file should let recovery rebuild the normalized stream from raw rollout"
        );
    }

    #[test]
    fn tmux_ready_completion_requires_current_turn_work_evidence() {
        let mut state = inflight::InflightTurnState::new(
            ProviderKind::Codex,
            42,
            Some("adk-cdx".to_string()),
            123,
            456,
            789,
            "background notification".to_string(),
            Some("session-1".to_string()),
            Some("AgentDesk-codex-adk-cdx".to_string()),
            Some("/tmp/out.jsonl".to_string()),
            Some("/tmp/in.input".to_string()),
            64,
        );
        state.task_notification_kind =
            Some(crate::services::agent_protocol::TaskNotificationKind::Background);

        assert!(
            !recovery_has_post_work_ready_evidence(&state),
            "task-notification-only inflight must not trust a stale tmux Ready for input footer"
        );

        state.full_response = "completed".to_string();
        assert!(recovery_has_post_work_ready_evidence(&state));

        state.full_response.clear();
        state.any_tool_used = true;
        assert!(recovery_has_post_work_ready_evidence(&state));

        state.any_tool_used = false;
        state.last_tool_summary = Some("Bash completed".to_string());
        assert!(recovery_has_post_work_ready_evidence(&state));
    }

    /// #3582 regression: the synthetic inflight that `rebind_inflight_for_channel`
    /// creates when `existing_inflight = None` (the STALL-WATCHDOG force-clean ->
    /// respawn path, which deletes the row first) must be stamped watcher-owned.
    /// Before the fix this row defaulted to `relay_owner_kind = None`, so
    /// `effective_relay_owner_kind()` resolved to `None` and every later idle-tail /
    /// panel / routing check ran the degraded bridge-owned path even though the
    /// watcher actually owns the live tmux relay. This mirrors the exact stamps the
    /// birth site applies; if a refactor drops `set_relay_owner_kind(Watcher)` there
    /// this assertion fails.
    #[test]
    fn synthetic_rebind_origin_row_is_watcher_owned() {
        let mut state = inflight::InflightTurnState::new(
            ProviderKind::Claude,
            1_479_671_298_497_183_835,
            Some("adk-cc".to_string()),
            0, // request_owner_user_id — no originating Discord user
            0, // user_msg_id
            0, // current_msg_id (placeholder)
            String::from("/api/inflight/rebind"),
            None,
            Some("AgentDesk-claude-adk-cc".to_string()),
            Some("/tmp/out.jsonl".to_string()),
            None,
            0,
        );
        // Birth-site stamps (must stay in sync with `rebind_inflight_for_channel`).
        state.rebind_origin = true;
        state.turn_source = inflight::TurnSource::ExternalAdopted;
        state.set_relay_owner_kind(inflight::RelayOwnerKind::Watcher);

        assert_eq!(
            state.effective_relay_owner_kind(),
            inflight::RelayOwnerKind::Watcher,
            "force-clean respawn must leave the relay watcher-owned, not degraded to None",
        );
        assert!(
            state.watcher_owns_live_relay,
            "the legacy bool must agree so older deserializers also see watcher ownership",
        );
        // rebind_origin and watcher ownership are independent flags and must coexist.
        assert!(state.rebind_origin);
    }
}
