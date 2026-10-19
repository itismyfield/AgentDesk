use super::*;
use crate::services::discord::{outbound::delivery_record as records, turn_finalizer::TurnKey};
use std::sync::Weak;

#[derive(Clone)]
struct Custody {
    key: TurnKey,
    pin: InflightEpisodePin,
    cancel: Weak<AtomicBool>,
}
static CUSTODY: LazyLock<Mutex<HashMap<u64, Custody>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

pub(super) fn before_terminal(context: &Context) {
    let mut tickets = CUSTODY.lock().unwrap_or_else(|p| p.into_inner());
    tickets.retain(|_, ticket| ticket.cancel.strong_count() > 0);
    tickets.insert(
        context.channel.get(),
        Custody {
            key: context.completion_key,
            pin: context.pin.clone(),
            cancel: Arc::downgrade(&context.cancel),
        },
    );
}

/// A repaired terminal retains its original actor until the existing receipt writer commits.
pub(in crate::services::discord) fn waiting_for_receipt(
    shared: &SharedData,
    provider: &ProviderKind,
    key: TurnKey,
) -> bool {
    if *provider != ProviderKind::Codex {
        return false;
    }
    let ticket = CUSTODY
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .get(&key.channel_id.get())
        .cloned();
    let Some(ticket) = ticket.filter(|ticket| ticket.key.exact_key() == key.exact_key()) else {
        return false;
    };
    let Some(cancel) = ticket.cancel.upgrade() else {
        return false;
    };
    if cancel.load(Ordering::Acquire)
        || !shared
            .tmux_watchers
            .get(&key.channel_id)
            .is_some_and(|watcher| Arc::ptr_eq(&watcher.cancel, &cancel))
    {
        return false;
    }
    let state = match inflight::load_inflight_state_read_only_result(provider, key.channel_id.get())
    {
        Ok(Some(state)) => state,
        Ok(None) => return false,
        Err(_) => return true,
    };
    if !ticket
        .pin
        .is_same_episode_as(&InflightEpisodePin::from_state(&state))
    {
        return false;
    }
    !records::read_record(provider, state.delivery_record_owner_channel_id()).is_some_and(
        |record| {
            record.confirmed_deliveries.iter().any(|receipt| {
                let source = &receipt.source;
                source.turn_nonce == state.turn_nonce.as_deref().unwrap_or_default()
                    && Some(source.tmux_session_name.as_str()) == state.tmux_session_name.as_deref()
                    && Some(source.range.0) == state.turn_start_offset
                    && source.range.1 == state.last_offset
                    && records::current_generation_mtime_ns(&source.tmux_session_name)
                        == source.generation_mtime_ns
                    && records::confirmed_delivery_receipt_exists(
                        provider,
                        ChannelId::new(state.channel_id),
                        receipt.message_id,
                        source,
                    )
            })
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::discord::{self, TmuxWatcherHandle};
    use std::sync::atomic::{AtomicI64, AtomicU64};

    fn ticket(channel: u64) -> (Arc<SharedData>, InflightTurnState, TurnKey) {
        let shared = discord::make_shared_data_for_tests();
        let tmux = format!("custody-unit-{channel}");
        let mut row = InflightTurnState::new(
            ProviderKind::Codex,
            channel,
            None,
            7,
            channel + 1,
            channel + 2,
            "original prompt".into(),
            Some("custody-session".into()),
            Some(tmux.clone()),
            Some(tc::session_temp_path(&tmux, "jsonl")),
            None,
            0,
        );
        row.runtime_kind = Some(RuntimeHandoffKind::CodexTui);
        row.set_relay_owner_kind(RelayOwnerKind::Watcher);
        row.turn_nonce = Some(format!("custody-episode-{channel}"));
        let key = TurnKey::new(
            ChannelId::new(channel),
            row.effective_finalizer_turn_id(),
            shared.restart.current_generation,
        )
        .with_episode_nonce(row.turn_nonce.as_deref());
        let cancel = Arc::new(AtomicBool::new(false));
        shared.tmux_watchers.insert(
            key.channel_id,
            TmuxWatcherHandle {
                tmux_session_name: tmux,
                output_path: row.output_path.clone().unwrap(),
                paused: Arc::new(AtomicBool::new(false)),
                resume_offset: Arc::new(Mutex::new(None)),
                cancel: cancel.clone(),
                pause_epoch: Arc::new(AtomicU64::new(0)),
                turn_delivered: Arc::new(AtomicBool::new(false)),
                last_heartbeat_ts_ms: Arc::new(AtomicI64::new(0)),
            },
        );
        CUSTODY.lock().unwrap_or_else(|p| p.into_inner()).insert(
            channel,
            Custody {
                key,
                pin: InflightEpisodePin::from_state(&row),
                cancel: Arc::downgrade(&cancel),
            },
        );
        (shared, row, key)
    }

    #[test]
    fn retired_row_releases_live_custody_ticket() {
        let _root = crate::config::TestRuntimeRootGuard::new();
        let (shared, row, key) = ticket(7_139_161);
        assert!(
            inflight::load_inflight_state_read_only(&ProviderKind::Codex, row.channel_id).is_none()
        );
        assert!(
            !waiting_for_receipt(&shared, &ProviderKind::Codex, key),
            "a retired row leaves no episode for its live custody ticket to retain"
        );
    }

    #[test]
    fn live_row_without_receipt_retains_custody_ticket() {
        let _root = crate::config::TestRuntimeRootGuard::new();
        let (shared, row, key) = ticket(7_139_162);
        inflight::save_inflight_state(&row).unwrap();
        assert!(records::read_record(&ProviderKind::Codex, row.channel_id).is_none());
        assert!(waiting_for_receipt(&shared, &ProviderKind::Codex, key));
    }
}
