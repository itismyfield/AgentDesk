//! Sweeps a user message's `⏳` that no live turn holds any more. Holds are judged per message
//! across the provider: a busy channel nearly always has a later turn in flight, and a thread
//! turn's row lives in its execution thread while the `⏳` stays on the original message.

use std::collections::HashSet;
use std::time::{Duration, SystemTime};

use super::orphan_sweep::PendingAnchorCandidate;
use super::*;
use crate::services::discord::{inflight, tui_direct_pending_start};
use crate::services::provider::ProviderKind;

/// Same grace as the bot-anchor sweep, so a slow-starting turn keeps its `⏳`.
const INTAKE_MIN_AGE_SECS: u64 = 900;
/// Removals per pass; a large backlog drains over several passes instead of bursting into 429s.
const INTAKE_MAX_PER_PASS: usize = 8;

/// Periodic entry: removes aged intake `⏳` whose message no inflight row, mailbox, pending start
/// or unsettled native input holds.
pub(in crate::services::discord) async fn sweep_orphan_intake_reactions(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
) -> usize {
    let reconciler = &shared.turn_view_reconciler;
    let min_age = Duration::from_secs(INTAKE_MIN_AGE_SECS);
    let kind = TurnViewTargetKind::IntakeUserMessage;
    let candidates: Vec<_> =
        (reconciler.persisted_pending_targets(shared, SystemTime::now(), kind))
            .into_iter()
            .filter(|candidate| candidate.attached_age >= min_age)
            .collect();
    if candidates.is_empty() {
        return 0;
    }
    let mailbox = shared.mailbox_held_messages().await;
    let native = native_held_messages(provider);
    let turns = turn_held_messages(provider);
    let held = |channel: u64, message: u64| {
        mailbox.contains(&message)
            || native.as_ref().is_none_or(|ids| ids.contains(&message))
            || turns.contains(&message)
            || inflight::load_inflight_state_read_only_result(provider, channel).is_err()
    };
    // Under the target lock: a turn or a restart of this message since the snapshot holds it.
    let held_now = |channel: u64, message: u64| {
        held(channel, message)
            || turn_held_messages(provider).contains(&message)
            || restarted_within(channel, message, min_age)
    };
    let mut cleared = 0;
    let unheld = candidates
        .into_iter()
        .filter(|candidate| !held(candidate.channel_id, candidate.message_id));
    for candidate in unheld.take(INTAKE_MAX_PER_PASS) {
        let PendingAnchorCandidate {
            channel_id,
            message_id,
            attached_age,
        } = candidate;
        let target = TurnViewTarget::intake_user_message(
            ChannelId::new(channel_id),
            MessageId::new(message_id),
        );
        if (reconciler.clear_orphan_pending(shared, target, &held_now, "orphan_intake_sweep")).await
        {
            cleared += 1;
            tracing::warn!(
                channel_id,
                message_id,
                attached_age_secs = attached_age.as_secs(),
                "swept orphan intake ⏳: no live turn holds the message"
            );
        }
    }
    cleared
}

/// Messages any of the provider's inflight rows or pending starts names, in any channel.
fn turn_held_messages(provider: &ProviderKind) -> HashSet<u64> {
    let rows = inflight::load_inflight_states_for_sweep(provider);
    let row_messages = rows
        .into_iter()
        .flat_map(|(row, _)| std::iter::once(row.user_msg_id).chain(row.source_message_ids));
    let starts = tui_direct_pending_start::load_all().into_iter();
    let started = starts
        .filter(|record| record.provider.eq_ignore_ascii_case(provider.as_str()))
        .map(|record| record.anchor_message_id);
    row_messages.chain(started).collect()
}

/// The message's `⏳` record was rewritten within `min_age`, as a fresh start of it does.
fn restarted_within(channel: u64, message: u64, min_age: Duration) -> bool {
    let target =
        TurnViewTarget::intake_user_message(ChannelId::new(channel), MessageId::new(message));
    let modified = TurnViewReconciler::persisted_target_path(target)
        .and_then(|path| std::fs::metadata(path).ok()?.modified().ok());
    modified.is_some_and(|at| at.elapsed().unwrap_or_default() < min_age)
}

/// Messages of native inputs whose outcome is not settled yet; `None` when unreadable.
fn native_held_messages(provider: &ProviderKind) -> Option<HashSet<u64>> {
    use crate::services::tui_input::human_frame::history::v2::native::boot_records;
    let path = runtime_store::runtime_root()?.join("injected_frames.json");
    let records = boot_records(&path, provider.as_str()).ok()?;
    let unsettled = records
        .iter()
        .filter(|record| record["settlement"]["outcome"].is_null());
    Some(
        unsettled
            .flat_map(|record| {
                let ticket = &record["settlement"]["ticket"];
                [
                    ticket["owning_episode"]["user_message_id"].as_u64(),
                    ticket["key"]["identity"]["primary"].as_u64(),
                ]
            })
            .flatten()
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Holds are per message wherever the turn runs: the original channel's later turn never
    /// blocks the sweep, while the message's own turn in its execution thread keeps its `⏳`.
    #[tokio::test]
    async fn sweep_clears_by_message_not_by_channel() {
        let _root = crate::config::TestRuntimeRootGuard::new();
        let shared = crate::services::discord::make_shared_data_for_tests();
        let http = Arc::new(serenity::http::Http::new("test-token"));
        let (origin, thread) = (1_430_000_000_000_008_489_u64, 1_430_000_000_000_008_499_u64);
        let (ended, live) = (origin + 1, origin + 3);
        for message in [ended, live] {
            let (channel_id, message_id) = (ChannelId::new(origin), MessageId::new(message));
            assert!(note_intake_turn_started(&shared, &http, channel_id, message_id, 1, "t").await);
            let path = TurnViewReconciler::persisted_target_path(
                TurnViewTarget::intake_user_message(channel_id, message_id),
            );
            crate::services::discord::health::test_support::age_file(&path.unwrap(), 1_000);
        }
        for (channel, row_message) in [(origin, ended + 5), (thread, live)] {
            let row = inflight::InflightTurnState::new(
                shared.provider.clone(),
                channel,
                None,
                1,
                row_message,
                row_message + 1,
                "prompt".into(),
                None,
                None,
                None,
                None,
                0,
            );
            inflight::save_inflight_state(&row).unwrap();
        }

        assert_eq!(
            sweep_orphan_intake_reactions(&shared, &shared.provider).await,
            1
        );
        let removed = |message: u64| {
            (shared.turn_view_reconciler.ops().iter())
                .any(|op| op.target.message_id.get() == message && !op.add && op.emoji == '⏳')
        };
        assert!(
            removed(ended),
            "another message's live turn does not hold this ⏳"
        );
        assert!(!removed(live), "the message's thread turn keeps its ⏳");
    }
}
