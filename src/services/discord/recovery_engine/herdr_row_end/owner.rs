//! Which durable rows a prior process's Herdr turn owns: new idle starts wait on them, and only
//! their dedicated owners end them, never a generic sweep.

use super::*;
use crate::services::session_host::GrantedServer;

/// Why a durable row keeps new idle starts off its channel.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::services::discord) enum RowHold {
    RestartedHerdrRow,
    /// The row did not read; an unread row is never taken for none.
    Unread,
    /// The row's execution ran on a server that is gone or is not the one its grant admitted.
    ServerReplaced,
}

impl RowHold {
    pub(in crate::services::discord) fn reason(self) -> &'static str {
        match self {
            Self::RestartedHerdrRow => "restarted_herdr_row",
            Self::Unread => "restarted_herdr_row_unread",
            Self::ServerReplaced => "herdr_server_replaced",
        }
    }
}

/// The channel's fresh durable row, when a prior process's Herdr turn holds it.
pub(in crate::services::discord) fn restarted_herdr_hold(
    provider: &ProviderKind,
    channel: u64,
    current: u64,
) -> Option<RowHold> {
    inflight::inflight_runtime_root()?;
    if !hosted_provider(provider) {
        return None;
    }
    match inflight::load_inflight_state_read_only_result(provider, channel) {
        Ok(row) => restarted_owner(provider, &row?, current),
        Err(_) => Some(RowHold::Unread),
    }
}

/// The automatic dequeue's hold: logs it, asks for the row's end, and arms the slow backstop so a
/// clear another owner makes is still read.
pub(in crate::services::discord) async fn hold_queue(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel: ChannelId,
) -> bool {
    let current = shared.restart.current_generation;
    let restarted = restarted_herdr_hold(provider, channel.get(), current);
    let hold = match restarted {
        Some(hold) => hold,
        None => match server_replaced_hold(shared, provider, channel).await {
            Some(hold) => hold,
            None => return false,
        },
    };
    tracing::info!(
        provider = provider.as_str(),
        channel_id = channel.get(),
        reason = hold.reason(),
        "dequeue_held"
    );
    if restarted.is_some() {
        // The end task may already be a pass's, which never starts a server; ask without waiting.
        super::super::herdr_server_replaced::demand(channel.get());
        request_row_end(shared, provider, channel, RowEndAsk::Input);
    } else {
        super::super::herdr_server_replaced::request(shared, provider, channel);
    }
    let arm = crate::services::discord::arm_slow_idle_queue_backstop_if_queue_nonempty;
    arm(shared, provider, channel, "restarted herdr row hold").await;
    true
}

/// Holds inputs while the row's granted server is absent or replaced; only witnesses are
/// compared, so a kept server's policy never holds them.
pub(in crate::services::discord) async fn server_replaced_hold(
    shared: &SharedData,
    provider: &ProviderKind,
    channel: ChannelId,
) -> Option<RowHold> {
    use super::super::herdr_server_replaced as replaced;
    if !hosted_provider(provider) {
        return None;
    }
    let launch = replaced::local_endpoint(channel.get())?;
    let pool = shared.pg_pool.as_ref()?;
    let row = replaced::canonical_row(pool, &shared.token_hash, provider, channel).await?;
    let nonce = replaced::on_endpoint(&row, &launch)?;
    let held = match replaced::granted_server(&launch, &nonce).await? {
        GrantedServer::Kept => false,
        GrantedServer::Replaced(_) => true,
        GrantedServer::Unanswered if replaced::absent(&launch).await => {
            replaced::demand(channel.get());
            true
        }
        GrantedServer::Unanswered => false,
    };
    held.then_some(RowHold::ServerReplaced)
}

/// REST's hold, read against this process's generation.
pub(in crate::services::discord) fn holds_rest(provider: &ProviderKind, channel: u64) -> bool {
    let current = crate::services::discord::runtime_store::process_generation();
    restarted_herdr_hold(provider, channel, current).is_some()
}

/// A prior process's row on a Herdr host: its recorded execution, else its session's host marker.
fn restarted_owner(
    provider: &ProviderKind,
    row: &inflight::InflightTurnState,
    current: u64,
) -> Option<RowHold> {
    if row.channel_id == 0 || inflight::row_is_current_generation(row, current) {
        return None;
    }
    let herdr = row.hosted_execution_nonce.is_some()
        || recovery_tmux_session_name(provider, row)
            .is_some_and(|name| crate::services::discord::turn_bridge::herdr_marked(&name));
    herdr.then_some(RowHold::RestartedHerdrRow)
}

/// A prior process's Herdr row whose end an owner other than a generic sweep settles: the end
/// pass, an admitted terminal, a before-start receipt, a hand-off, a rebind or a replay.
pub(in crate::services::discord) fn dedicated_cleanup_owner(
    provider: &ProviderKind,
    row: &inflight::InflightTurnState,
    current: u64,
) -> bool {
    hosted_provider(provider)
        && restarted_owner(provider, row, current) == Some(RowHold::RestartedHerdrRow)
        && (target(provider, row, current)
            || row.tui_terminal_kind.is_some()
            || row.herdr_before_start_stop.is_some()
            || inflight::before_start_receipt_present(provider, row.channel_id)
            || row.restart_mode == Some(InflightRestartMode::HotSwapHandoff)
            || row.rebind_origin
            || row.replay_rerun_blocked())
}
