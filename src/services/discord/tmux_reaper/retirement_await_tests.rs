use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use futures::future::BoxFuture;
use poise::serenity_prelude::ChannelId;
use tokio::sync::Notify;

use super::tests::{admit_any_host, seed_busy_channel, still_busy};
use crate::config::TestEnvVarGuard;
use crate::services::discord::health::legacy_supervision::RetiredForTest;
use crate::services::discord::health::legacy_supervision::test_support::fingerprint;
use crate::services::provider::ProviderKind;

async fn final_probe_case(periodic: bool, retire: bool, channel: u64) {
    let temp = tempfile::tempdir().unwrap();
    let _root =
        TestEnvVarGuard::set_path_after_shared_test_env_lock("AGENTDESK_ROOT_DIR", temp.path());
    let shared = crate::services::discord::make_shared_data_for_tests();
    shared.settings.write().await.provider = ProviderKind::Claude;
    let token = seed_busy_channel(&shared, channel).await;
    shared.restart.global_active.store(1, Ordering::Relaxed);
    let root = crate::services::discord::runtime_store::discord_inflight_root().unwrap();
    let row = crate::services::discord::inflight::inflight_state_path(
        &root,
        &ProviderKind::Claude,
        channel,
    );
    let before = fingerprint(&row);
    assert!(before.is_some());
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let calls = Arc::new(AtomicUsize::new(0));
    let probe = {
        let (entered, release, calls) = (entered.clone(), release.clone(), calls.clone());
        move |_: String| -> BoxFuture<'static, bool> {
            let (entered, release, calls) = (entered.clone(), release.clone(), calls.clone());
            Box::pin(async move {
                if calls.fetch_add(1, Ordering::Relaxed) == 1 {
                    entered.notify_one();
                    release.notified().await;
                }
                false
            })
        }
    };
    let mut retired = None;
    let work = async {
        if periodic {
            super::reap_stale_busy_mailboxes_with_probe(&shared, &probe, &admit_any_host).await;
        } else {
            let healed = super::heal_stale_busy_mailbox_with_probe(
                &shared,
                &ProviderKind::Claude,
                ChannelId::new(channel),
                "intake_await_test",
                false,
                &probe,
                &admit_any_host,
            )
            .await;
            assert_eq!(healed, !retire);
        }
    };
    let transition = async {
        entered.notified().await;
        assert_eq!(calls.load(Ordering::Relaxed), 2);
        assert!(still_busy(&shared, channel).await);
        if retire {
            retired = Some(RetiredForTest::new("claude", channel));
        }
        release.notify_one();
    };
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        tokio::join!(work, transition);
    })
    .await
    .expect("final probe barrier");
    assert_eq!(still_busy(&shared, channel).await, retire);
    assert_eq!(token.cancelled.load(Ordering::Relaxed), !retire);
    assert_eq!(
        shared.restart.global_active.load(Ordering::Relaxed),
        usize::from(retire)
    );
    if retire {
        assert_eq!(fingerprint(&row), before);
    } else {
        assert!(
            fingerprint(&row).is_none(),
            "finalizer clears the legacy row"
        );
    }
}

#[tokio::test(flavor = "current_thread")]
async fn intake_rechecks_retirement_after_final_probe() {
    let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    final_probe_case(false, true, 6_325_412_001).await;
    final_probe_case(false, false, 6_325_412_002).await;
}

#[tokio::test(flavor = "current_thread")]
async fn periodic_rechecks_retirement_after_final_probe() {
    let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    final_probe_case(true, true, 6_325_412_003).await;
    final_probe_case(true, false, 6_325_412_004).await;
}
