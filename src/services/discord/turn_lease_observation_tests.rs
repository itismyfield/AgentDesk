use super::*;
use crate::services::discord::{health::HealthRegistry, make_shared_data_for_tests_with_storage};

async fn i8432_assert_mailbox_unobservable(
    registry: &HealthRegistry,
    channel: ChannelId,
    request: ReleaseRequest,
    detail: &str,
) {
    let inspected = inspect(registry, &PROVIDER, channel)
        .await
        .map_err(|error| error.to_string());
    let released = release(registry, request)
        .await
        .map_err(|error| error.to_string());
    let reason = format!("unobservable: {detail}");
    assert_eq!((inspected, released), (Err(reason.clone()), Err(reason)));
}

#[tokio::test]
async fn i8432_inspect_and_release_refuse_unregistered_mailbox() {
    with_isolated_runtime_root(|| async {
        let shared = make_shared_data_for_tests_with_storage(None);
        let registry = HealthRegistry::new();
        registry.register("codex".into(), shared.clone()).await;
        let channel = ChannelId::new(843201);
        let request = seed(&shared, channel).await;
        shared.mailboxes.remove_fixture_for_test(channel);

        i8432_assert_mailbox_unobservable(&registry, channel, request, "mailbox is not registered")
            .await;
        assert!(shared.mailbox_peek(channel).is_none());
        assert!(inflight::load_inflight_state_read_only(&PROVIDER, channel.get()).is_some());
    })
    .await;
}

#[tokio::test]
async fn i8432_inspect_and_release_refuse_an_actor_without_a_snapshot() {
    with_isolated_runtime_root(|| async {
        let shared = make_shared_data_for_tests_with_storage(None);
        let registry = HealthRegistry::new();
        registry.register("codex".into(), shared.clone()).await;
        for (channel, closed) in [
            (ChannelId::new(843202), true),
            (ChannelId::new(843203), false),
        ] {
            let request = seed(&shared, channel).await;
            if closed {
                shared.mailboxes.insert_unreachable_for_test(channel);
            } else {
                shared.mailboxes.insert_reply_dropping_for_test(channel);
            }

            i8432_assert_mailbox_unobservable(
                &registry,
                channel,
                request,
                "mailbox actor did not answer",
            )
            .await;
            assert!(inflight::load_inflight_state_read_only(&PROVIDER, channel.get()).is_some());
        }
    })
    .await;
}

#[tokio::test]
async fn i8432_release_rechecks_unobservable_mailbox_after_failed_cas() {
    with_isolated_runtime_root(|| async {
        let shared = make_shared_data_for_tests_with_storage(None);
        let channel = ChannelId::new(843204);
        let request = seed(&shared, channel).await;
        let snapshot = shared.mailbox(channel).try_snapshot().await.unwrap();
        shared
            .mailboxes
            .insert_snapshot_until_finish_for_test(channel, snapshot);

        let result = release_on(&shared, &PROVIDER, channel, request)
            .await
            .map_err(|error| error.to_string());
        assert!(shared.mailbox_peek(channel).unwrap().is_closed());
        assert_eq!(
            result,
            Err("unobservable: mailbox actor did not answer".into())
        );
        assert!(inflight::load_inflight_state_read_only(&PROVIDER, channel.get()).is_some());
        assert_eq!(shared.restart.global_active.load(Ordering::Relaxed), 1);
    })
    .await;
}

#[tokio::test]
async fn i8432_observed_empty_mailbox_requires_the_inspected_runtime() {
    with_isolated_runtime_root(|| async {
        let shared = make_shared_data_for_tests_with_storage(None);
        let registry = HealthRegistry::new();
        registry.register("codex".into(), shared.clone()).await;
        let channel = ChannelId::new(843205);
        let request = seed(&shared, channel).await;
        let released = release(&registry, request.clone()).await.unwrap();
        assert_eq!(released["status"], "operator_released");
        assert_eq!(inspect(&registry, &PROVIDER, channel).await.unwrap(), None);
        let retry = release(&registry, request.clone()).await.unwrap();
        assert_eq!(
            retry,
            serde_json::json!({"released": false, "status": "already_released"})
        );

        let mut stale = request;
        stale.expected.runtime.push_str("-previous-runtime");
        assert!(release(&registry, stale).await.is_err());
    })
    .await;
}
