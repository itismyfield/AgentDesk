//! A recorded restart notice through the running actor: posted once, not again after a restart.
use super::*;
use crate::services::tui_o::store::restart_notice::{self, Intent};
use crate::services::tui_o::writer::restart_notice::TEXT;

fn spawn_notice_actor(harness: &Harness, source: SourceId) -> (watch::Sender<bool>, Actor) {
    let bindings = Arc::new(FakeBindings::new());
    let mut startup = bound(
        1,
        None,
        BindingTarget::Source(source),
        BindingCause::Startup,
        None,
    );
    startup.channel_id = harness.channel_id;
    bindings.commit(startup);
    spawn_with(harness.writer(), ShadowProvider::Claude, bindings)
}

#[tokio::test(start_paused = true)]
async fn the_actor_posts_a_recorded_restart_notice_once_across_a_restart() {
    let (harness, _, _) = switched_over(&row("m0", "before"));
    harness.gate.acquired();
    let root = harness._runtime.path().to_path_buf();
    let intent = Intent::exact(CHANNEL, ShadowProvider::Claude, Some("cut-turn"), 4).unwrap();
    for boot in 0..2 {
        restart_notice::record(&root, &intent).unwrap();
        restart_notice::record(&root, &intent).unwrap();
        let (stop, task) = spawn(harness.writer());
        polls(3).await;
        halt(stop, task).await;
        assert_eq!(harness.port.posts(), [TEXT], "boot {boot}");
        assert!(restart_notice::pending(&root, CHANNEL).is_empty());
    }
}

#[tokio::test(start_paused = true)]
async fn a_busy_restart_intent_prevents_the_idle_shortcut_until_settled() {
    let channel = 838507;
    let (harness, _, source) = switched_over_on(channel, &row("m0", "before"));
    harness.gate.acquired();
    harness.lease.busy.store(true, Ordering::SeqCst);
    let root = harness._runtime.path();
    let intent = Intent::exact(channel, ShadowProvider::Claude, Some("cut-turn"), 4).unwrap();
    restart_notice::record(root, &intent).unwrap();
    let (stop, task) = spawn_notice_actor(&harness, source);
    polls(6).await;
    assert!(!task.is_finished());
    assert_eq!(restart_notice::pending(root, channel), [intent]);
    assert!(harness.port.posts().is_empty());

    harness.lease.busy.store(false, Ordering::SeqCst);
    tokio::time::sleep(std::time::Duration::from_millis(1_020)).await;
    assert_eq!(harness.port.posts(), [TEXT]);
    assert!(restart_notice::pending(root, channel).is_empty());
    polls(4).await;
    assert_eq!(harness.port.posts(), [TEXT]);
    halt(stop, task).await;
}

#[tokio::test(start_paused = true)]
async fn recording_a_restart_intent_wakes_an_already_idle_actor() {
    let channel = 838508;
    let (harness, _, source) = switched_over_on(channel, &row("m0", "before"));
    harness.gate.acquired();
    let root = harness._runtime.path();
    let (stop, task) = spawn_notice_actor(&harness, source);
    polls(6).await;
    assert!(!task.is_finished());
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    let intent = Intent::exact(channel, ShadowProvider::Claude, Some("cut-turn"), 4).unwrap();
    restart_notice::record(root, &intent).unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert_eq!(harness.port.posts(), [TEXT]);
    assert!(restart_notice::pending(root, channel).is_empty());
    polls(4).await;
    assert_eq!(harness.port.posts(), [TEXT]);
    halt(stop, task).await;
}
