//! Cooperative watcher replacement custody. This is not a delivery receipt or a
//! new restart store. The successor consumes the original opened source, byte
//! carry, parser and render state before polling, including when the file is at
//! EOF. Different turns/sources remain retained rather than borrowing authority
//! from a replacement row. Existing terminal receipt/lease checks still decide
//! whether anything may be published or finalized.
use super::*;
use crate::services::discord::inflight::InflightTurnIdentity;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use tokio::sync::{Mutex, OwnedMutexGuard};

pub(in crate::services::discord) type Store = Arc<HandoffStore>;

#[derive(Default)]
pub(in crate::services::discord) struct HandoffStore {
    pending: Arc<Mutex<Vec<Pending>>>,
    // Receipt metadata from the SAME custody capsule, readable while its
    // successor owns the reader mutex. This is not a new transport permission.
    recorded: std::sync::Mutex<Option<RecordedEpisode>>,
}

#[derive(Clone)]
pub(in crate::services::discord) struct RecordedEpisode {
    pub(in crate::services::discord) original: InflightTurnState,
    authority: WatcherSourceAuthority,
    source: Arc<std::fs::File>,
}

impl RecordedEpisode {
    pub(in crate::services::discord) fn matches_source(
        &self,
        generation: Option<i64>,
        stamp: Option<crate::services::cluster::stream_relay::SourceStamp>,
    ) -> bool {
        generation == Some(self.authority.generation_mtime_ns)
            && self.authority.source_stamp.is_some()
            && stamp == self.authority.source_stamp
    }
}

pub(in crate::services::discord) fn recorded_episode(
    shared: &SharedData,
    provider: &ProviderKind,
    channel: ChannelId,
    session: &str,
) -> Option<RecordedEpisode> {
    if !matches!(
        crate::services::discord::inflight::load_inflight_state_read_only_result(
            provider,
            channel.get()
        ),
        Ok(None)
    ) {
        return None;
    }
    let recorded = shared
        .tmux_relay_coord(channel)
        .cancel_handoffs
        .recorded
        .lock()
        .ok()?
        .clone()?;
    let row = &recorded.original;
    let path = row.output_path.as_deref()?;
    let start = row.turn_start_offset?;
    (row.provider == provider.as_str()
        && row.channel_id == channel.get()
        && row.delivery_record_owner_channel_id() == channel.get()
        && row.tmux_session_name.as_deref() == Some(session)
        && row
            .turn_nonce
            .as_deref()
            .is_some_and(|nonce| !nonce.is_empty())
        && recorded.authority.generation_mtime_ns != 0
        && recorded.authority.generation_mtime_ns == read_generation_file_mtime_ns(session)
        && recorded.authority.reset_incarnation
            == shared.relay_frontier_token(channel).reset_incarnation
        && recorded.authority.source_file
            != crate::services::cluster::stream_relay::SourceFileIdentity::Unavailable
        && crate::services::cluster::stream_relay::SourceFileIdentity::from_open_file(
            &recorded.source,
        ) == recorded.authority.source_file
        && std::fs::File::open(path).ok().is_some_and(|file| {
            crate::services::cluster::stream_relay::SourceFileIdentity::from_open_file(&file)
                == recorded.authority.source_file
        })
        && recent_turn_stop_for_watcher_range(channel, session, start).is_none())
    .then_some(recorded)
}

#[derive(Clone)]
pub(in crate::services::discord) struct Pending {
    legacy_mode: WatcherLegacyMode,
    provider: ProviderKind,
    session: String,
    path: String,
    cancel: Arc<AtomicBool>,
    pub(super) source: Arc<std::fs::File>,
    pub(super) authority: WatcherSourceAuthority,
    pub(super) offset: u64,
    pub(super) buffer: String,
    pub(super) buffer_start: u64,
    pub(super) utf8: Utf8ChunkDecoder,
    pub(super) turn: Option<CollectedTurnStream>,
    pub(super) identity: Option<InflightTurnIdentity>,
    pub(super) nonce: Option<String>,
    pub(super) restored: Option<RestoredWatcherTurn>,
    pub(super) rewind: Option<RestoredWatcherTurn>,
    pub(super) rewind_key: Option<WatcherRewindAttemptKey>,
    pub(super) rewind_attempts: u8,
    pub(super) mirrored: bool,
    pub(super) ack: Option<SessionBoundRelayAckTarget>,
    pub(super) first_sequence: Option<u64>,
}

impl Pending {
    fn recorded_episode(&self) -> Option<RecordedEpisode> {
        let turn = self.turn.as_ref()?;
        if turn.was_paused {
            return None;
        }
        let original = turn.startup_inflight_snapshot.as_ref()?;
        if !self.identity.as_ref()?.matches_state(original) || self.nonce != original.turn_nonce {
            return None;
        }
        Some(RecordedEpisode {
            original: original.clone(),
            authority: self.authority,
            source: self.source.clone(),
        })
    }

    fn matches(
        &self,
        custody: &Custody,
        shared: &SharedData,
        channel: ChannelId,
        row: Option<&InflightTurnState>,
    ) -> bool {
        let (provider, session, path, cancel) = (
            &custody.provider,
            custody.session.as_str(),
            custody.path.as_str(),
            &custody.cancel,
        );
        let same_episode = if !custody.legacy_mode.is_legacy() {
            self.legacy_mode == custody.legacy_mode
                && self.identity.is_none()
                && self.nonce.is_none()
                && self.restored.is_none()
                && self.rewind.is_none()
                && self.rewind_key.is_none()
                && self.rewind_attempts == 0
                && self.turn.as_ref().is_none_or(|turn| {
                    turn.startup_inflight_snapshot.is_none()
                        && turn.turn_identity_for_panel.is_none()
                        && turn.restored_response_seed.is_empty()
                })
        } else {
            self.legacy_mode.is_legacy()
                && match row {
                    Some(row) => {
                        self.identity
                            .as_ref()
                            .is_some_and(|id| id.matches_state(row))
                            && self.nonce == row.turn_nonce
                            && row.output_path.as_deref() == Some(path)
                    }
                    None => {
                        // Missing projection is not lost custody. Resume only an episode
                        // captured while the original source/turn was known, never a fresh
                        // rowless read or an identity reconstructed from the successor.
                        // Existing publication/receipt/lease gates still decide delivery.
                        self.turn
                            .as_ref()
                            .and_then(|turn| turn.startup_inflight_snapshot.as_ref())
                            .is_some_and(|original| {
                                self.identity
                                    .as_ref()
                                    .is_some_and(|id| id.matches_state(original))
                                    && self.nonce.as_deref().is_some_and(|nonce| !nonce.is_empty())
                                    && self.nonce == original.turn_nonce
                                    && original.provider == provider.as_str()
                                    && original.channel_id == channel.get()
                                    && original.tmux_session_name.as_deref() == Some(session)
                                    && original.output_path.as_deref() == Some(path)
                                    && original
                                        .turn_start_offset
                                        .is_some_and(|start| start < self.offset)
                            })
                    }
                }
        };
        self.provider == *provider
            && self.session == session
            && self.path == path
            && self.cancel.load(Ordering::Acquire)
            && !Arc::ptr_eq(&self.cancel, cancel)
            && same_episode
            && self.authority.generation_mtime_ns != 0
            && self.authority.generation_mtime_ns == read_generation_file_mtime_ns(session)
            && self.authority.reset_incarnation
                == shared.relay_frontier_token(channel).reset_incarnation
            && self.authority.source_file
                != crate::services::cluster::stream_relay::SourceFileIdentity::Unavailable
            && std::fs::File::open(path).ok().is_some_and(|file| {
                crate::services::cluster::stream_relay::SourceFileIdentity::from_open_file(&file)
                    == self.authority.source_file
                    && file.metadata().is_ok_and(|meta| meta.len() >= self.offset)
            })
            && self.turn.as_ref().is_none_or(|turn| !turn.was_paused)
            && recent_turn_stop_for_watcher_range(
                channel,
                session,
                self.identity
                    .as_ref()
                    .and_then(|identity| identity.turn_start_offset)
                    .unwrap_or(self.buffer_start),
            )
            .is_none()
    }
}

/// Holds the channel's reader custody until the outgoing watcher has published
/// its checkpoint. A successor cannot race ahead of a still-running predecessor.
/// Waiting is bounded: a wedged reader leaves custody intact and the caller exits
/// for the existing recovery machinery, rather than guessing that it joined.
pub(super) struct Custody {
    store: Store,
    pending: OwnedMutexGuard<Vec<Pending>>,
    checkpoint: Option<Pending>,
    cancel: Arc<AtomicBool>,
    legacy_mode: WatcherLegacyMode,
    provider: ProviderKind,
    session: String,
    path: String,
}

impl Custody {
    pub(super) async fn acquire(
        shared: &SharedData,
        channel: ChannelId,
        provider: &ProviderKind,
        session: &str,
        path: &str,
        cancel: &Arc<AtomicBool>,
        legacy_mode: WatcherLegacyMode,
    ) -> Option<Self> {
        let store = shared.tmux_relay_coord(channel).cancel_handoffs.clone();
        let pending = match tokio::time::timeout(
            std::time::Duration::from_secs(15),
            store.pending.clone().lock_owned(),
        )
        .await
        {
            Ok(guard) => guard,
            Err(_) => {
                tracing::warn!(
                    channel_id = channel.get(),
                    session,
                    "watcher cancellation custody still held; preserving state for a later recovery attempt"
                );
                return None;
            }
        };
        Some(Self {
            store,
            pending,
            checkpoint: None,
            cancel: cancel.clone(),
            legacy_mode,
            provider: provider.clone(),
            session: session.into(),
            path: path.into(),
        })
    }

    pub(super) fn take_for_current(
        &mut self,
        shared: &SharedData,
        channel: ChannelId,
    ) -> Option<Pending> {
        if self.cancel.load(Ordering::Acquire) {
            return None;
        }
        // A channel lookup is not a spawn identity. Pin the exact handle and do
        // not adopt state while a bridge pause/resume command is outstanding.
        let handle = shared.tmux_watchers.get(&channel)?;
        if !Arc::ptr_eq(&handle.cancel, &self.cancel)
            || handle.paused.load(Ordering::Acquire)
            || handle
                .resume_offset
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .is_some()
        {
            return None;
        }
        drop(handle);
        let pin = crate::services::discord::tmux_watcher_registry::WatcherClaimIncarnation::capture_for_source(
            &shared.tmux_watchers, &self.session, std::path::Path::new(&self.path),
        )?;
        if !Arc::ptr_eq(&pin.cancel, &self.cancel) {
            return None;
        }
        let pending = pin.adopt_if_current(&shared.tmux_watchers, |pin| {
            if self.cancel.load(Ordering::Acquire)
                || pin.paused.load(Ordering::Acquire)
                || pin
                    .resume_offset
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .is_some()
            {
                return None;
            }
            // Re-read at the incarnation-fenced take, and do not treat an I/O or
            // parse failure as rowless permission. This read cannot backfill a row.
            let row = if self.legacy_mode.is_legacy() {
                crate::services::discord::inflight::load_inflight_state_read_only_result(
                    &self.provider,
                    channel.get(),
                )
                .ok()?
            } else {
                None
            };
            let index = self
                .pending
                .iter()
                .position(|pending| pending.matches(self, shared, channel, row.as_ref()))?;
            let pending = self.pending.remove(index);
            if self.legacy_mode.is_legacy() {
                *self
                    .store
                    .recorded
                    .lock()
                    .unwrap_or_else(|e| e.into_inner()) = pending.recorded_episode();
            }
            #[cfg(test)]
            crate::services::discord::inflight::o_seed_observation::record_event(
                &self.provider,
                channel.get(),
                if self.legacy_mode.is_legacy() {
                    "capsule_legacy_adopted"
                } else {
                    "capsule_o_adopted"
                },
            );
            // Moving custody is not settlement. Keep an outgoing checkpoint
            // before the successor can await: cancellation before its first
            // collector checkpoint must not lose the only retained copy.
            let mut checkpoint = pending.clone();
            checkpoint.cancel = self.cancel.clone();
            self.checkpoint = Some(checkpoint);
            Some(pending)
        })??;
        tracing::info!(channel_id = channel.get(), session = %self.session, offset = pending.offset,
            "watcher consumed cancellation source/parser/body handoff");
        Some(pending)
    }

    pub(super) fn checkpoint(
        &mut self,
        parser: &TurnParseState<'_>,
        relay: &SupervisorRelayState<'_>,
        authority: WatcherSourceAuthority,
        turn: Option<&CollectedTurnStream>,
        row: Option<&InflightTurnState>,
    ) {
        let Some(source) = parser.retained_source.as_ref() else {
            return;
        };
        // Do not replace an already useful checkpoint with a pause/empty skip.
        if turn.is_none() && parser.all_data.is_empty() && !parser.utf8_decoder.has_pending() {
            return;
        }
        let row = if self.legacy_mode.is_legacy() {
            turn.and_then(|turn| turn.startup_inflight_snapshot.as_ref())
                .or(row)
        } else {
            None
        };
        let mut turn = turn.cloned();
        if !self.legacy_mode.is_legacy() {
            if let Some(turn) = &mut turn {
                turn.startup_inflight_snapshot = None;
                turn.turn_identity_for_panel = None;
                turn.restored_response_seed.clear();
            }
        }
        self.checkpoint = Some(Pending {
            legacy_mode: self.legacy_mode,
            provider: self.provider.clone(),
            session: self.session.clone(),
            path: self.path.clone(),
            cancel: self.cancel.clone(),
            source: source.clone(),
            authority,
            offset: *parser.current_offset,
            buffer: parser.all_data.clone(),
            buffer_start: *parser.all_data_start_offset,
            utf8: parser.utf8_decoder.clone(),
            turn,
            identity: row.map(InflightTurnIdentity::from_state),
            nonce: row.and_then(|row| row.turn_nonce.clone()),
            restored: self
                .legacy_mode
                .is_legacy()
                .then(|| parser.restored_turn.clone())
                .flatten(),
            rewind: self
                .legacy_mode
                .is_legacy()
                .then(|| parser.pending_terminal_rewind_seed.clone())
                .flatten(),
            rewind_key: self
                .legacy_mode
                .is_legacy()
                .then(|| parser.terminal_rewind_attempt_key.clone())
                .flatten(),
            rewind_attempts: if self.legacy_mode.is_legacy() {
                *parser.terminal_rewind_attempts
            } else {
                0
            },
            mirrored: *relay.all_data_fully_mirrored_to_session_relay,
            ack: relay.all_data_session_bound_relay_ack.clone(),
            first_sequence: *relay.all_data_first_forwarded_relay_sequence,
        });
    }

    pub(super) fn settled(&mut self) {
        self.checkpoint = None;
        *self
            .store
            .recorded
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = None;
    }
}

impl Drop for Custody {
    fn drop(&mut self) {
        if self.cancel.load(Ordering::Acquire)
            && let Some(checkpoint) = self.checkpoint.take()
        {
            tracing::info!(session = %self.session, offset = checkpoint.offset,
                "watcher retained cancellation source/parser/body without advancing delivery");
            if self.legacy_mode.is_legacy() {
                *self
                    .store
                    .recorded
                    .lock()
                    .unwrap_or_else(|e| e.into_inner()) = checkpoint.recorded_episode();
            }
            self.pending.push(checkpoint);
        }
    }
}

#[cfg(test)]
#[path = "cancel_handoff/interrupted_adoption_tests.rs"]
pub(super) mod interrupted_adoption_tests;

#[path = "cancel_handoff/completion.rs"]
pub(super) mod completion;

/// A retained original episode still owes preview/actor settlement after the
/// sink commits. Do not let the rowless watermark shortcut bypass that epilogue.
/// This only preserves candidacy; existing sink ACK/receipt and lease gates run.
pub(super) fn has_recorded_completion(
    context: &TerminalPreflightContext<'_>,
    start: u64,
    end: u64,
) -> bool {
    recorded_episode(
        context.shared,
        context.watcher_provider,
        context.channel_id,
        context.tmux_session_name,
    )
    .is_some_and(|episode| {
        episode.original.turn_start_offset == Some(start)
            && end > start
            && episode
                .source
                .metadata()
                .is_ok_and(|meta| meta.len() >= end)
    })
}

/// Cancellation moves custody; it does not discard output this collector already
/// parsed. The post-collect cancel exit belongs to the no-result path only, so a
/// turn whose terminal was parsed before the cancel still reaches delivery.
pub(super) fn cancel_yields_before_delivery(
    cancel: &AtomicBool,
    turn: Option<&CollectedTurnStream>,
) -> bool {
    cancel.load(Ordering::Acquire) && !turn.is_some_and(|turn| turn.found_result)
}

#[cfg(test)]
pub(super) mod capsule_observation {
    use super::*;

    #[derive(Clone, Debug, PartialEq, Eq)]
    pub(in crate::services::discord::tmux::tmux_watcher) struct Summary {
        pub origin: WatcherLegacyMode,
        pub body: String,
        pub buffer: String,
        pub offset: u64,
        pub has_utf8_carry: bool,
        pub has_identity: bool,
        pub identity: Option<InflightTurnIdentity>,
        pub nonce: Option<String>,
        pub has_startup_snapshot: bool,
        pub startup_snapshot: Option<serde_json::Value>,
        pub restored_response_seed: String,
        pub authority: WatcherSourceAuthority,
        pub opened_source: crate::services::cluster::stream_relay::SourceFileIdentity,
        pub cancel_address: usize,
        pub has_restored_seed: bool,
        pub has_rewind_key: bool,
        pub rewind_attempts: u8,
        pub has_turn_identity_for_panel: bool,
        pub utf8_state: String,
    }

    pub(in crate::services::discord::tmux::tmux_watcher) async fn pending(
        shared: &SharedData,
        channel: ChannelId,
    ) -> Vec<Summary> {
        let store = shared.tmux_relay_coord(channel).cancel_handoffs.clone();
        let pending = store.pending.lock().await;
        pending
            .iter()
            .map(|capsule| Summary {
                origin: capsule.legacy_mode,
                body: capsule
                    .turn
                    .as_ref()
                    .map(|turn| turn.full_response.clone())
                    .unwrap_or_default(),
                buffer: capsule.buffer.clone(),
                offset: capsule.offset,
                has_utf8_carry: capsule.utf8.has_pending(),
                has_identity: capsule.identity.is_some(),
                identity: capsule.identity.clone(),
                nonce: capsule.nonce.clone(),
                has_startup_snapshot: capsule
                    .turn
                    .as_ref()
                    .is_some_and(|turn| turn.startup_inflight_snapshot.is_some()),
                startup_snapshot: capsule.turn.as_ref()
                    .and_then(|turn| turn.startup_inflight_snapshot.as_ref())
                    .map(|row| serde_json::to_value(row).unwrap()),
                restored_response_seed: capsule.turn.as_ref()
                    .map(|turn| turn.restored_response_seed.clone()).unwrap_or_default(),
                authority: capsule.authority,
                opened_source: crate::services::cluster::stream_relay::SourceFileIdentity::from_open_file(&capsule.source),
                cancel_address: Arc::as_ptr(&capsule.cancel) as usize,
                has_restored_seed: capsule.restored.is_some() || capsule.rewind.is_some(),
                has_rewind_key: capsule.rewind_key.is_some(),
                rewind_attempts: capsule.rewind_attempts,
                has_turn_identity_for_panel: capsule.turn.as_ref()
                    .is_some_and(|turn| turn.turn_identity_for_panel.is_some()),
                utf8_state: format!("{:?}", capsule.utf8),
            })
            .collect()
    }
}
