use std::sync::Arc;

use poise::serenity_prelude::{self as serenity, ChannelId, MessageId};

use super::super::clear_persist_failure_tests::{
    SESSION_ID, boundary_rows, queue_len, seed_backlog, seed_session, session_state,
};
use super::super::{SoftClearNotifyMode, clear_channel_session_state};
use super::*;
use crate::db::o_channel_homes::HomeState;
use crate::services::provider::ProviderKind;

/// What a refused clear must have left alone: the queued backlog, the provider session, the
/// transcript boundary and the session transition.
async fn untouched(shared: &Arc<crate::services::discord::SharedData>, pool: &sqlx::PgPool) {
    let channel_id = ChannelId::new(CHANNEL);
    assert_eq!(queue_len(shared, channel_id).await, 1);
    let session = session_state(shared, channel_id).await;
    assert_eq!(session, (Some(SESSION_ID.to_string()), false));
    assert!(boundary_rows(pool, channel_id).await.is_empty());
    let transition = shared.session_transition_lock(channel_id).try_lock_owned();
    assert!(transition.is_ok(), "no transition was taken");
}

const CHANNEL: u64 = 9_200_000_000_000_101;

// The `/clear` and `!clear` entry: refused before any change while the home drains or is not held
// here; once no home is registered for the channel the clear runs as before.
#[test]
fn a_delegated_channel_clear_is_refused_unless_its_home_is_held_with_intake_open_pg() {
    let root = tempfile::tempdir().expect("scratch runtime root");
    let _env = crate::config::TestEnvVarGuard::set_path("AGENTDESK_ROOT_DIR", root.path());
    let provider = ProviderKind::Gemini;
    let channel_id = ChannelId::new(CHANNEL);
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("test runtime");
    runtime.block_on(async {
        let db = crate::dispatch::test_support::DispatchPostgresTestDb::create(
            "agentdesk_clear_home_fence",
            "delegated channel clear fence",
        )
        .await;
        let pool = db.connect_and_migrate_with_max_connections(4).await;
        let shared =
            crate::services::discord::make_shared_data_for_tests_with_storage(Some(pool.clone()));
        seed_session(&shared, channel_id).await;
        seed_backlog(&shared, &provider, channel_id).await;
        let http = Arc::new(
            serenity::HttpBuilder::new("test-token")
                .proxy("http://127.0.0.1:1")
                .ratelimiter_disabled(true)
                .build(),
        );
        let clear = || {
            let source = "!clear";
            let notify = SoftClearNotifyMode::Suppress;
            clear_channel_session_state(&http, &shared, &provider, channel_id, source, notify)
        };
        let refused = |result: anyhow::Result<()>| {
            let error = result.expect_err("refused");
            *error
                .downcast_ref::<CommandRefused>()
                .expect("typed refusal")
        };

        let home = channel_home::register_for_test(CHANNEL, Some(HomeState::Releasing));
        let draining = refused(clear().await);
        assert_eq!(draining, CommandRefused(HomeRefusal::Draining));
        untouched(&shared, &pool).await;
        home.close();
        let not_held = refused(clear().await);
        assert_eq!(not_held, CommandRefused(HomeRefusal::NotHeld));
        untouched(&shared, &pool).await;

        channel_home::register_for_test(CHANNEL + 1, Some(HomeState::Worker));
        channel_home::unregister(&CHANNEL.to_string());
        let cleared = clear().await;
        assert!(cleared.is_ok(), "{cleared:?}");
        assert_eq!(queue_len(&shared, channel_id).await, 0);
        assert_eq!(session_state(&shared, channel_id).await, (None, true));
        assert_eq!(boundary_rows(&pool, channel_id).await, [1]);

        pool.close().await;
        db.drop().await;
    });
}

#[tokio::test(flavor = "current_thread")]
async fn d2b_reset_sink_refuses_claimed_turn_and_drain_reset_remains_allowed() {
    let root = tempfile::tempdir().unwrap();
    let _env = crate::config::TestEnvVarGuard::set_path("AGENTDESK_ROOT_DIR", root.path());
    let shared = crate::services::discord::make_shared_data_for_tests_with_storage(None);
    let channel = ChannelId::new(CHANNEL + 10);
    seed_session(&shared, channel).await;
    let provider = ProviderKind::Gemini;
    let http = Arc::new(
        serenity::HttpBuilder::new("test-token")
            .proxy("http://127.0.0.1:1")
            .ratelimiter_disabled(true)
            .build(),
    );
    let home = channel_home::register_for_test(channel.get(), Some(HomeState::Worker));
    let token = Arc::new(crate::services::provider::CancelToken::new());
    shared
        .mailbox(channel)
        .restore_active_turn(
            token.clone(),
            serenity::UserId::new(7),
            serenity::MessageId::new(123),
        )
        .await;
    shared.overrides.model_session_reset_pending.insert(channel);
    home.close_intake();
    let reset = super::super::reset_provider_session_if_pending(
        &http, &shared, &provider, channel, channel,
    )
    .await
    .unwrap();
    assert_eq!(
        reset,
        super::super::ManagedReset::Refused("home_draining".into())
    );
    assert_eq!(
        session_state(&shared, channel).await.0.as_deref(),
        Some(SESSION_ID)
    );
    assert!(
        shared
            .overrides
            .model_session_reset_pending
            .contains(&channel)
    );
    assert!(Arc::ptr_eq(
        &shared
            .mailbox(channel)
            .snapshot()
            .await
            .cancel_token
            .unwrap(),
        &token
    ));
    let reset = super::super::reset_channel_provider_state_for_home_drain(
        &http,
        &shared,
        &provider,
        channel,
        "channel home release",
        true,
        false,
        false,
    )
    .await;
    assert!(matches!(reset, super::super::ManagedReset::Applied(_)));
    assert_eq!(session_state(&shared, channel).await.0, None);
}

#[tokio::test(flavor = "current_thread")]
async fn d2b_clear_and_reset_keep_admission_across_effect_await() {
    let root = tempfile::tempdir().unwrap();
    let _env = crate::config::TestEnvVarGuard::set_path("AGENTDESK_ROOT_DIR", root.path());
    for label in ["clear", "reset"] {
        let shared = crate::services::discord::make_shared_data_for_tests_with_storage(None);
        let channel = ChannelId::new(CHANNEL + 11);
        seed_session(&shared, channel).await;
        let home = channel_home::register_for_test(channel.get(), Some(HomeState::Worker));
        let barrier = Arc::new((tokio::sync::Notify::new(), tokio::sync::Notify::new()));
        PAUSE.with(|slot| *slot.borrow_mut() = Some((label, barrier.clone())));
        let http = Arc::new(
            serenity::HttpBuilder::new("test-token")
                .proxy("http://127.0.0.1:1")
                .ratelimiter_disabled(true)
                .build(),
        );
        let work = tokio::spawn(async move {
            if label == "clear" {
                super::super::clear_channel_session_state_fenced(
                    &http,
                    &shared,
                    &ProviderKind::Gemini,
                    channel,
                    "!clear",
                    SoftClearNotifyMode::Suppress,
                    None,
                )
                .await
                .unwrap();
            } else {
                assert!(matches!(
                    super::super::reset_channel_provider_state(
                        &http,
                        &shared,
                        &ProviderKind::Gemini,
                        channel,
                        "pending",
                        true,
                        false,
                        false
                    )
                    .await,
                    super::super::ManagedReset::Applied(_)
                ));
            }
            assert_eq!(session_state(&shared, channel).await.0, None);
        });
        tokio::time::timeout(std::time::Duration::from_secs(10), barrier.0.notified())
            .await
            .expect("effect boundary reached");
        home.close_intake();
        assert_eq!(
            home.commands_in_flight(),
            1,
            "effect suspended after admission"
        );
        assert!(home.admit_command("gemini").is_none());
        barrier.1.notify_one();
        work.await.unwrap();
        assert_eq!(home.commands_in_flight(), 0, "actual effect completed");
        PAUSE.with(|slot| *slot.borrow_mut() = None);
        channel_home::unregister(&channel.get().to_string());
    }
}

#[tokio::test(flavor = "current_thread")]
async fn d2b_slash_stop_reply_keeps_permit_until_finish_and_dormant_is_unchanged() {
    let root = tempfile::tempdir().unwrap();
    let _env = crate::config::TestEnvVarGuard::set_path("AGENTDESK_ROOT_DIR", root.path());
    let shared = crate::services::discord::make_shared_data_for_tests_with_storage(None);
    let channel = ChannelId::new(CHANNEL + 12);
    let provider = ProviderKind::Gemini;
    let legacy =
        crate::services::discord::commands::stop::run_slash_stop(&shared, &provider, channel).await;
    assert_eq!(
        legacy.text(),
        crate::services::discord::commands::NO_ACTIVE_TURN_RESPONSE
    );
    legacy.finish(&shared, &provider, channel).await;
    let home = channel_home::register_for_test(channel.get(), Some(HomeState::Worker));
    let reply =
        crate::services::discord::commands::stop::run_slash_stop(&shared, &provider, channel).await;
    home.close_intake();
    assert_eq!(home.commands_in_flight(), 1);
    reply.finish(&shared, &provider, channel).await;
    assert_eq!(home.commands_in_flight(), 0);
    let refused =
        crate::services::discord::commands::stop::run_slash_stop(&shared, &provider, channel).await;
    assert_eq!(refused.text(), "home_draining");
}

#[tokio::test(flavor = "current_thread")]
async fn d2b_queued_cancel_keeps_scope_and_never_cancels_active_successor() {
    let root = tempfile::tempdir().unwrap();
    let _env = crate::config::TestEnvVarGuard::set_path("AGENTDESK_ROOT_DIR", root.path());
    let shared = crate::services::discord::make_shared_data_for_tests_with_storage(None);
    let provider = ProviderKind::Gemini;
    let channel = ChannelId::new(CHANNEL + 15);
    seed_backlog(&shared, &provider, channel).await;
    let token = Arc::new(crate::services::provider::CancelToken::new());
    shared
        .mailbox(channel)
        .restore_active_turn(
            token.clone(),
            serenity::UserId::new(7),
            serenity::MessageId::new(123),
        )
        .await;
    let home = channel_home::register_for_test(channel.get(), Some(HomeState::Worker));
    let barrier = Arc::new((tokio::sync::Notify::new(), tokio::sync::Notify::new()));
    PAUSE.with(|slot| *slot.borrow_mut() = Some(("queued", barrier.clone())));
    let owner = shared.clone();
    let work = tokio::spawn(async move {
        super::super::cancel_queued_with_home_permit(
            &owner,
            &ProviderKind::Gemini,
            channel,
            MessageId::new(channel.get() + 1),
        )
        .await
        .unwrap()
    });
    tokio::time::timeout(std::time::Duration::from_secs(10), barrier.0.notified())
        .await
        .expect("effect boundary reached");
    home.close_intake();
    assert_eq!(home.commands_in_flight(), 1);
    assert_eq!(queue_len(&shared, channel).await, 1);
    barrier.1.notify_one();
    assert!(work.await.unwrap().is_some());
    assert_eq!(queue_len(&shared, channel).await, 0);
    assert_eq!(home.commands_in_flight(), 0);
    assert!(Arc::ptr_eq(
        &shared
            .mailbox(channel)
            .snapshot()
            .await
            .cancel_token
            .unwrap(),
        &token
    ));
    PAUSE.with(|slot| *slot.borrow_mut() = None);
    assert!(
        super::super::cancel_queued_with_home_permit(
            &shared,
            &provider,
            channel,
            MessageId::new(123)
        )
        .await
        .is_err()
    );
    assert!(!token.cancelled.load(std::sync::atomic::Ordering::Acquire));
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn d2b_clear_reply_stays_counted_after_clear_body_finishes() {
    use crate::services::discord::admin_host_guard::tests::{Recorder, api_child};
    if !api_child(
        "services::discord::commands::control::home_fence::tests::d2b_clear_reply_stays_counted_after_clear_body_finishes",
    ) {
        return;
    }
    let api = Recorder::start().await;
    crate::services::discord::internal_api::init(api.port, None);
    let http = api.http.clone();

    let root = tempfile::tempdir().unwrap();
    let _env = crate::config::TestEnvVarGuard::set_path("AGENTDESK_ROOT_DIR", root.path());
    let shared = crate::services::discord::make_shared_data_for_tests_with_storage(None);
    let channel = ChannelId::new(CHANNEL + 16);
    seed_session(&shared, channel).await;
    let home = channel_home::register_for_test(channel.get(), Some(HomeState::Worker));
    let permit = admit(channel, "gemini").unwrap();
    let barrier = Arc::new((tokio::sync::Notify::new(), tokio::sync::Notify::new()));
    let gate = barrier.clone();
    let owner = shared.clone();
    let work = tokio::spawn(super::super::clear_with_reply_scope(permit, async move {
        super::super::clear_channel_session_state_fenced(
            &http,
            &owner,
            &ProviderKind::Gemini,
            channel,
            "/clear",
            SoftClearNotifyMode::Suppress,
            None,
        )
        .await
        .unwrap();
        assert_eq!(session_state(&owner, channel).await.0, None);
        gate.0.notify_one();
        gate.1.notified().await;
        channel
            .say(
                &http,
                crate::services::discord::commands::SESSION_CLEARED_RESPONSE,
            )
            .await
            .unwrap();
    }));
    tokio::time::timeout(std::time::Duration::from_secs(10), barrier.0.notified())
        .await
        .expect("clear body finished before reply");
    home.close_intake();
    assert_eq!(
        home.commands_in_flight(),
        1,
        "reply remains outstanding after reset completed"
    );
    barrier.1.notify_one();
    work.await.unwrap();
    assert_eq!(home.commands_in_flight(), 0, "reply completed");
    assert!(api.take().iter().any(|call| call.contains(crate::services::discord::commands::SESSION_CLEARED_RESPONSE)), "real reply transport reached");
}

#[cfg(unix)]
async fn b2b1_effect(
    label: &'static str,
    shared: Arc<crate::services::discord::SharedData>,
    channel: ChannelId,
) -> bool {
    let http = Arc::new(
        serenity::HttpBuilder::new("test-token")
            .proxy("http://127.0.0.1:1")
            .ratelimiter_disabled(true)
            .build(),
    );
    let provider = ProviderKind::Gemini;
    match label {
        "clear" => super::super::clear_channel_session_state_fenced(
            &http,
            &shared,
            &provider,
            channel,
            "!clear",
            SoftClearNotifyMode::Suppress,
            None,
        )
        .await
        .is_ok(),
        _ => matches!(
            super::super::reset_channel_provider_state(
                &http, &shared, &provider, channel, "pending", true, false, false
            )
            .await,
            super::super::ManagedReset::Applied(_)
        ),
    }
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn b2b1_clear_and_reset_withdraw_only_once_admitted_and_restore_nothing() {
    use crate::services::discord::turn_presence::entrypoints::tests::Probe;
    let root = tempfile::tempdir().unwrap();
    let _env = crate::config::TestEnvVarGuard::set_path("AGENTDESK_ROOT_DIR", root.path());
    for label in ["clear", "reset"] {
        let shared = crate::services::discord::make_shared_data_for_tests_with_storage(None);
        let probe = Probe::install(&shared);
        let channel = ChannelId::new(CHANNEL + 20);
        seed_session(&shared, channel).await;
        let ticket = probe.arm(channel.get());
        let home = channel_home::register_for_test(channel.get(), Some(HomeState::Worker));
        home.close_intake();
        assert!(!b2b1_effect(label, shared.clone(), channel).await);
        assert!(
            Probe::current(&ticket),
            "{label}: a refusal withdraws nothing"
        );
        channel_home::unregister(&channel.get().to_string());
        let barrier = Arc::new((tokio::sync::Notify::new(), tokio::sync::Notify::new()));
        PAUSE.with(|slot| *slot.borrow_mut() = Some((label, barrier.clone())));
        let work = tokio::spawn(b2b1_effect(label, shared.clone(), channel));
        tokio::time::timeout(std::time::Duration::from_secs(10), barrier.0.notified())
            .await
            .expect("effect boundary reached");
        assert!(
            Probe::current(&ticket),
            "{label}: withdrawn before its refusal check"
        );
        barrier.1.notify_one();
        assert!(work.await.unwrap(), "{label} applied");
        PAUSE.with(|slot| *slot.borrow_mut() = None);
        assert!(
            !Probe::current(&ticket),
            "{label}: the change withdrew the approval"
        );
        assert!(
            Probe::current(&probe.arm(channel.get())),
            "{label}: fresh after"
        );
    }
}

#[cfg(unix)]
#[test]
fn b2b1_a_clear_whose_persist_fails_still_withdraws_and_keeps_its_session() {
    use super::super::clear_persist_failure_tests::{break_queue_persist, queue_file};
    use crate::services::discord::turn_presence::entrypoints::tests::Probe;
    let root = tempfile::tempdir().unwrap();
    let _env = crate::config::TestEnvVarGuard::set_path("AGENTDESK_ROOT_DIR", root.path());
    let provider = ProviderKind::Gemini;
    let channel = ChannelId::new(CHANNEL + 21);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .start_paused(true)
        .build()
        .unwrap();
    runtime.block_on(async {
        let shared = crate::services::discord::make_shared_data_for_tests_with_storage(None);
        let probe = Probe::install(&shared);
        seed_session(&shared, channel).await;
        seed_backlog(&shared, &provider, channel).await;
        break_queue_persist(&queue_file(root.path(), &shared, &provider, channel));
        let ticket = probe.arm(channel.get());
        assert!(!b2b1_effect("clear", shared.clone(), channel).await);
        assert_eq!(queue_len(&shared, channel).await, 1, "backlog restored");
        let session = session_state(&shared, channel).await;
        assert_eq!(session, (Some(SESSION_ID.to_string()), false));
        assert!(
            !Probe::current(&ticket),
            "the failed clear restored no approval"
        );
    });
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn b2b1_queued_cancel_and_drain_reset_keep_the_active_turn_scope() {
    use crate::services::discord::turn_presence::entrypoints::tests::Probe;
    let root = tempfile::tempdir().unwrap();
    let _env = crate::config::TestEnvVarGuard::set_path("AGENTDESK_ROOT_DIR", root.path());
    let shared = crate::services::discord::make_shared_data_for_tests_with_storage(None);
    let probe = Probe::install(&shared);
    let provider = ProviderKind::Gemini;
    let channel = ChannelId::new(CHANNEL + 22);
    seed_session(&shared, channel).await;
    seed_backlog(&shared, &provider, channel).await;
    let ticket = probe.arm(channel.get());
    let removed = super::super::cancel_queued_with_home_permit(
        &shared,
        &provider,
        channel,
        MessageId::new(channel.get() + 1),
    )
    .await
    .unwrap();
    assert!(removed.is_some());
    assert_eq!(queue_len(&shared, channel).await, 0);
    assert!(
        Probe::current(&ticket),
        "a queued cancel is no parent change"
    );
    let http = Arc::new(serenity::Http::new(""));
    let reset = super::super::reset_channel_provider_state_for_home_drain(
        &http,
        &shared,
        &provider,
        channel,
        "home release",
        true,
        false,
        false,
    )
    .await;
    assert!(matches!(reset, super::super::ManagedReset::Applied(_)));
    assert!(
        !Probe::current(&ticket),
        "the drain reset shares the reset body"
    );
}
