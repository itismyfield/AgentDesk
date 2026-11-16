//! A recorded thread child hosted through the production boot-ownership entry.

use super::*;

const PARENT: u64 = 8_118_007;

/// Rewrites CHANNEL's store as a child of PARENT recorded it, under a boot selecting only PARENT.
fn recorded_child(harness: &Harness) -> test_override::ChannelsGuard {
    let path = init_path(harness, CHANNEL);
    let mut init: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    init["policy_parent"] = PARENT.into();
    std::fs::write(&path, serde_json::to_vec(&init).unwrap()).unwrap();
    let selected = test_override::force_channels(&[(PARENT, ClaudeTui)]);
    let init = serde_json::from_value(init).unwrap();
    test_override::with_channels(|boot| boot.unwrap().record_child_fixture(&init));
    selected
}

#[tokio::test(start_paused = true)]
async fn a_recorded_thread_child_gets_its_own_ready_writer_and_posts_its_output_once() {
    let (harness, path, source) = switched_over(&row("m0", "before the switch"));
    let startup = p5_event(CHANNEL, "claude", p5::BindingTarget::Source(source));
    p5_log(harness._runtime.path(), CHANNEL, &startup);
    let _boot = recorded_child(&harness);
    harness.gate.acquired();
    let (io, ready) = (TestIo::over(&harness), Arc::new(Readiness::default()));
    let hosts = hosted(&harness, &io, true, &ready);
    append(&path, &row("m1", "child answer"));
    polls(3).await;
    assert!(ready.is_ready(CHANNEL), "the child's own writer is ready");
    assert!(
        io.calls().contains(&("bindings", CHANNEL)),
        "the child reads its own source bindings"
    );
    assert_eq!(harness.port.posts(), ["child answer"]);
    // A second start finds the child's claim taken: one writer, so one post.
    let again = hosted(&harness, &io, true, &ready);
    polls(3).await;
    assert_eq!(harness.port.posts(), ["child answer"]);
    abort(hosts);
    abort(again);
}
