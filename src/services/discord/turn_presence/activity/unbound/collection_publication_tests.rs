use super::*;

#[path = "retired_codex_runtime_marker_tests.rs"]
mod retired_codex_runtime_marker_tests;

#[tokio::test(flavor = "current_thread")]
async fn a2_s2_superseded_collection_preserves_live_presence_pg() {
    use crate::services::discord::turn_presence::supervisor::tests::LiveSupervisor;

    let id = 69_185_301;
    let rig = Rig::new(id, false, false).await;
    let _turn = crate::services::tui_o::turn_mode::TestConfirmation::new(id);
    let no_database = crate::services::discord::make_shared_data_for_tests_with_storage(None);
    let watch = rig.watch();
    let (ordinary, stamp) = read(
        &no_database,
        &ProviderKind::Claude,
        rig.channel,
        watch.clone(),
    )
    .await;
    assert_eq!(
        ordinary,
        observed(Activity::Unknown, "session_probe_failed")
    );
    assert!(stamp.snapshot.is_none());
    let ordinary_callbacks = AtomicUsize::new(0);
    assert!(
        stamp
            .publish(&watch, || {
                ordinary_callbacks.fetch_add(1, Ordering::SeqCst);
            })
            .await
            .is_some(),
        "an ordinary database error must retain its publishable Unknown"
    );
    assert_eq!(ordinary_callbacks.load(Ordering::SeqCst), 1);

    let supervisor = Arc::new(LiveSupervisor::new(
        rig.shared.clone(),
        ProviderKind::Claude,
        id,
    ));
    supervisor.tick().await;
    let before = supervisor.state();
    assert_eq!(before["activity"], "idle");
    assert_eq!(before["reason"], "no_session");
    assert!(before["generation"].as_u64().unwrap() > 0);

    let pause = Arc::new(Pause {
        at: 1,
        count: AtomicUsize::new(0),
        entered: Default::default(),
        resume: Default::default(),
    });
    lock(&PAUSES).insert(id, pause.clone());
    let _pause = PauseGuard(id);
    let ticking = supervisor.clone();
    let task = tokio::spawn(async move { ticking.tick().await });
    tokio::time::timeout(std::time::Duration::from_secs(20), pause.entered.notified())
        .await
        .expect("live supervisor did not reach its first collected files");
    let foreign = crate::services::discord::make_shared_data_for_tests_with_storage(None);
    let bumped = foreign
        .tmux_watchers
        .restore_owner_channel_for_tmux_session(
            &format!("AgentDesk-codex-a2-unrelated-{id}"),
            ChannelId::new(id + 1),
        );
    pause.resume.notify_one();
    task.await.unwrap();
    assert!(
        bumped,
        "an unrelated registry must advance the global owner epoch"
    );
    let after = supervisor.state();
    assert_eq!(
        after, before,
        "a superseded collection must not publish or flicker real Presence"
    );
    supervisor.tick().await;
    let recovered = supervisor.state();
    assert_eq!(recovered["activity"], "idle");
    assert_eq!(
        recovered["generation"], before["generation"],
        "the following stable tick must not repair an Idle-to-Unknown flicker"
    );
    eprintln!(
        "A2_S2_REAL_PRESENCE superseded_updates=0 activity_transitions=0 ordinary_unknown_callbacks=1"
    );
}
