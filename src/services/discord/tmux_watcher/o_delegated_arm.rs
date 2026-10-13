//! Watcher terminal arms by O ownership. On O's channel Legacy promotes any task card, consumes the
//! range with no body transport, lease or evidence and clears its "..."; a Legacy send claims first.

use std::sync::Arc;

use super::task_response_authority::PrepareWatcherTaskResponseError;
use super::*;

use crate::services::discord::inflight::{InflightTurnIdentity, InflightTurnState};

pub(super) struct DelegatedTerminal<'a> {
    pub(super) http: &'a Arc<serenity::Http>,
    pub(super) shared: &'a Arc<SharedData>,
    pub(super) provider: &'a ProviderKind,
    pub(super) channel_id: ChannelId,
    pub(super) tmux_session_name: &'a str,
    pub(super) placeholder_msg_id: Option<MessageId>,
    pub(super) inflight_before_relay: Option<&'a InflightTurnState>,
    pub(super) inflight_identity_before_relay: Option<&'a InflightTurnIdentity>,
    pub(super) consumed_end: u64,
    pub(super) response_sent_offset: usize,
    pub(super) last_edit_text: &'a str,
    pub(super) turn_data_start_offset: u64,
    pub(super) observed_generation_mtime_ns: &'a mut Option<i64>,
    pub(super) task_card:
        Option<&'a crate::services::discord::task_notification_delivery::TaskNotificationContext>,
}

/// Mirrors the delegated-success watermark epilogue; the confirmed end advances later through
/// the watcher's lease-free commit path.
pub(super) async fn consume_delegated_terminal(
    arm: DelegatedTerminal<'_>,
) -> Result<(), PrepareWatcherTaskResponseError> {
    if let Some(context) = arm.task_card {
        super::task_response_authority::promote_delegated_task_card(
            arm.http,
            arm.shared,
            arm.provider,
            arm.channel_id,
            arm.tmux_session_name,
            context,
        )
        .await?;
    }
    let generation_mtime_ns = read_generation_file_mtime_ns(arm.tmux_session_name);
    *arm.observed_generation_mtime_ns = Some(generation_mtime_ns);
    if let Some(msg_id) = arm.placeholder_msg_id {
        let _ = delete_terminal_placeholder_unless_delivered(
            arm.http,
            arm.channel_id,
            arm.shared,
            arm.provider,
            arm.tmux_session_name,
            msg_id,
            arm.inflight_before_relay,
            Some((arm.turn_data_start_offset, arm.consumed_end)),
            arm.response_sent_offset,
            arm.last_edit_text,
            false,
            "watcher_o_delegated_cleanup",
        )
        .await;
    }
    crate::services::observability::watcher_latency::record_first_relay(arm.channel_id.get());
    if let Some(identity) = arm.inflight_identity_before_relay {
        let _ = crate::services::discord::inflight::persist_watcher_relay_watermark_locked(
            arm.provider,
            arm.channel_id.get(),
            identity,
            arm.tmux_session_name,
            crate::services::discord::inflight::WatcherRelayWatermarkPatch {
                last_watcher_relayed_offset: Some(arm.turn_data_start_offset),
                last_watcher_relayed_generation_mtime_ns: Some(generation_mtime_ns),
            },
        );
    }
    clear_provider_overload_retry_state(arm.channel_id);
    Ok(())
}

/// The claim a direct terminal body sends under, taken at each arm's own transport; a task
/// response claims inside its own send instead.
pub(super) fn direct_body_claim(
    claims: bool,
    channel: ChannelId,
    session: &str,
) -> Option<crate::services::tui_o::cutover::BodyClaim<'_>> {
    claims.then(|| crate::services::tui_o::cutover::BodyClaim::tmux(channel.get(), Some(session)))
}

/// Whether O took (or holds) the channel since the watcher's peek, so nothing Legacy sent counts.
pub(super) fn o_took_channel(channel: ChannelId, session: &str) -> bool {
    crate::services::tui_o::cutover::peek_o_owns_tui_output_for_channel_tmux(
        channel.get(),
        Some(session),
    ) != Ok(false)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum RetiredTerminalOutcome {
    Consumed,
    Retained,
}

pub(super) struct RetiredTerminal<'a, 'b> {
    pub(super) context: &'a PollWatcherContext<'b>,
    pub(super) controls: &'a PollWatcherControls<'b>,
    pub(super) parser: &'a TurnParseState<'b>,
    pub(super) turn: &'a CollectedTurnStream,
    pub(super) authority: WatcherSourceAuthority,
    pub(super) epoch: u64,
}

impl RetiredTerminal<'_, '_> {
    // Confirmed source changes end this incarnation; busy authority and failed reads stay retryable.
    fn permanently_invalid(&self) -> bool {
        use crate::services::cluster::stream_relay::SourceFileIdentity;
        use crate::services::tmux_common as tc;
        use crate::services::tui_prompt_dedupe as binding;
        use std::os::fd::AsRawFd;
        use std::os::unix::fs::OpenOptionsExt;
        let ctx = self.context;
        tc::try_with_tmux_source_authority(ctx.tmux_session_name, |_| {
            let stamp = self.authority.source_stamp?;
            let _registry = crate::services::discord::tmux_watcher_registry::lock_tmux_watcher_registry();
            let handle = ctx.shared.tmux_watchers.get(&ctx.channel_id)?;
            if handle.tmux_session_name != ctx.tmux_session_name
                || handle.output_path != ctx.output_path
                || !Arc::ptr_eq(&handle.cancel, self.controls.cancel)
                || !ctx.shared.tmux_watchers.watcher_reservation_matches(
                    ctx.tmux_session_name, ctx.channel_id, self.controls.cancel,
                )
            {
                return Some(true);
            }
            drop(handle);
            let expected = ctx.retired_binding.as_ref()?;
            let mut current = binding::try_peek_tmux_runtime_binding(ctx.tmux_session_name).ok()?;
            if let Some(binding) = current.as_mut() {
                binding.last_offset = 0;
                binding.relay_last_offset = None;
            }
            if current != *expected {
                return Some(true);
            }
            let witness = crate::services::discord::delivery_lease_cell::source_epoch_observer::read_source_epoch_witness(ctx.tmux_session_name);
            if witness.generation.is_some_and(|generation| {
                Some(generation) != stamp.witness.generation
            }) || witness.spawn_nonce_hash.is_some_and(|nonce| {
                Some(nonce) != stamp.witness.spawn_nonce_hash
            })
            {
                return Some(true);
            }
            if self.authority.reset_incarnation
                != ctx.shared.relay_frontier_token(ctx.channel_id).reset_incarnation
            {
                return Some(true);
            }
            let owner = std::fs::read_to_string(tc::tmux_owner_path(ctx.tmux_session_name)).ok()?;
            if !owner.trim().is_empty() && owner.trim() != tc::current_tmux_owner_marker() {
                return Some(true);
            }
            if tc::read_tmux_channel_binding(ctx.tmux_session_name)
                .is_some_and(|channel| channel != ctx.channel_id.get())
                || tc::resolve_tmux_runtime_kind_marker(ctx.tmux_session_name).is_some_and(|kind| {
                    kind != if *ctx.watcher_provider == ProviderKind::Claude {
                        crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui
                    } else {
                        crate::services::agent_protocol::RuntimeHandoffKind::CodexTui
                    }
                })
            {
                return Some(true);
            }
            let file = std::fs::OpenOptions::new().read(true).custom_flags(libc::O_NONBLOCK)
                .open(ctx.output_path).ok()?;
            let metadata = file.metadata().ok()?;
            if !metadata.is_file() {
                return Some(true);
            }
            if *ctx.watcher_provider == ProviderKind::Claude
                && ctx.output_path != tc::session_temp_path(ctx.tmux_session_name, "jsonl")
            {
                use crate::services::claude_tui::source_verify::{observe_transcript, OpenedTranscript, FirstRecord};
                if let Ok(OpenedTranscript::Opened { first: FirstRecord::Session(id), .. }) =
                    observe_transcript(std::path::Path::new(&format!("/dev/fd/{}", file.as_raw_fd())))
                    && current.as_ref().and_then(|binding| binding.session_id.as_deref()) != Some(id.as_str())
                {
                    return Some(true);
                }
            }
            let identity = match SourceFileIdentity::from_open_file(&file) {
                SourceFileIdentity::Unavailable => return None,
                identity => identity,
            };
            Some(identity != self.authority.source_file
                || metadata.len() < *self.parser.current_offset)
        }).flatten().unwrap_or(false)
    }

    fn range(&self) -> (u64, u64) {
        (
            self.turn.turn_data_start_offset,
            terminal_event_consumed_offset(
                self.parser
                    .current_offset
                    .saturating_sub(self.parser.utf8_decoder.pending_len()),
                self.parser.all_data,
            ),
        )
    }

    fn under_current_authority<T>(
        &self,
        expected: &Option<crate::services::tui_prompt_dedupe::TuiRuntimeBinding>,
        settle: impl FnOnce() -> T,
    ) -> Option<T> {
        use crate::services::cluster::stream_relay::SourceFileIdentity;
        use crate::services::tui_prompt_dedupe as binding;
        use std::os::fd::AsRawFd;
        use std::os::unix::fs::OpenOptionsExt;
        let ctx = self.context;
        let controls = self.controls;
        crate::services::tmux_common::try_with_tmux_source_authority(ctx.tmux_session_name, |source_guard| {
            let _registry = crate::services::discord::tmux_watcher_registry::lock_tmux_watcher_registry();
            let owner = ctx.shared.tmux_watchers.owner_channel_for_tmux_session(ctx.tmux_session_name)?;
            let handle = ctx.shared.tmux_watchers.get(&owner)?;
            let mut current = binding::try_peek_tmux_runtime_binding(ctx.tmux_session_name).ok()?;
            if let Some(binding) = current.as_mut() { binding.last_offset = 0; binding.relay_last_offset = None; }
            let kind = crate::services::tmux_common::resolve_tmux_runtime_kind_marker(ctx.tmux_session_name)?;
            let wrapper = ctx.output_path == crate::services::tmux_common::session_temp_path(ctx.tmux_session_name, "jsonl");
            let stamp = self.authority.source_stamp?;
            let source = self.parser.retained_source.as_ref()?;
            let reopened = std::fs::OpenOptions::new().read(true).custom_flags(libc::O_NONBLOCK).open(ctx.output_path).ok()?;
            if !reopened.metadata().ok()?.is_file() { return None; }
            let _reset = ctx.shared.acquire_relay_frontier_mutation_for_incarnation(ctx.channel_id, self.authority.reset_incarnation)?;
            let range = self.range();
            let native_session_matches = if kind == crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui && !wrapper {
                use crate::services::claude_tui::source_verify::{observe_transcript, OpenedTranscript, FirstRecord};
                matches!(observe_transcript(std::path::Path::new(&format!("/dev/fd/{}", reopened.as_raw_fd()))),
                    Ok(OpenedTranscript::Opened { file, first: FirstRecord::Session(id) })
                        if file == stamp.file && current.as_ref().and_then(|binding| binding.session_id.as_deref()) == Some(id.as_str()))
            } else { true };
            let valid = native_session_matches && session_belongs_to_current_runtime(ctx.tmux_session_name, &crate::services::tmux_common::current_tmux_owner_marker())
                && kind == if *ctx.watcher_provider == ProviderKind::Claude { crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui } else { crate::services::agent_protocol::RuntimeHandoffKind::CodexTui }
                && crate::services::tmux_common::read_tmux_channel_binding(ctx.tmux_session_name) == Some(ctx.channel_id.get())
                && current.as_ref().is_none_or(|binding| binding.runtime_kind == kind)
                && ctx.shared.tmux_watchers.watcher_reservation_matches(ctx.tmux_session_name, ctx.channel_id, controls.cancel)
                && owner == ctx.channel_id && handle.tmux_session_name == ctx.tmux_session_name
                && handle.output_path == ctx.output_path && Arc::ptr_eq(&handle.cancel, controls.cancel)
                && Arc::ptr_eq(&handle.paused, controls.paused) && Arc::ptr_eq(&handle.pause_epoch, controls.pause_epoch)
                && !controls.paused.load(Ordering::Acquire) && !self.turn.was_paused
                && controls.pause_epoch.load(Ordering::Acquire) == self.epoch
                && !controls.turn_delivered.load(Ordering::Acquire)
                && controls.resume_offset.lock().ok()?.is_none()
                && self.authority == self.turn.source_authority
                && stamp.file != SourceFileIdentity::Unavailable && stamp.file == self.authority.source_file
                && self.authority.generation_mtime_ns != 0
                && self.authority.generation_mtime_ns == read_generation_file_mtime_ns(ctx.tmux_session_name)
                && stamp.witness == crate::services::discord::delivery_lease_cell::source_epoch_observer::read_source_epoch_witness(ctx.tmux_session_name)
                && self.authority.reset_incarnation == ctx.shared.relay_frontier_token(ctx.channel_id).reset_incarnation
                && SourceFileIdentity::from_open_file(source) == stamp.file
                && SourceFileIdentity::from_open_file(&reopened) == stamp.file
                && reopened.metadata().ok()?.is_file()
                && reopened.metadata().ok()?.len() >= *self.parser.current_offset
                && range.0 < range.1 && range.1 == *self.parser.all_data_start_offset
                && current == *expected
                && current.as_ref().map_or(wrapper, |binding| if wrapper { binding.relay_output_path.as_deref().is_none_or(|path| path == ctx.output_path) } else { binding.output_path == ctx.output_path })
                && current.as_ref().is_none_or(|binding| binding::codex_verified_publication_allowed(source_guard, binding))
                && binding::codex_verified_source_allowed_under_source_authority(source_guard, current.as_ref().map_or(ctx.output_path, |binding| binding.output_path.as_str()), current.as_ref().and_then(|binding| binding.session_id.as_deref()))
                && crate::services::tui_o::cutover::peek_o_owns_tui_output_for_channel(ctx.channel_id.get(), Some(kind)) == Ok(true)
                && recent_turn_stop_for_watcher_range(ctx.channel_id, ctx.tmux_session_name, range.0).is_none()
                && self.turn.completion_actor.is_none()
                && self.turn.startup_inflight_snapshot.is_none() && self.turn.turn_identity_for_panel.is_none()
                && self.turn.restored_response_seed.is_empty();
            drop(handle);
            valid.then(settle)
        }).flatten()
    }
}

/// Settles source custody only; O's independent writer still owns its POST and receipt.
pub(super) async fn consume_retired_terminal(
    arm: RetiredTerminal<'_, '_>,
    custody: &mut cancel_handoff::Custody,
) -> RetiredTerminalOutcome {
    let ctx = arm.context;
    let Some(binding) = ctx.retired_binding.as_ref() else {
        return RetiredTerminalOutcome::Retained;
    };
    if arm.under_current_authority(binding, || ()).is_none() {
        return RetiredTerminalOutcome::Retained;
    }
    if let Some(card) = arm
        .turn
        .task_notification_kind
        .and(arm.turn.task_notification_context.as_ref())
    {
        if task_response_authority::promote_retired_task_card(
            ctx.http,
            ctx.shared,
            ctx.watcher_provider,
            ctx.channel_id,
            ctx.tmux_session_name,
            card,
        )
        .await
        .is_err()
        {
            return RetiredTerminalOutcome::Retained;
        }
    }
    if arm.under_current_authority(binding, || ()).is_none() {
        return RetiredTerminalOutcome::Retained;
    }
    if let Some(message) = arm.turn.placeholder_msg_id {
        let cleanup = delete_terminal_placeholder_unless_delivered(
            ctx.http,
            ctx.channel_id,
            ctx.shared,
            ctx.watcher_provider,
            ctx.tmux_session_name,
            message,
            None,
            Some(arm.range()),
            arm.turn.response_sent_offset,
            &arm.turn.last_edit_text,
            false,
            "watcher_o_retired_cleanup",
        )
        .await;
        if cleanup.is_some_and(|outcome| !outcome.is_committed()) {
            return RetiredTerminalOutcome::Retained;
        }
    }
    let consumed = arm
        .under_current_authority(binding, || {
            let settled = custody.settle_retired_prefix(arm.authority, arm.range());
            #[cfg(test)]
            if settled {
                crate::services::discord::inflight::o_seed_observation::retired_terminal_consumed(
                    ctx.watcher_provider,
                    ctx.channel_id.get(),
                    arm.range(),
                );
            }
            settled
        })
        .unwrap_or(false);
    if consumed {
        #[cfg(unix)]
        if !arm.turn.injected_candidates.is_empty() && !arm.controls.cancel.load(Ordering::Acquire)
        {
            crate::services::discord::replay_policy::injected::consume(
                ctx.shared,
                ctx.channel_id.get(),
                arm.turn.injected_candidates.clone(),
            )
            .await;
        }
        RetiredTerminalOutcome::Consumed
    } else {
        RetiredTerminalOutcome::Retained
    }
}

pub(super) async fn dispatch_retired_terminal(
    arm: RetiredTerminal<'_, '_>,
    custody: &mut cancel_handoff::Custody,
) {
    loop {
        let next = RetiredTerminal {
            context: arm.context,
            controls: arm.controls,
            parser: arm.parser,
            turn: arm.turn,
            authority: arm.authority,
            epoch: arm.epoch,
        };
        if consume_retired_terminal(next, custody).await == RetiredTerminalOutcome::Consumed {
            return;
        }
        #[cfg(test)]
        crate::services::discord::inflight::o_seed_observation::record_event(
            arm.context.watcher_provider,
            arm.context.channel_id.get(),
            "retired_terminal_retained",
        );
        if arm.controls.cancel.load(Ordering::Acquire) {
            return;
        }
        let still_current = {
            let _registry =
                crate::services::discord::tmux_watcher_registry::lock_tmux_watcher_registry();
            arm.context
                .shared
                .tmux_watchers
                .get(&arm.context.channel_id)
                .is_some_and(|handle| Arc::ptr_eq(&handle.cancel, arm.controls.cancel))
        };
        if !still_current
            || arm.permanently_invalid()
            || arm.controls.pause_epoch.load(Ordering::Acquire) != arm.epoch
            || arm
                .controls
                .resume_offset
                .lock()
                .is_ok_and(|offset| offset.is_some())
            || arm.controls.turn_delivered.load(Ordering::Acquire)
            || arm
                .context
                .shared
                .restart
                .shutting_down
                .load(Ordering::Acquire)
        {
            // Signal only this outgoing incarnation so Drop publishes its retained checkpoint.
            let _ = arm.controls.cancel.compare_exchange(
                false,
                true,
                Ordering::AcqRel,
                Ordering::Acquire,
            );
            return;
        }
        sleep_or_jsonl_event(
            std::time::Duration::from_millis(250),
            arm.controls.jsonl_notify,
            arm.controls.dead_marker_notify,
        )
        .await;
    }
}

pub(super) async fn dispatch_collected(
    (ctx, controls): (&PollWatcherContext<'_>, &PollWatcherControls<'_>),
    parser: &TurnParseState<'_>,
    relay: &SupervisorRelayState<'_>,
    custody: &mut cancel_handoff::Custody,
    (authority, epoch, nonce): (WatcherSourceAuthority, u64, &Option<String>),
    outcome: CollectOutcome,
) -> Result<CollectOutcome, ()> {
    custody.checkpoint_collection(ctx, parser, relay, authority, nonce, &outcome);
    let checkpoint_turn = match &outcome {
        CollectOutcome::Fallthrough(turn) => Some(turn),
        _ => None,
    };
    #[cfg(test)]
    if checkpoint_turn.is_some_and(|turn| turn.found_result) {
        crate::services::discord::inflight::o_seed_observation::terminal_checkpoint(
            ctx.watcher_provider,
            ctx.channel_id.get(),
        )
        .await;
    }
    if cancel_handoff::cancel_yields_before_delivery(controls.cancel, checkpoint_turn) {
        return Err(());
    }
    let collected = match outcome {
        CollectOutcome::ContinueWatcherLoop => return Ok(CollectOutcome::ContinueWatcherLoop),
        CollectOutcome::Fallthrough(collected) => collected,
    };
    if ctx.legacy_mode == WatcherLegacyMode::RetiredO && collected.found_result {
        dispatch_retired_terminal(
            RetiredTerminal {
                context: ctx,
                controls,
                parser,
                turn: &collected,
                authority,
                epoch,
            },
            custody,
        )
        .await;
        return if controls.cancel.load(Ordering::Acquire) {
            Err(())
        } else {
            Ok(CollectOutcome::ContinueWatcherLoop)
        };
    }
    Ok(CollectOutcome::Fallthrough(collected))
}

pub(super) fn pin_retired_binding(
    mode: WatcherLegacyMode,
    session: &str,
) -> Option<Option<crate::services::tui_prompt_dedupe::TuiRuntimeBinding>> {
    if mode.is_legacy() {
        return None;
    }
    crate::services::tui_prompt_dedupe::try_peek_tmux_runtime_binding(session)
        .ok()
        .map(|mut binding| {
            if let Some(binding) = binding.as_mut() {
                binding.last_offset = 0;
                binding.relay_last_offset = None;
            }
            binding
        })
}
