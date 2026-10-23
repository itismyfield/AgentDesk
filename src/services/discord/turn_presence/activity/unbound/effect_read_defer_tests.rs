//! The periodic observer does not supersede an effect read still running on the same channel.
use super::*;
use crate::services::discord::turn_presence::supervisor::tests::LiveSupervisor;

#[tokio::test(flavor = "current_thread")]
async fn observer_tick_defers_to_a_running_effect_read_pg() {
    let rig = Rig::new(69_182_090, false, false).await;
    // The observer reads only channels confirmed in turn mode.
    let _turn = crate::services::tui_o::turn_mode::TestConfirmation::new(rig.channel.get());
    let supervisor =
        LiveSupervisor::new(rig.shared.clone(), ProviderKind::Claude, rig.channel.get());
    let pause = Arc::new(Pause {
        at: 2,
        count: AtomicUsize::new(0),
        entered: Default::default(),
        resume: Default::default(),
    });
    lock(&PAUSES).insert(rig.channel.get(), pause.clone());
    let _pause = PauseGuard(rig.channel.get());
    let (shared, channel) = (rig.shared.clone(), rig.channel);
    let effect = tokio::spawn(async move {
        crate::services::discord::turn_presence::activity::activity_now(
            &shared,
            &ProviderKind::Claude,
            channel,
        )
        .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(10), pause.entered.notified())
        .await
        .unwrap();
    supervisor.tick().await;
    assert!(
        supervisor.state()["observed_at"].is_null(),
        "the observer read while an effect read was running"
    );
    pause.resume.notify_one();
    assert_eq!(
        effect.await.unwrap(),
        observed(Activity::Idle, "no_session"),
        "the effect read was superseded"
    );
    supervisor.tick().await;
    assert_eq!(
        supervisor.state()["reason"],
        "no_session",
        "the observer resumes once the effect read ends"
    );
}
