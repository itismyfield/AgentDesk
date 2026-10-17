use super::*;
use crate::services::discord::task_supervisor::watcher_completion::Outcome;
use crate::services::discord::task_supervisor::watcher_spawn_observation::Scope;
use crate::services::discord::tmux::watcher_install_fixture as support;

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

#[test]
fn legacy_restore_preserves_backfill_and_session_adoption() {
    if !support::isolated(
        module_path!(),
        "legacy_restore_preserves_backfill_and_session_adoption",
    ) {
        return;
    }
    runtime().block_on(async {
        let _absence = crate::services::discord::health::lock_watcher_absence_for_test().await;
        let (fixture, _discord) = support::fixture(ProviderKind::Claude, 632_520_106).await;
        let row = crate::services::discord::inflight::InflightTurnState::new(
            fixture.provider.clone(),
            fixture.channel.get(),
            Some("legacy-restore".into()),
            7,
            101,
            102,
            "legacy prompt".into(),
            None,
            Some(fixture.session.clone()),
            Some(fixture.output.clone()),
            None,
            0,
        );
        let path = crate::services::discord::health::test_support::seed_backfill_row(&row);
        let before = std::fs::read(&path).unwrap();
        let submitted = Scope::new(&fixture.shared);
        let observed = support::observe(&fixture);
        crate::services::discord::tmux::restore_tmux_watchers(&fixture.http, &fixture.shared).await;
        assert_eq!(
            submitted.submissions(),
            1,
            "Legacy restore must still submit"
        );
        assert!(
            fixture
                .shared
                .core
                .lock()
                .await
                .sessions
                .contains_key(&fixture.channel)
        );
        support::until(&observed, "Legacy watcher initialization", |s| {
            s.watcher_initialization_observed
        })
        .await;
        let joined = submitted.cancel_and_join_all().await;
        assert_eq!(joined.len(), 1);
        assert!(
            joined
                .iter()
                .all(|(_, result)| *result == Outcome::Returned)
        );
        let effects = observed.snapshot();
        assert!(effects.writable_inflight_load_calls > 0, "{effects:?}");
        assert!(effects.compatibility_backfill_attempts > 0, "{effects:?}");
        assert_ne!(
            std::fs::read(path).unwrap(),
            before,
            "Legacy compatibility backfill remains active"
        );
    });
}
