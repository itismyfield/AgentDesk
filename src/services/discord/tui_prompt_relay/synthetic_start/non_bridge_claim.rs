//! An idle observer yields its prompt only to a non-bridge owner proven to read the
//! observed source `E`; a refused delegation of the same turn goes back to the bridge.
use super::bridge_handoff::reown::{ObservedTurn, reown_refused_delegation};
use super::*;
use crate::services::discord::inflight::InflightEpisodePin;

#[cfg(test)]
mod strict_yield_tests;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::services::discord::tui_prompt_relay) enum SourceMatch {
    Match,
    Mismatch,
    Unknown,
}

pub(in crate::services::discord::tui_prompt_relay) enum NonBridgeWait {
    Yielded,
    /// `own_row` pins this turn's delegated row as the last poll read it, if there was one.
    Refused {
        own_row: Option<InflightEpisodePin>,
        reason: &'static str,
    },
}

#[derive(Debug)]
pub(in crate::services::discord::tui_prompt_relay) enum NonBridgeHandoff {
    Yielded,
    Reowned(ExternalInputRelayLease),
    Fallback,
}

#[cfg(test)]
pub(in crate::services::discord::tui_prompt_relay) static REVALIDATE_PAUSE: Mutex<
    Option<(u64, Arc<tokio::sync::Notify>, Arc<tokio::sync::Notify>)>,
> = Mutex::new(None);

/// The row fields a yield rests on. Progress (offsets, body, message rollover,
/// native session) is left out so a live turn does not fail its own recheck.
#[derive(PartialEq)]
struct RowCoords {
    provider: String,
    channel: u64,
    anchor: u64,
    started_at: String,
    nonce: Option<String>,
    turn_id: Option<String>,
    session_key: Option<String>,
    tmux: Option<String>,
    output: Option<String>,
    start: Option<u64>,
    source: TurnSource,
    owner: RelayOwnerKind,
    terminal: bool,
}

impl RowCoords {
    fn of(row: &InflightTurnState) -> Self {
        Self {
            provider: row.provider.clone(),
            channel: row.channel_id,
            anchor: row.user_msg_id,
            started_at: row.started_at.clone(),
            nonce: row.turn_nonce.clone(),
            turn_id: row.external_turn_id.clone(),
            session_key: row.session_key.clone(),
            tmux: row.tmux_session_name.clone(),
            output: row.output_path.clone(),
            start: row.turn_start_offset,
            source: row.turn_source,
            owner: row.effective_relay_owner_kind(),
            terminal: row.terminal_delivery_committed,
        }
    }
}

/// What one poll saw; the last one is logged and decides the result.
struct Poll {
    yielded: bool,
    reason: &'static str,
    own_row: Option<InflightEpisodePin>,
    owner: Option<RelayOwnerKind>,
    row_source: Option<String>,
    watcher_source: Option<String>,
    row_match: SourceMatch,
    watcher_match: SourceMatch,
    watcher_live: bool,
    producer_live: bool,
    row_revalidated: bool,
    watcher_revalidated: bool,
}

impl Poll {
    fn refused(reason: &'static str) -> Self {
        Self {
            yielded: false,
            reason,
            own_row: None,
            owner: None,
            row_source: None,
            watcher_source: None,
            row_match: SourceMatch::Unknown,
            watcher_match: SourceMatch::Unknown,
            watcher_live: false,
            producer_live: false,
            row_revalidated: false,
            watcher_revalidated: false,
        }
    }
}

/// `None`, an empty path, or one that does not resolve is unknown, never a match.
fn compare_source(expected: Option<&Path>, other: Option<&str>) -> SourceMatch {
    let other = other
        .filter(|path| !path.trim().is_empty())
        .and_then(|path| std::fs::canonicalize(path).ok());
    match (expected, other) {
        (Some(expected), Some(other)) if expected == other => SourceMatch::Match,
        (Some(_), Some(_)) => SourceMatch::Mismatch,
        _ => SourceMatch::Unknown,
    }
}

fn producer_live(tmux: &str) -> bool {
    crate::services::cluster::relay_producer_registry::global_relay_producer_registry()
        .get_live_producer(tmux)
        .is_some()
}

async fn poll_once(
    watchers: &super::super::super::TmuxWatcherRegistry,
    provider: &ProviderKind,
    channel: ChannelId,
    tmux: &str,
    expected: Option<&Path>,
    turn_id: &str,
) -> Poll {
    let read_row =
        || super::super::super::inflight::load_inflight_state_read_only(provider, channel.get());
    let Some(row) = read_row() else {
        return Poll::refused("no row");
    };
    if row.turn_source != TurnSource::ExternalInput
        || row.tmux_session_name.as_deref() != Some(tmux)
    {
        return Poll::refused("row is not this session's external input");
    }
    if turn_id.is_empty() || row.external_turn_id.as_deref() != Some(turn_id) {
        return Poll::refused("row belongs to another turn");
    }
    let owner = row.effective_relay_owner_kind();
    let mut poll = Poll::refused("row is not delegated");
    poll.owner = Some(owner);
    poll.row_source = row.output_path.clone();
    if !matches!(
        owner,
        RelayOwnerKind::Watcher | RelayOwnerKind::SessionBoundRelay
    ) {
        return poll;
    }
    poll.own_row = Some(InflightEpisodePin::from_state(&row));
    // Copy the watcher's path and cancel handle together, then resolve paths without holding it.
    let seen = watchers.by_tmux_session.get(tmux).map(|entry| {
        let live = !entry.cancel.load(Ordering::Relaxed) && !entry.heartbeat_stale();
        (entry.output_path.clone(), entry.cancel.clone(), live)
    });
    poll.watcher_source = seen.as_ref().map(|(path, _, _)| path.clone());
    poll.watcher_live = seen.as_ref().is_some_and(|(_, _, live)| *live);
    poll.producer_live = owner == RelayOwnerKind::SessionBoundRelay && producer_live(tmux);
    poll.row_match = compare_source(expected, row.output_path.as_deref());
    poll.watcher_match = compare_source(expected, poll.watcher_source.as_deref());
    poll.reason =
        if poll.row_match != SourceMatch::Match || poll.watcher_match != SourceMatch::Match {
            "owner does not read the expected source"
        } else if !poll.watcher_live {
            "watcher is not relay-live"
        } else if owner == RelayOwnerKind::SessionBoundRelay && !poll.producer_live {
            "session-bound owner has no live producer"
        } else {
            ""
        };
    let Some((watched, cancel, _)) = seen.filter(|_| poll.reason.is_empty()) else {
        return poll;
    };
    #[cfg(test)]
    super::bridge_handoff::pause_for_test(&REVALIDATE_PAUSE, channel).await;
    let coords = RowCoords::of(&row);
    poll.row_revalidated = read_row().is_some_and(|current| RowCoords::of(&current) == coords);
    poll.watcher_revalidated = watchers.by_tmux_session.get(tmux).is_some_and(|entry| {
        Arc::ptr_eq(&entry.cancel, &cancel)
            && entry.output_path == watched
            && !entry.cancel.load(Ordering::Relaxed)
            && !entry.heartbeat_stale()
    }) && (owner != RelayOwnerKind::SessionBoundRelay
        || producer_live(tmux));
    poll.yielded = poll.row_revalidated && poll.watcher_revalidated;
    poll.reason = match (poll.row_revalidated, poll.watcher_revalidated) {
        (true, true) => "non-bridge owner reads the expected source",
        (false, _) => "row changed before the yield",
        (true, false) => "watcher changed before the yield",
    };
    poll
}

/// Yield only to this turn's Watcher row, or session-bound row with a live producer, whose
/// output and live watcher both resolve to `expected_output_path` (`E`) and survive a recheck.
pub(in crate::services::discord::tui_prompt_relay) async fn wait_for_source_bound_non_bridge_claim(
    watchers: &super::super::super::TmuxWatcherRegistry,
    provider: &ProviderKind,
    channel: ChannelId,
    tmux: &str,
    expected_output_path: &Path,
    turn_id: &str,
) -> NonBridgeWait {
    let started = tokio::time::Instant::now();
    let deadline = started + TUI_DIRECT_SYNTHETIC_CLAIM_WAIT;
    // `E` resolves once; the rows and watchers it is compared with resolve on every poll.
    let expected = std::fs::canonicalize(expected_output_path).ok();
    loop {
        let poll = poll_once(
            watchers,
            provider,
            channel,
            tmux,
            expected.as_deref(),
            turn_id,
        )
        .await;
        let now = tokio::time::Instant::now();
        if poll.yielded || now >= deadline {
            tracing::info!(
                provider = provider.as_str(),
                channel_id = channel.get(),
                tmux_session_name = tmux,
                expected_source = %expected_output_path.display(),
                row_source = poll.row_source.as_deref().unwrap_or(""),
                watcher_source = poll.watcher_source.as_deref().unwrap_or(""),
                expected_row_match = ?poll.row_match,
                expected_watcher_match = ?poll.watcher_match,
                row_revalidated = poll.row_revalidated,
                watcher_revalidated = poll.watcher_revalidated,
                relay_owner_kind = ?poll.owner,
                watcher_live = poll.watcher_live,
                producer_live = poll.producer_live,
                yield_allowed = poll.yielded,
                reason = poll.reason,
                waited_ms = (now - started).as_millis() as u64,
                "TUI-direct idle observer settled whether a non-bridge owner reads the expected source"
            );
            return if poll.yielded {
                NonBridgeWait::Yielded
            } else {
                NonBridgeWait::Refused {
                    own_row: poll.own_row,
                    reason: poll.reason,
                }
            };
        }
        tokio::time::sleep(TUI_DIRECT_SYNTHETIC_CLAIM_POLL.min(deadline - now)).await;
    }
}

/// Yield, re-own this turn's refused delegation for the bridge, or fall back to the
/// caller's existing branch with its own lease. Only `Reowned` carries a lease to tail with.
pub(in crate::services::discord::tui_prompt_relay) async fn hand_off_observed_turn(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel: ChannelId,
    tmux: &str,
    expected_output_path: &Path,
    prompt_end: u64,
    lease: &ExternalInputRelayLease,
) -> NonBridgeHandoff {
    let turn_id = lease.turn_id.as_deref().unwrap_or_default();
    let wait = wait_for_source_bound_non_bridge_claim(
        &shared.tmux_watchers,
        provider,
        channel,
        tmux,
        expected_output_path,
        turn_id,
    )
    .await;
    let (handoff, reason) = match wait {
        NonBridgeWait::Yielded => (NonBridgeHandoff::Yielded, "yielded"),
        NonBridgeWait::Refused {
            own_row: None,
            reason,
        } => (NonBridgeHandoff::Fallback, reason),
        NonBridgeWait::Refused {
            own_row: Some(own_row),
            ..
        } => {
            let turn = ObservedTurn {
                provider,
                channel,
                tmux,
                expected_output: expected_output_path,
                prompt_end,
                lease,
            };
            match reown_refused_delegation(shared, &turn, &own_row).await {
                Ok(current) => (NonBridgeHandoff::Reowned(current), "re-owned"),
                Err(reason) => (NonBridgeHandoff::Fallback, reason),
            }
        }
    };
    tracing::info!(
        provider = provider.as_str(),
        channel_id = channel.get(),
        tmux_session_name = tmux,
        turn_id,
        handoff = match handoff {
            NonBridgeHandoff::Yielded => "yielded",
            NonBridgeHandoff::Reowned(_) => "reowned",
            NonBridgeHandoff::Fallback => "fallback",
        },
        reason,
        "TUI-direct idle observer handed off its observed turn"
    );
    handoff
}
