//! The stale mailbox repair's row clear resumes the channel's owner once when it removed a row, and
//! gives no notice when no row was there to remove.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use poise::serenity_prelude::ChannelId;

use crate::services::discord::queue_io::cancel_backstop_test_support as support;
use crate::services::discord::{self as discord, inflight};
use crate::services::provider::ProviderKind;
use crate::test_support::wait::{WAIT_CAP, await_within};

/// Unhooks the channel's owner wake when the case ends.
struct WakeHook(ChannelId);

impl Drop for WakeHook {
    fn drop(&mut self) {
        support::set_owner_wake_for_tests(self.0, None);
    }
}

#[tokio::test(flavor = "current_thread")]
async fn a_repair_row_clear_resumes_the_owner_once_only_when_it_removed_a_row() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let provider = ProviderKind::Claude;
    let shared = discord::make_shared_data_for_tests();
    let registry = discord::health::HealthRegistry::new();
    registry
        .register("claude".to_string(), shared.clone())
        .await;
    for (id, row) in [(7_753_401u64, true), (7_753_402, false)] {
        let channel = ChannelId::new(id);
        let dir = discord::runtime_store::discord_pending_queue_root().unwrap();
        let file = dir.join(format!("claude/repair-wake/{id}.json"));
        shared
            .mailbox(channel)
            .v2_adopt(file, "repair-wake".into())
            .await
            .unwrap();
        if row {
            let state = inflight::InflightTurnState::new(
                provider.clone(),
                id,
                None,
                1,
                id + 1,
                id + 2,
                "repaired".to_string(),
                None,
                Some(format!("repair-wake-{id}")),
                None,
                None,
                0,
            );
            let root = discord::runtime_store::discord_inflight_root().unwrap();
            let path = inflight::inflight_state_path(&root, &provider, id);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, serde_json::to_vec_pretty(&state).unwrap()).unwrap();
        }
        let wakes = Arc::new(AtomicUsize::new(0));
        let seen = wakes.clone();
        let hook = Arc::new(move || {
            seen.fetch_add(1, Ordering::SeqCst);
        });
        support::set_owner_wake_for_tests(channel, Some(hook));
        let _unhook = WakeHook(channel);
        let completion = support::KickoffCompletion::install(channel);

        // The stale mailbox repair's own sequence: clear the row, then resume what it held.
        if discord::clear_inflight_state_for_channel(&provider, id) {
            discord::health::repair_cleared_row(Some(&registry), &provider, id).await;
        }
        await_within(completion.processed(0, WAIT_CAP), WAIT_CAP).await;
        assert!(
            !inflight::inflight_state_file_exists(&provider, id),
            "{id}: no row left"
        );
        let expected = usize::from(row);
        assert_eq!(
            wakes.load(Ordering::SeqCst),
            expected,
            "{id}: resume notices"
        );
    }
}
