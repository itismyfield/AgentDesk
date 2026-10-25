use super::*;
use crate::services::discord::router::message_handler::codex_resume_companion_queue_tests::Fixture;
use crate::services::discord::task_supervisor::watcher_spawn_observation::Scope;
use crate::services::discord::tmux::watcher_install_fixture as install;
use crate::services::discord::turn_presence::activity::Activity;
use crate::services::tui_prompt_dedupe as dedupe;

#[test]
fn delayed_codex_rehydration_restores_one_idle_watcher_pg() {
    if !install::isolated(
        module_path!(),
        "delayed_codex_rehydration_restores_one_idle_watcher_pg",
    ) {
        return;
    }
    tokio::runtime::Builder::new_current_thread()
        .on_thread_start(|| dedupe::binding_events::set_test_root(Some(&install::root())))
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let fixture = Fixture::new(7_359_001, true).await;
            std::fs::remove_file(&fixture.install.output).unwrap();
            fixture.cold_memory();
            let submitted = Scope::new(fixture.shared());

            crate::services::discord::tmux::restore_tmux_watchers(
                &fixture.install.http,
                fixture.shared(),
            )
            .await;
            assert_eq!(submitted.submissions(), 0, "boot precedes discovery");
            fixture
                .assert_activity(Activity::Unknown, "watcher_unbound")
                .await;

            let mut next_rehydrate = tokio::time::Instant::now();
            codex_source_phase(fixture.shared(), &mut next_rehydrate).await;
            assert!(
                dedupe::runtime_binding_for_tmux_session(&fixture.install.session).is_some(),
                "the real source phase must finish rehydration"
            );
            assert_eq!(
                submitted.submissions(),
                1,
                "completed discovery must retry the skipped boot watcher exactly once"
            );
            fixture.assert_activity(Activity::Idle, "closed").await;
            let first = fixture.install.handle();
            for _ in 0..2 {
                next_rehydrate = tokio::time::Instant::now();
                codex_source_phase(fixture.shared(), &mut next_rehydrate).await;
            }
            assert_eq!(
                submitted.submissions(),
                1,
                "later ticks must reuse the watcher"
            );
            assert!(Arc::ptr_eq(&first.cancel, &fixture.install.handle().cancel));
            assert_eq!(submitted.cancel_and_join_all().await.len(), 1);
        });
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .on_thread_start(|| dedupe::binding_events::set_test_root(Some(&install::root())))
        .enable_all()
        .build()
        .unwrap()
}

async fn tick(fixture: &Fixture) {
    let mut next = tokio::time::Instant::now();
    codex_source_phase(fixture.shared(), &mut next).await;
}

async fn cold(channel: u64) -> Fixture {
    let fixture = Fixture::new(channel, true).await;
    std::fs::remove_file(&fixture.install.output).unwrap();
    fixture.cold_memory();
    fixture
}

async fn boot(fixture: &Fixture) {
    crate::services::discord::tmux::restore_tmux_watchers(&fixture.install.http, fixture.shared())
        .await;
}

use crate::services::discord::tmux::restore_retry;

#[test]
fn codex_rehydration_retry_is_bounded_after_failed_attach_pg() {
    if !install::isolated(
        module_path!(),
        "codex_rehydration_retry_is_bounded_after_failed_attach_pg",
    ) {
        return;
    }
    runtime().block_on(async {
        let fixture = cold(7_359_002).await;
        let submitted = Scope::new(fixture.shared());
        boot(&fixture).await;
        let saved = Arc::new(Mutex::new(None));
        let capture = saved.clone();
        let marker = fixture.marker();
        restore_retry::before_retry(fixture.shared(), move || {
            *capture.lock().unwrap() = Some(std::fs::read(&marker).unwrap());
            std::fs::remove_file(marker).unwrap();
        });
        tick(&fixture).await;
        assert_eq!(restore_retry::attempts(fixture.shared()), 1);
        assert_eq!(
            submitted.submissions(),
            0,
            "lost proof must leave the watcher unbound"
        );
        fixture
            .assert_activity(Activity::Unknown, "watcher_unbound")
            .await;
        std::fs::write(fixture.marker(), saved.lock().unwrap().take().unwrap()).unwrap();
        for _ in 0..2 {
            tick(&fixture).await;
        }
        assert_eq!(
            restore_retry::attempts(fixture.shared()),
            1,
            "a failed retry consumes its budget"
        );
        assert_eq!(submitted.submissions(), 0);
        fixture
            .assert_activity(Activity::Unknown, "watcher_unbound")
            .await;
        assert!(submitted.cancel_and_join_all().await.is_empty());
    });
}

#[test]
fn codex_rehydration_existing_watcher_is_not_retried_pg() {
    if !install::isolated(
        module_path!(),
        "codex_rehydration_existing_watcher_is_not_retried_pg",
    ) {
        return;
    }
    runtime().block_on(async {
        let fixture = cold(7_359_003).await;
        let submitted = Scope::new(fixture.shared());
        tick(&fixture).await;
        assert_eq!(
            submitted.submissions(),
            0,
            "discovery before boot cannot restore"
        );
        boot(&fixture).await;
        assert_eq!(submitted.submissions(), 1);
        let first = fixture.install.handle();
        for _ in 0..2 {
            tick(&fixture).await;
        }
        assert_eq!(
            restore_retry::attempts(fixture.shared()),
            0,
            "an attached channel gets no retry"
        );
        assert_eq!(submitted.submissions(), 1);
        assert!(Arc::ptr_eq(&first.cancel, &fixture.install.handle().cancel));
        fixture.assert_activity(Activity::Idle, "closed").await;
        assert_eq!(submitted.cancel_and_join_all().await.len(), 1);
    });
}

#[test]
fn codex_rehydration_waits_for_boot_restore_completion_pg() {
    if !install::isolated(
        module_path!(),
        "codex_rehydration_waits_for_boot_restore_completion_pg",
    ) {
        return;
    }
    runtime().block_on(async {
        let fixture = cold(7_359_004).await;
        let submitted = Scope::new(fixture.shared());
        restore_retry::begin_for_test(fixture.shared(), &fixture.install.http);
        tick(&fixture).await;
        assert!(dedupe::runtime_binding_for_tmux_session(&fixture.install.session).is_some());
        assert_eq!(
            restore_retry::attempts(fixture.shared()),
            0,
            "an unfinished boot cannot retry"
        );
        assert_eq!(submitted.submissions(), 0);
        boot(&fixture).await;
        tick(&fixture).await;
        assert_eq!(restore_retry::attempts(fixture.shared()), 0);
        assert_eq!(submitted.submissions(), 1);
        assert_eq!(submitted.cancel_and_join_all().await.len(), 1);
    });
}

#[test]
fn codex_rehydration_standby_token_does_not_arm_restore_pg() {
    if !install::isolated(
        module_path!(),
        "codex_rehydration_standby_token_does_not_arm_restore_pg",
    ) {
        return;
    }
    runtime().block_on(async {
        let fixture = cold(7_359_005).await;
        fixture
            .shared()
            .http
            .cached_bot_token
            .set("standby-fixture".into())
            .unwrap();
        let submitted = Scope::new(fixture.shared());
        for _ in 0..2 {
            tick(&fixture).await;
        }
        assert!(dedupe::runtime_binding_for_tmux_session(&fixture.install.session).is_some());
        assert_eq!(restore_retry::attempts(fixture.shared()), 0);
        assert_eq!(
            submitted.submissions(),
            0,
            "a standby token is not boot restore authority"
        );
        fixture
            .assert_activity(Activity::Unknown, "watcher_unbound")
            .await;
        assert!(submitted.cancel_and_join_all().await.is_empty());
    });
}

#[test]
fn codex_rehydration_retry_never_cleans_a_now_dead_pane_pg() {
    if !install::isolated(
        module_path!(),
        "codex_rehydration_retry_never_cleans_a_now_dead_pane_pg",
    ) {
        return;
    }
    runtime().block_on(async {
        let fixture = cold(7_359_006).await;
        let submitted = Scope::new(fixture.shared());
        let queue = fixture.queue_identity().await;
        boot(&fixture).await;
        restore_retry::before_retry(fixture.shared(), || install::pane("dead"));
        tick(&fixture).await;
        assert_eq!(restore_retry::attempts(fixture.shared()), 1);
        assert_eq!(submitted.submissions(), 0);
        assert!(
            !install::root().join("tmux-kills").exists(),
            "retry has no dead-session kill authority"
        );
        assert_eq!(fixture.queue_identity().await, queue);
        assert!(submitted.cancel_and_join_all().await.is_empty());
    });
}
