use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use poise::serenity_prelude::ChannelId;

use crate::config::TestEnvVarGuard;
use crate::services::discord::health::HealthRegistry;
use crate::services::discord::health::recovery::stall_watchdog_auto_heal_tests::{
    result_line, seed_idle_inflight,
};
use crate::services::discord::queue_io::cancel_backstop_test_support as support;
use crate::services::discord::relay_recovery::tests::orphan_token_finish::v2_attempted_front;
use crate::services::provider::ProviderKind;
use crate::test_support::wait::{WAIT_CAP, await_within};

// With no inflight user id to finish by, the repair still reports the input a V2 owner has
// waiting; a front an attempt carries is not waiting.
#[tokio::test(flavor = "current_thread")]
async fn unpinned_stale_turn_repair_reports_the_v2_owner_waiting_input() {
    let tempdir = tempfile::tempdir().expect("runtime root tempdir");
    let _env = TestEnvVarGuard::set_path("AGENTDESK_ROOT_DIR", tempdir.path());
    let provider = ProviderKind::Claude;
    let registry = HealthRegistry::new();
    let shared = crate::services::discord::make_shared_data_for_tests();
    registry
        .register(provider.as_str().to_string(), shared.clone())
        .await;
    let output = tempdir.path().join("unpinned-stale-turn.jsonl");
    std::fs::write(&output, result_line("")).expect("write ready output fixture");
    for (channel, waiting) in [(4_800_801, true), (4_800_802, false)] {
        let channel = ChannelId::new(channel);
        let tmux = format!("AgentDesk-claude-unpinned-{channel}");
        v2_attempted_front(&shared, &provider, channel, waiting).await;
        seed_idle_inflight(&provider, channel, 0, &tmux, &output, 0, "unpinned");

        let repair = super::super::clear_idle_tmux_stale_turn(
            &registry,
            provider.as_str(),
            channel.get(),
            &tmux,
            "unpinned_stale_turn_test",
        );
        let result = repair.await.expect("the unpinned repair runs");
        assert!(!result.had_active_turn);
        assert_eq!(result.has_pending_queue, waiting);
    }
}

/// Unhooks the channel's owner wake when the case ends.
struct WakeHook(ChannelId);

impl Drop for WakeHook {
    fn drop(&mut self) {
        support::set_owner_wake_for_tests(self.0, None);
    }
}

// A repair that removed the row and released no token resumes the owner once; a repair that
// finds no row to clear refuses and resumes nothing.
#[tokio::test(flavor = "current_thread")]
async fn a_stale_turn_repair_resumes_the_owner_once_after_its_row_clear() {
    check_stale_turn_resume(cfg!(unix), 7_753_301).await;
}

#[tokio::test(flavor = "current_thread")]
async fn stale_turn_repair_resumes_once_without_a_unix_input_owner() {
    check_stale_turn_resume(false, 7_753_303).await;
}

async fn check_stale_turn_resume(owner: bool, first_channel: u64) {
    let tempdir = tempfile::tempdir().expect("runtime root tempdir");
    let _env = TestEnvVarGuard::set_path("AGENTDESK_ROOT_DIR", tempdir.path());
    let provider = ProviderKind::Claude;
    let registry = HealthRegistry::new();
    let shared = crate::services::discord::make_shared_data_for_tests();
    registry
        .register(provider.as_str().to_string(), shared.clone())
        .await;
    let output = tempdir.path().join("resumed-stale-turn.jsonl");
    std::fs::write(&output, result_line("")).expect("write ready output fixture");
    for (channel, row) in [(first_channel, true), (first_channel + 1, false)] {
        let channel = ChannelId::new(channel);
        let tmux = format!("AgentDesk-claude-resumed-{channel}");
        if owner {
            v2_attempted_front(&shared, &provider, channel, true).await;
        }
        assert_eq!(
            crate::services::discord::queue_io::input_owner::closed::owned(&shared, channel).await,
            owner
        );
        if row {
            seed_idle_inflight(&provider, channel, 0, &tmux, &output, 0, "resumed");
        }
        let wakes = Arc::new(AtomicUsize::new(0));
        let seen = wakes.clone();
        let hook = Arc::new(move || {
            seen.fetch_add(1, Ordering::SeqCst);
        });
        support::set_owner_wake_for_tests(channel, Some(hook));
        let _unhook = WakeHook(channel);
        let completion = support::KickoffCompletion::install(channel);

        let repair = super::super::clear_idle_tmux_stale_turn(
            &registry,
            provider.as_str(),
            channel.get(),
            &tmux,
            "resumed_stale_turn_test",
        );
        let repaired = repair.await;
        await_within(completion.processed(0, WAIT_CAP), WAIT_CAP).await;
        assert_eq!(
            repaired.is_some(),
            row,
            "{channel}: the repair ran only on a row"
        );
        let expected = usize::from(row);
        assert_eq!(
            completion.started_requests(),
            expected,
            "repair resume requests"
        );
        if owner {
            assert_eq!(
                wakes.load(Ordering::SeqCst),
                expected,
                "{channel}: resume notices"
            );
        } else {
            assert_eq!(wakes.load(Ordering::SeqCst), 0, "no Unix owner installed");
        }
    }
}
