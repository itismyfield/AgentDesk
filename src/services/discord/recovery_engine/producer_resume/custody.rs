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
        _ => return true,
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
