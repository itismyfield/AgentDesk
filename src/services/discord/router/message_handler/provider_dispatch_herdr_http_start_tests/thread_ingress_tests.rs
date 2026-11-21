use super::*;

const THREAD_PARENT: u64 = CHANNEL + 1;
const COLD_THREAD: u64 = CHANNEL + 2;
const FOREIGN_THREAD: u64 = CHANNEL + 3;
const FOREIGN_PARENT: u64 = CHANNEL + 4;

struct HostingFixture {
    previous: crate::config::Config,
    _lock: std::sync::MutexGuard<'static, ()>,
}

impl HostingFixture {
    fn new() -> Self {
        let lock = crate::services::provider_hosting::TEST_CONFIG_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let previous = crate::config_live_reload::current()
            .map(|config| (*config).clone())
            .unwrap_or_default();
        Self {
            previous,
            _lock: lock,
        }
    }

    fn install(&self) {
        let config = crate::config_live_reload::current().unwrap();
        crate::services::provider_hosting::install_provider_hosting_config(&config);
        let select =
            crate::services::provider_hosting::resolve_provider_session_selection_with_channel;
        assert!(!select(&ProviderKind::Claude, true, None).requested_tui_hosting);
        assert!(select(&ProviderKind::Claude, true, Some(CHANNEL)).requested_tui_hosting);
    }
}

impl Drop for HostingFixture {
    fn drop(&mut self) {
        crate::services::provider_hosting::install_provider_hosting_config(&self.previous);
    }
}

async fn thread_app(fx: &Fixture, intake: &Intake) -> Router {
    intake.mock.allow_channel(THREAD_PARENT);
    intake.mock.add_thread(CHANNEL, THREAD_PARENT);
    intake.mock.add_thread(COLD_THREAD, THREAD_PARENT);
    intake.mock.add_thread(FOREIGN_THREAD, FOREIGN_PARENT);
    intake.mock.allow_channel(FOREIGN_PARENT);
    intake.shared.settings.write().await.allowed_channel_ids =
        [THREAD_PARENT].into_iter().collect();
    crate::db::agents::insert_agent_channels_for_tests(
        &fx.pool,
        "busy-entry",
        Some(THREAD_PARENT_NAME),
        None,
    )
    .await;
    app(fx, intake).await
}

#[test]
fn external_thread_http_start_and_deliver_use_live_child_pg() {
    let fx = Fixture::new("thread-http-live", None);
    let launcher = Arc::new(Launcher::default());
    let (_, started) = reconnect::launch(&fx, &launcher);
    let before = fx.rig.sends().len();
    let _registry = fx.rig.registry_on_this_thread();
    let _admission = open_admission();
    let hosting = HostingFixture::new();
    let intake = Intake::new_thread(&fx, &started, THREAD_PARENT);
    hosting.install();
    let heads = answer_input(&fx, &intake, &started);
    let fallback = FallbackCount::new();
    intake.runtime.block_on(async {
        let app = thread_app(&fx, &intake).await;
        let cases = [
            (11, "start"),
            (11, "deliver"),
            (12, "start"),
            (12, "deliver"),
            (10, "start"),
            (10, "deliver"),
        ];
        for (i, (kind, endpoint)) in cases.into_iter().enumerate() {
            intake.mock.add_thread_kind(CHANNEL, THREAD_PARENT, kind);
            let origin = format!("external-thread-{kind}-{endpoint}");
            let result = call_target(&app, endpoint, &origin, "imessage", Some(CHANNEL)).await;
            assert_eq!(result.0, StatusCode::OK, "{endpoint}: {}", result.1);
            completed(&intake, i + 1).await;
            let sends = fx.rig.sends();
            assert_eq!(sends.len(), before + 2 * (i + 1));
            assert!(
                sends[before + 2 * i]["text"]
                    .as_str()
                    .unwrap()
                    .contains(INPUT)
            );
            assert_eq!(sends[before + 2 * i]["pane_id"], PANE);
            assert_eq!(sends[before + 2 * i + 1]["pane_id"], PANE);
            assert_eq!(sends[before + 2 * i + 1]["keys"], json!(["enter"]));
            assert_eq!(
                heads.lock().unwrap()[2 * i]["original"]["channel_id"],
                CHANNEL
            );
            if endpoint == "deliver" {
                assert_eq!(result.1["channel_id"], CHANNEL.to_string());
                intake
                    .shared
                    .restart
                    .reconcile_done
                    .store(false, Ordering::Release);
                let replay = call_target(&app, endpoint, &origin, "imessage", Some(CHANNEL)).await;
                assert_eq!(
                    replay, result,
                    "a replay does not reacquire child execution"
                );
                intake
                    .shared
                    .restart
                    .reconcile_done
                    .store(true, Ordering::Release);
            }
        }
        assert!(intake.posts.to(THREAD_PARENT).is_empty());
        assert_eq!(launcher.creates.load(Ordering::SeqCst), 1);
        fallback.assert_none();
    });
}

#[test]
fn external_thread_http_unavailable_has_no_effect_pg() {
    let fx = Fixture::new("thread-http-unavailable", None);
    let launcher = Arc::new(Launcher::default());
    let (_, started) = reconnect::launch(&fx, &launcher);
    let before = fx.rig.sends().len();
    let _registry = fx.rig.registry_on_this_thread();
    let _admission = open_admission();
    let hosting = HostingFixture::new();
    let intake = Intake::new_thread(&fx, &started, THREAD_PARENT);
    hosting.install();
    intake.runtime.block_on(async {
        let app = thread_app(&fx, &intake).await;
        let channels = || async {
            intake
                .shared
                .mailboxes
                .snapshot_all()
                .await
                .into_keys()
                .collect::<std::collections::BTreeSet<_>>()
        };
        let before_channels = channels().await;
        for endpoint in ["start", "deliver"] {
            let origin = format!("cold-thread-{endpoint}");
            let cold = call_target(&app, endpoint, &origin, "imessage", Some(COLD_THREAD)).await;
            assert_eq!(
                cold.0,
                StatusCode::SERVICE_UNAVAILABLE,
                "{endpoint}: {}",
                cold.1
            );
        }
        intake
            .shared
            .restart
            .reconcile_done
            .store(false, Ordering::Release);
        for endpoint in ["start", "deliver"] {
            let origin = format!("boot-thread-{endpoint}");
            let boot = call_target(&app, endpoint, &origin, "imessage", Some(CHANNEL)).await;
            assert_eq!(
                boot.0,
                StatusCode::SERVICE_UNAVAILABLE,
                "boot/{endpoint}: {}",
                boot.1
            );
        }
        intake
            .shared
            .restart
            .reconcile_done
            .store(true, Ordering::Release);
        sqlx::query(
            "INSERT INTO intake_session_owners
            (provider, raw_channel_id, owner_instance_id, generation)
            VALUES ('claude', $1, 'another-node', 1)",
        )
        .bind(CHANNEL.to_string())
        .execute(&fx.pool)
        .await
        .unwrap();
        for endpoint in ["start", "deliver"] {
            let remote =
                call_target(&app, endpoint, "remote-child", "imessage", Some(CHANNEL)).await;
            assert_eq!(
                remote.0,
                StatusCode::SERVICE_UNAVAILABLE,
                "{endpoint}: {}",
                remote.1
            );
            let foreign = call_target(
                &app,
                endpoint,
                "foreign-child",
                "imessage",
                Some(FOREIGN_THREAD),
            )
            .await;
            assert_eq!(
                foreign.0,
                StatusCode::FORBIDDEN,
                "{endpoint}: {}",
                foreign.1
            );
            let not_thread = call_target(
                &app,
                endpoint,
                "not-thread",
                "imessage",
                Some(FOREIGN_PARENT),
            )
            .await;
            assert_eq!(
                not_thread.0,
                StatusCode::FORBIDDEN,
                "{endpoint}: {}",
                not_thread.1
            );
            intake.mock.fail_channel(CHANNEL);
            let unread =
                call_target(&app, endpoint, "unread-child", "imessage", Some(CHANNEL)).await;
            intake.mock.heal_channel(CHANNEL);
            assert_eq!(
                unread.0,
                StatusCode::SERVICE_UNAVAILABLE,
                "{endpoint}: {}",
                unread.1
            );
            assert_eq!(unread.1["status"], "thread_parent_unverified");
            assert_eq!(unread.1["retryable"], true);
        }
        let origins: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM imsg_reply_obligations")
            .fetch_one(&fx.pool)
            .await
            .unwrap();
        let unexpected_sessions: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM sessions WHERE channel_id = ANY($1)")
                .bind([THREAD_PARENT.to_string(), COLD_THREAD.to_string()])
                .fetch_one(&fx.pool)
                .await
                .unwrap();
        assert_eq!(unexpected_sessions, 0);
        assert_eq!(channels().await, before_channels);
        assert_eq!(origins, 0);
        assert_eq!(fx.rig.sends().len(), before);
        assert_eq!(launcher.creates.load(Ordering::SeqCst), 1);
        assert!(intake.posts.to(CHANNEL).is_empty());
        assert!(intake.posts.to(THREAD_PARENT).is_empty());
        let state = intake
            .mailbox
            .v2_request(|slot| slot.as_ref().unwrap().json())
            .await
            .unwrap();
        assert_eq!(state["items"], json!([]));
        assert!(state["head_injection"].is_null());
    });
}

#[test]
fn external_thread_http_revalidates_herdr_pane_before_first_key_pg() {
    let fx = Fixture::new("thread-http-locator", None);
    let launcher = Arc::new(Launcher::default());
    let (_, started) = reconnect::launch(&fx, &launcher);
    let before = fx.rig.sends().len();
    let _registry = fx.rig.registry_on_this_thread();
    let _admission = open_admission();
    let hosting = HostingFixture::new();
    let intake = Intake::new_thread(&fx, &started, THREAD_PARENT);
    hosting.install();
    let fallback = FallbackCount::new();
    let context = match fx.record() {
        HostedRecord::Known(record) => context_of(&record.execution_nonce),
        other => panic!("{other:?}"),
    };
    intake.runtime.block_on(async {
        let app = thread_app(&fx, &intake).await;
        let (entered, release) = super::super::super::super::OwnerTurn::pause_late(CHANNEL);
        let reply = call_target(
            &app,
            "start",
            "late-thread-locator",
            "imessage",
            Some(CHANNEL),
        )
        .await;
        assert_eq!(reply.0, StatusCode::OK, "{}", reply.1);
        entered
            .recv_timeout(LIMIT)
            .expect("the actual owner reaches the pre-key observer");
        assert_eq!(fx.rig.sends().len(), before);
        fx.rig.run_provider(&context, true);
        release.send(()).unwrap();
        wait_for_idle_turn_to_unwind(&intake).await;
        assert_eq!(
            fx.rig.sends().len(),
            before,
            "the stale locator has no key effect"
        );
        assert_eq!(launcher.creates.load(Ordering::SeqCst), 1);
        assert!(
            !intake
                .posts
                .to(CHANNEL)
                .iter()
                .any(|post| post.contains("HTTP session answered"))
        );
        assert!(intake.posts.to(THREAD_PARENT).is_empty());
        fallback.assert_none();
    });
}
