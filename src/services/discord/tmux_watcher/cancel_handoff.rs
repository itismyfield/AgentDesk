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

pub(in crate::services::discord) type Store = Arc<Mutex<Vec<Pending>>>;

#[derive(Clone)]
pub(in crate::services::discord) struct Pending {
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
        let Some(row) = row else {
            return false;
        };
        self.provider == *provider
            && self.session == session
            && self.path == path
            && self.cancel.load(Ordering::Acquire)
            && !Arc::ptr_eq(&self.cancel, cancel)
            && self
                .identity
                .as_ref()
                .is_some_and(|identity| identity.matches_state(row))
            && self.nonce == row.turn_nonce
            && row.output_path.as_deref() == Some(path)
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
    pending: OwnedMutexGuard<Vec<Pending>>,
    checkpoint: Option<Pending>,
    cancel: Arc<AtomicBool>,
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
    ) -> Option<Self> {
        let store = shared.tmux_relay_coord(channel).cancel_handoffs.clone();
        let pending = match tokio::time::timeout(
            std::time::Duration::from_secs(15),
            store.lock_owned(),
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
            pending,
            checkpoint: None,
            cancel: cancel.clone(),
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
        let row =
            crate::services::discord::inflight::load_inflight_state(&self.provider, channel.get());
        let index = self
            .pending
            .iter()
            .position(|pending| pending.matches(self, shared, channel, row.as_ref()))?;
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
            let pending = self.pending.remove(index);
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
        let row = turn
            .and_then(|turn| turn.startup_inflight_snapshot.as_ref())
            .or(row);
        self.checkpoint = Some(Pending {
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
            turn: turn.cloned(),
            identity: row.map(InflightTurnIdentity::from_state),
            nonce: row.and_then(|row| row.turn_nonce.clone()),
            restored: parser.restored_turn.clone(),
            rewind: parser.pending_terminal_rewind_seed.clone(),
            rewind_key: parser.terminal_rewind_attempt_key.clone(),
            rewind_attempts: *parser.terminal_rewind_attempts,
            mirrored: *relay.all_data_fully_mirrored_to_session_relay,
            ack: relay.all_data_session_bound_relay_ack.clone(),
            first_sequence: *relay.all_data_first_forwarded_relay_sequence,
        });
    }

    pub(super) fn settled(&mut self) {
        self.checkpoint = None;
    }
}

impl Drop for Custody {
    fn drop(&mut self) {
        if self.cancel.load(Ordering::Acquire)
            && let Some(checkpoint) = self.checkpoint.take()
        {
            tracing::info!(session = %self.session, offset = checkpoint.offset,
                "watcher retained cancellation source/parser/body without advancing delivery");
            self.pending.push(checkpoint);
        }
    }
}

#[cfg(test)]
#[path = "cancel_handoff/interrupted_adoption_tests.rs"]
pub(super) mod interrupted_adoption_tests;
