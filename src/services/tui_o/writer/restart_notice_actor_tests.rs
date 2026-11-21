//! A recorded restart notice through the running actor: posted once, not again after a restart.
use super::*;
use crate::services::tui_o::store::restart_notice::{self, Intent};
use crate::services::tui_o::writer::restart_notice::TEXT;

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
