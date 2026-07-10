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

use super::manual_rebind_output_path::saved_output_path_for_rebind_resolution;
use super::manual_rebind_override::upsert_rebind_session_id_override;
use super::*;

mod adoption;
mod codex_tui_replay;

pub(crate) use self::adoption::{
    claude_tui_force_initial_offset_for_adopted_transcript,
    claude_tui_rebind_should_reregister_runtime_binding, rebind_initial_offset_with_floor,
    rebind_initial_offset_with_floor_unless_forced, rebind_output_paths_same,
};
pub(crate) use self::codex_tui_replay::{
    PendingCodexTuiRebindRelay, codex_tui_existing_inflight_cursor_is_raw_rollout,
    codex_tui_existing_inflight_raw_cursor, codex_tui_existing_normalized_relay_replay_events,
    codex_tui_existing_normalized_relay_resume_path,
    codex_tui_rebind_already_relayed_response_prefix, codex_tui_rebind_prompt_replay_start_offset,
    codex_tui_rebind_raw_start_offset, codex_tui_rebind_replays_existing_raw_bytes,
    codex_tui_rebind_should_load_existing_normalized_replay_events,
};

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
    overrides: ManualRebindOverrides,
) -> Result<RebindOutcome, RebindError> {
    rebind_inflight_for_channel_inner(
        http,
        shared,
        provider,
        channel_id,
        tmux_session_override,
        overrides,
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
        ManualRebindOverrides::default(),
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
    overrides: ManualRebindOverrides,
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

    let existing_session_id = overrides.session_id().map(str::to_string).or_else(|| {
        existing_inflight
            .as_ref()
            .and_then(|state| state.session_id.clone())
    });
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

    upsert_rebind_session_id_override(shared, provider, &tmux_session_name, overrides.session_id())
        .await?;

    let existing_saved_output_path_for_resolution = saved_output_path_for_rebind_resolution(
        shared,
        provider,
        existing_saved_output_path.as_deref(),
        existing_session_id.as_deref(),
        &tmux_session_name,
        overrides.output_path(),
    )
    .await;
    let runtime_state =
        match overrides.runtime_state(provider, &tmux_session_name, existing_session_id.clone())? {
            Some(runtime_state) => runtime_state,
            None => resolve_rebind_runtime_state(
                provider,
                &tmux_session_name,
                existing_saved_output_path_for_resolution.as_deref(),
                existing_session_id.clone(),
            )?,
        };
    let mut output_path = runtime_state.output_path;
    let mut synthetic_initial_offset = runtime_state.synthetic_initial_offset;
    let input_fifo_for_state = runtime_state.input_fifo_path;
    let runtime_kind_for_state = runtime_state.runtime_kind;
    let session_id_for_state = runtime_state.session_id;
    let mut force_initial_offset = runtime_state.force_initial_offset;
    let mut forced_adopted_transcript_rebase_offset = None;
    if force_initial_offset.is_none()
        && let Some(offset) = claude_tui_force_initial_offset_for_adopted_transcript(
            runtime_kind_for_state,
            existing_inflight.as_ref(),
            &output_path,
            synthetic_initial_offset,
        )
    {
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::info!(
            "  [{ts}] ♻ rebind starting adopted Claude transcript at EOF for {}: {:?} -> {} (offset {})",
            tmux_session_name,
            existing_saved_output_path,
            output_path,
            offset
        );
        force_initial_offset = Some(offset);
        forced_adopted_transcript_rebase_offset = Some(offset);
    }
    let mut existing_offset_rebase_to_output: Option<u64> = forced_adopted_transcript_rebase_offset
        .or_else(|| {
            runtime_state
                .rebase_existing_offsets_to_output
                .then_some(force_initial_offset.unwrap_or(synthetic_initial_offset))
        });
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
    let output_len_for_floor = if force_initial_offset.is_some() {
        // A forced initial offset is already expressed in the coordinate space
        // the watcher must use. In the Codex-TUI rebuild path we deliberately
        // set it to 0 before `spawn_codex_tui_rebind_relay_output` truncates the
        // normalized relay file; applying a durable floor here would mix the old
        // pre-truncation file coordinates into the new zero-based relay stream.
        // This exemption also covers remembered retry floors, which arrive
        // through `minimum_initial_offset`.
        None
    } else {
        Some(
            std::fs::metadata(&output_path)
                .map(|metadata| metadata.len())
                .unwrap_or(0),
        )
    };
    let initial_offset = rebind_initial_offset_with_floor_unless_forced(
        initial_offset_without_floor,
        minimum_initial_offset,
        output_len_for_floor,
        force_initial_offset,
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
            existing.turn_start_offset = Some(rebased_last_offset);
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
        if session_id_for_state.is_some() {
            session.session_id = session_id_for_state.clone();
        }
        restore_recovered_session_worktree(session, &recovered_state_for_session);
    }

    let finish_mailbox_on_completion = if existing_inflight.is_some() {
        reregister_active_turn_from_inflight(shared, &recovered_state_for_session).await
    } else {
        false
    };

    if claude_tui_rebind_should_reregister_runtime_binding(runtime_kind_for_state, &output_path) {
        crate::services::tui_prompt_dedupe::register_rehydrated_tmux_runtime_binding(
            provider.as_str(),
            &tmux_session_name,
            channel_id,
            crate::services::tui_prompt_dedupe::TuiRuntimeBinding {
                runtime_kind: RuntimeHandoffKind::ClaudeTui,
                output_path: output_path.clone(),
                relay_output_path: None,
                input_fifo_path: None,
                session_id: session_id_for_state.clone(),
                last_offset: initial_offset,
                relay_last_offset: None,
            },
        );
    }

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
    use crate::services::agent_protocol::{RuntimeHandoff, RuntimeHandoffKind};
    use crate::services::discord::inflight;
    use crate::services::provider::ProviderKind;

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

#[cfg(test)]
mod stall_watchdog_respawn_deadlock_tests {
    //! #4400 (b): the 16:32Z self-deadlock — force-clean deleted the row, the
    //! dying watcher's last poll re-minted a zero-id synthetic row via the
    //! #3107 self-heal, and every subsequent watchdog respawn tick died on this
    //! module's preflight with `InflightAlreadyExists` because that shape
    //! classified as `Pending`. These tests pin both the adoption fix and the
    //! untouched row-absent (07-07) create-new path.
    use super::*;
    use crate::services::provider::ProviderKind;

    /// 16:32Z incident reproduction: with the re-minted orphan row persisted,
    /// the respawn preflight must route onto the `InflightRestore` resume arm
    /// (no 409 — invariant I1) and the resume machinery must start the watcher
    /// at the row's committed offset so the backlog written while the watcher
    /// was dead (the 16:30~16:37Z window) is still relayed (invariant I3).
    /// Mutation kill: reverting the `can_adopt_orphaned_synthetic_watcher_row`
    /// arm classifies this row `Pending` and the first assert fails.
    #[test]
    fn respawn_preflight_adopts_reacquired_orphan_row_instead_of_409() {
        let _lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let tmp = tempfile::tempdir().expect("tempdir");
        unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", tmp.path()) };

        let provider = ProviderKind::Claude;
        let channel_id = 1_479_671_298_497_183_835_u64;
        let output_path = tmp.path().join("claude-transcript.jsonl");
        let output_path_str = output_path.display().to_string();
        // 8_192 committed bytes plus 4_096 backlog bytes produced while the
        // watcher was dead — resume must start AT the committed offset, not at
        // EOF (which would drop the backlog) and not at 0 (rebase).
        std::fs::write(&output_path, vec![b'x'; 12_288]).expect("write transcript");

        // The row exactly as `reacquire_watcher_inflight_for_active_stream`
        // (#3107 self-heal) re-mints it after force-clean deleted the real row,
        // persisted through the same atomic if-absent path the self-heal uses.
        let mut orphan = super::inflight::InflightTurnState::new(
            provider.clone(),
            channel_id,
            None,
            0,                         // request_owner_user_id — headless re-acquire
            0,                         // user_msg_id
            1_518_888_000_000_000_001, // current_msg_id — surviving placeholder message
            String::new(),
            None,
            Some("AgentDesk-claude-adk-cc".to_string()),
            Some(output_path_str.clone()),
            None,
            8_192,
        );
        orphan.turn_source = super::inflight::TurnSource::ExternalInput;
        orphan.set_relay_owner_kind(super::inflight::RelayOwnerKind::Watcher);
        assert!(
            super::inflight::save_inflight_state_if_absent(&orphan).expect("persist orphan row"),
            "the self-heal if-absent write must land on an empty store"
        );

        let existing = super::inflight::load_inflight_state(&provider, channel_id)
            .expect("re-minted orphan row must load");
        assert_eq!(
            recovery_phase_for_existing_inflight_rebind(&existing),
            RecoveryPhase::InflightRestore,
            "respawn must adopt the re-minted orphan row onto the resume path instead of \
             returning InflightAlreadyExists on every watchdog tick (I1)"
        );

        let (resume_offset, current_len, truncated) =
            recovery_watcher_start_offset_for_state(&output_path_str, &existing);
        assert_eq!(
            resume_offset, 8_192,
            "resume must preserve the row's committed offset — the dead-window backlog \
             (bytes 8_192..12_288) stays relayable (I3)"
        );
        assert_eq!(current_len, 12_288);
        assert!(!truncated, "a grown live file is not a truncation restart");
    }

    /// 07-07 regression pin: when force-clean actually deleted the row and no
    /// self-heal re-minted one (row ABSENT at respawn), the preflight finds no
    /// existing inflight and the synthetic rebind-origin birth path must still
    /// create the row atomically — the adoption fix only reroutes rows that
    /// exist.
    #[test]
    fn respawn_with_absent_row_still_creates_new_synthetic_inflight() {
        let _lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let tmp = tempfile::tempdir().expect("tempdir");
        unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", tmp.path()) };

        let provider = ProviderKind::Claude;
        let channel_id = 1_479_671_298_497_184_007_u64;
        assert!(
            super::inflight::load_inflight_state(&provider, channel_id).is_none(),
            "preflight must observe no existing inflight (the 07-07 shape)"
        );

        // Birth-site mirror of the `existing_inflight = None` branch of
        // `rebind_inflight_for_channel`.
        let mut state = super::inflight::InflightTurnState::new(
            provider.clone(),
            channel_id,
            Some("adk-cc".to_string()),
            0,
            0,
            0,
            String::from("/api/inflight/rebind"),
            None,
            Some("AgentDesk-claude-adk-cc".to_string()),
            Some("/tmp/out.jsonl".to_string()),
            None,
            0,
        );
        state.rebind_origin = true;
        state.turn_source = super::inflight::TurnSource::ExternalAdopted;
        state.set_relay_owner_kind(super::inflight::RelayOwnerKind::Watcher);

        assert!(
            super::inflight::save_inflight_state_create_new(&state).is_ok(),
            "row-absent respawn must keep succeeding through the atomic create-new path"
        );
        assert!(
            super::inflight::load_inflight_state(&provider, channel_id).is_some(),
            "the synthetic rebind-origin row must be persisted"
        );
    }

    /// #4400 (b) review r2: full-path 16:32Z reproduction through
    /// `rebind_inflight_for_channel` itself. The r1 classifier/helper tests
    /// proved the phase decision but missed that the resume machinery's
    /// adoption save (`identity_gate.rs`) refused zero-id rows (`RebindError::
    /// Internal` — a 500 loop instead of the 409 loop) and that the adopted
    /// transcript's offsets were EOF-rebased (`adoption.rs` — backlog loss).
    /// This test drives the REAL path end to end against a live tmux session:
    ///   - `Ok(..)` kills reverting the identity-gate zero-id adoption arm
    ///     (that mutation returns `Err(Internal("persist existing inflight
    ///     watcher adoption …"))` — I1);
    ///   - `initial_offset == 8_192` kills reverting the adopted-orphan
    ///     durable-transcript arm (that mutation EOF-rebases to 12_288 — I3);
    ///   - `watcher_spawned` + the registry entry prove the single-watcher
    ///     claim was reached (the opus-arm safety property).
    ///
    /// Follows the `platform::tmux::live_pane_tests` precedent: skips when
    /// tmux is unavailable. `session_id` is seeded on the orphan row to stand
    /// in for the PG dispatched-session selector cache the live runtime
    /// carries (`shared.pg_pool` is `None` in tests); the saved-output-path
    /// `Keep` decision it produces is identical to the incident's.
    #[cfg(unix)]
    #[test]
    fn rebind_full_path_adopts_orphan_row_and_resumes_at_committed_offset() {
        // Sync `#[test]` + `block_on` (the `tmux_watcher/tests.rs` precedent)
        // so the env-lock guard is never held across an `.await` inside an
        // async fn — keeps the `await_holding_lock` ratchet baseline intact.
        let _lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let tmp = tempfile::tempdir().expect("tempdir");
        let _env_reset = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
            "AGENTDESK_ROOT_DIR",
            tmp.path(),
        );
        if !crate::services::platform::tmux::is_available() {
            eprintln!("skipping #4400 full-path rebind test: tmux is not available");
            return;
        }

        let provider = ProviderKind::Claude;
        let channel_id = 4_400_163_200_000_001_u64;
        let discord_channel = ChannelId::new(channel_id);
        // `-cc` suffix keeps `channel_supports_provider` resolving Claude for
        // the parsed channel name; the pid keeps parallel runs collision-free.
        let tmux_session = format!("AgentDesk-claude-e2e4400-{}-cc", std::process::id());
        let created =
            crate::services::platform::tmux::create_session(&tmux_session, None, "sleep 60")
                .expect("create tmux session");
        assert!(
            created.status.success(),
            "tmux session must start: {}",
            String::from_utf8_lossy(&created.stderr)
        );
        // The ClaudeTui runtime-kind marker the live handoff writes — this is
        // what `resolve_rebind_runtime_state` keys the transcript branch on.
        crate::services::tmux_common::write_tmux_runtime_kind_marker(
            &tmux_session,
            RuntimeHandoffKind::ClaudeTui,
        )
        .expect("write runtime-kind marker");

        // UUID-stem Claude transcript: 8_192 committed bytes + 4_096 backlog
        // bytes written while the watcher was dead (no trailing newline, so
        // the spawned watcher cannot consume a complete JSONL line).
        let session_uuid = "48fdb7f3-4400-4000-8000-000000163200";
        let transcript_path = tmp.path().join(format!("{session_uuid}.jsonl"));
        let transcript_path_str = transcript_path.display().to_string();
        std::fs::write(&transcript_path, vec![b'x'; 12_288]).expect("write transcript");

        // The re-minted #3107 self-heal orphan row, persisted through the same
        // atomic if-absent path the self-heal uses.
        let mut orphan = super::inflight::InflightTurnState::new(
            provider.clone(),
            channel_id,
            None,
            0,
            0,
            1_518_888_000_000_000_001,
            String::new(),
            Some(session_uuid.to_string()),
            Some(tmux_session.clone()),
            Some(transcript_path_str.clone()),
            None,
            8_192,
        );
        orphan.turn_source = super::inflight::TurnSource::ExternalInput;
        orphan.set_relay_owner_kind(super::inflight::RelayOwnerKind::Watcher);
        assert!(
            super::inflight::save_inflight_state_if_absent(&orphan).expect("persist orphan row"),
            "the self-heal if-absent write must land on an empty store"
        );

        let shared = crate::services::discord::make_shared_data_for_tests();
        let http = std::sync::Arc::new(serenity::Http::new("Bot test-token"));
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("current-thread runtime");
        let result = runtime.block_on(rebind_inflight_for_channel(
            &http,
            &shared,
            &provider,
            channel_id,
            Some(tmux_session.clone()),
            ManualRebindOverrides::default(),
        ));

        // Synchronous assertion window: on a current-thread runtime the
        // spawned watcher task cannot run once `block_on` returns, so the
        // persisted row below is exactly what the rebind path wrote.
        let row = super::inflight::load_inflight_state(&provider, channel_id);
        let watcher_entry = shared.tmux_watchers.remove(&discord_channel);
        if let Some((_, handle)) = watcher_entry.as_ref() {
            handle
                .cancel
                .store(true, std::sync::atomic::Ordering::SeqCst);
        }
        let _ = crate::services::platform::tmux::kill_session(&tmux_session, "#4400 e2e cleanup");

        let outcome = result.expect(
            "adopting the re-minted orphan row must resume the relay — neither the 409 \
             (InflightAlreadyExists) nor the 500 (Internal adoption-save refusal) deadlock (I1)",
        );
        assert_eq!(
            outcome.initial_offset, 8_192,
            "the watcher must resume at the committed offset — an EOF rebase (12_288) would \
             drop the backlog written while the watcher was dead (I3)"
        );
        assert!(
            outcome.watcher_spawned,
            "the single-watcher claim must be reached and spawn for the adopted row"
        );
        assert!(
            watcher_entry.is_some(),
            "the watcher registry must carry the channel's claimed watcher handle"
        );

        let row = row.expect("the adopted row must remain persisted");
        assert_eq!(row.user_msg_id, 0, "adoption must not mint a fake user id");
        assert_eq!(
            row.request_owner_user_id, 0,
            "adoption must not seize ownership for a synthetic owner (I2)"
        );
        assert_eq!(
            row.last_offset, 8_192,
            "the persisted committed offset must survive adoption un-rebased (I3)"
        );
        assert_eq!(
            row.tmux_session_name.as_deref(),
            Some(tmux_session.as_str())
        );
        assert_eq!(
            row.output_path.as_deref(),
            Some(transcript_path_str.as_str())
        );
        assert_eq!(
            row.effective_relay_owner_kind(),
            super::inflight::RelayOwnerKind::Watcher,
            "the adopted row must stay watcher-owned"
        );
    }
}
