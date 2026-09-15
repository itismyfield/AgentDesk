from pathlib import Path

def edit(path, old, new):
    p=Path(path);s=p.read_text();assert s.count(old)==1,(path,old[:80],s.count(old));p.write_text(s.replace(old,new))
b='src/services/discord/tmux_watcher/'
edit(b+'cancel_handoff.rs','pub(in crate::services::discord) type Store = Arc<Mutex<Vec<Pending>>>;','''pub(in crate::services::discord) type Store = Arc<HandoffStore>;

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
    pub(in crate::services::discord) authority: WatcherSourceAuthority,
    source: Arc<std::fs::File>,
}

pub(in crate::services::discord) fn recorded_episode(
    shared: &SharedData, provider: &ProviderKind, channel: ChannelId,
    session: &str,
) -> Option<RecordedEpisode> {
    if !crate::services::discord::relay_recovery::cohort::enforcement_admits(channel.get())
        || !matches!(crate::services::discord::inflight::load_inflight_state_read_only_result(provider, channel.get()), Ok(None))
    { return None; }
    let recorded = shared.tmux_relay_coord(channel).cancel_handoffs.recorded.lock().ok()?.clone()?;
    let row = &recorded.original;
    let path = row.output_path.as_deref()?;
    let start = row.turn_start_offset?;
    (row.provider == provider.as_str() && row.channel_id == channel.get()
        && row.delivery_record_owner_channel_id() == channel.get()
        && row.tmux_session_name.as_deref() == Some(session)
        && row.turn_nonce.as_deref().is_some_and(|nonce| !nonce.is_empty())
        && recorded.authority.generation_mtime_ns != 0
        && recorded.authority.generation_mtime_ns == read_generation_file_mtime_ns(session)
        && recorded.authority.reset_incarnation == shared.relay_frontier_token(channel).reset_incarnation
        && recorded.authority.source_file != crate::services::cluster::stream_relay::SourceFileIdentity::Unavailable
        && crate::services::cluster::stream_relay::SourceFileIdentity::from_open_file(&recorded.source) == recorded.authority.source_file
        && std::fs::File::open(path).ok().is_some_and(|file|
            crate::services::cluster::stream_relay::SourceFileIdentity::from_open_file(&file) == recorded.authority.source_file)
        && recent_turn_stop_for_watcher_range(channel, session, start).is_none())
        .then_some(recorded)
}
''')
edit(b+'cancel_handoff.rs', 'impl Pending {', '''impl Pending {
    fn recorded_episode(&self) -> Option<RecordedEpisode> {
        let original = self.turn.as_ref()?.startup_inflight_snapshot.as_ref()?;
        if !self.identity.as_ref()?.matches_state(original) || self.nonce != original.turn_nonce {
            return None;
        }
        Some(RecordedEpisode { original: original.clone(), authority: self.authority, source: self.source.clone() })
    }
''')
edit(b+'cancel_handoff.rs','pub(super) struct Custody {','pub(super) struct Custody {\n    store: Store,')
edit(b+'cancel_handoff.rs','            store.lock_owned(),','            store.pending.clone().lock_owned(),')
edit(b+'cancel_handoff.rs','        Some(Self {\n            pending,','        Some(Self {\n            store,\n            pending,')
edit(b+'cancel_handoff.rs','            let pending = self.pending.remove(index);','''            let pending = self.pending.remove(index);
            *self.store.recorded.lock().unwrap_or_else(|e| e.into_inner()) = pending.recorded_episode();''')
edit(b+'cancel_handoff.rs','        self.checkpoint = None;','''        self.checkpoint = None;
        *self.store.recorded.lock().unwrap_or_else(|e| e.into_inner()) = None;''')
edit(b+'cancel_handoff.rs','            self.pending.push(checkpoint);','''            *self.store.recorded.lock().unwrap_or_else(|e| e.into_inner()) = checkpoint.recorded_episode();
            self.pending.push(checkpoint);''')
edit('src/services/discord/relay_coord.rs','cancel_handoffs: Arc::new(tokio::sync::Mutex::new(Vec::new())),','cancel_handoffs: Default::default(),')
p=Path('src/services/discord/session_relay_sink/delivery_frontier.rs')
s=p.read_text();s+='''
/// Reuse a cancelled reader's original episode only for the exact source/fenced
/// frame. The existing sink lease still serializes every actual transport.
fn cancellation_episode(
    shared: &SharedData, delivery: &SessionRelayDelivery,
) -> Option<crate::services::discord::InflightTurnState> {
    let captured = crate::services::discord::tmux::tmux_watcher::cancel_handoff::recorded_episode(
        shared, &delivery.provider, ChannelId::new(delivery.channel_id), &delivery.session_name,
    )?;
    let row = captured.original;
    let end = delivery.terminal_consumed_end?;
    (delivery.frame_turn_user_msg_id == row.user_msg_id
        && delivery.frame_turn_started_at == row.started_at
        && delivery.frame_turn_start_offset == row.turn_start_offset
        && row.turn_start_offset.is_some_and(|start| end > start)
        && delivery.relay_generation_mtime_ns == Some(captured.authority.generation_mtime_ns)
        && captured.authority.source_stamp.is_some()
        && delivery.relay_source_stamp == captured.authority.source_stamp
        && row.output_path.as_ref().is_some_and(|path| std::fs::metadata(path).is_ok_and(|meta| meta.len() >= end)))
        .then_some(row)
}

impl super::SessionBoundDiscordRelaySink {
    pub(super) async fn cancelled_episode_is_retained(&self, delivery: &SessionRelayDelivery) -> bool {
        self.health_registry.shared_for_provider(&delivery.provider).await
            .is_some_and(|shared| cancellation_episode(&shared, delivery).is_some())
    }
}

fn persist_cancelled_episode(
    ctx: SinkDeliveryCtx<'_>, original: &crate::services::discord::InflightTurnState,
    anchor: Option<u64>, body: &str,
) -> SinkDeliveryProofResult {
    use crate::services::discord::outbound::delivery_record as records;
    let Some(anchor) = anchor.filter(|id| *id != 0) else { return SinkDeliveryProofResult::LandedUnrecorded; };
    let source = records::ExactJsonlSourceIdentity {
        provider: ctx.provider.as_str().into(), tmux_session_name: ctx.delivery.session_name.clone(),
        turn_nonce: original.turn_nonce.clone().unwrap_or_default(), range: ctx.authority.range,
        generation_mtime_ns: ctx.authority.identity.generation_mtime_ns,
        offset_authority_channel_id: ctx.channel.get(), delivery_channel_id: ctx.channel.get(),
    };
    if records::record_current_pinned_delivery(&source, anchor).is_err() {
        return SinkDeliveryProofResult::LandedUnrecorded;
    }
    records::record_pinned_delivery_metadata(&source, body, original.effective_finalizer_turn_id());
    SinkDeliveryProofResult::Persisted
}
''';p.write_text(s)
edit(str(p),'''            self.delivery,
        )
    }
}''','''            self.delivery,
        ).or_else(|| cancellation_episode(self.shared, self.delivery))
    }
}''')
edit(str(p),'''    if !mutation.persist(
''','''    if let Some(original) = cancellation_episode(ctx.shared, ctx.delivery) {
        // `mutation` continues holding the existing reset-incarnation guard
        // while the confirmed receipt is persisted; never recreate the row.
        return persist_cancelled_episode(ctx, &original, terminal_anchor_msg_id, raw_body);
    }
    if !mutation.persist(
''')
edit('src/services/discord/session_relay_sink/terminal_handoff.rs','''                .is_none()
            {
                Ok(SessionRelayDeliveryOutcome::NotDelivered)''','''                .is_none()
                && !self.cancelled_episode_is_retained(&delivery).await
            {
                Ok(SessionRelayDeliveryOutcome::NotDelivered)''')
p=Path(b+'entry.rs');s=p.read_text().replace('(&tmux_session_name)', '(tmux_session_name)').replace(', &tmux_session_name)', ', tmux_session_name)').replace('(&output_path)', '(output_path)').replace('            &shared,','            shared,').replace('            &tmux_session_name,','            tmux_session_name,');p.write_text(s)
