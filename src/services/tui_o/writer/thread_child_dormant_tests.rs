//! A dormant parent's thread child reads dormant too, through the real host and installed boot.

use super::*;

const PARENT: u64 = 8_118_017;

#[tokio::test(start_paused = true)]
async fn readers_s1b_host_dormant_parent_keeps_its_thread_child_dormant() {
    if !child_case(concat!(
        module_path!(),
        "::readers_s1b_host_dormant_parent_keeps_its_thread_child_dormant"
    )) {
        return;
    }
    let (child, old, latest) = fixture(CHANNEL, "child");
    let events = [
        logged_on(CHANNEL, 1, None, &old),
        logged_on(CHANNEL, 2, Some(&old), &latest),
    ];
    p5_log(child._runtime.path(), CHANNEL, &events.concat());
    // CHANNEL's store is the thread child PARENT recorded; only PARENT is selected, and dormant.
    let path = init_path(&child, CHANNEL);
    let mut init: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    init["policy_parent"] = PARENT.into();
    std::fs::write(&path, serde_json::to_vec(&init).unwrap()).unwrap();
    let config = serde_json::from_value(serde_json::json!({
        "server": {}, "tui_o": {"writer": {"channels": [PARENT], "dormant_channels": [PARENT]}},
        "agents": [{"id": "parent", "name": "Parent", "channels": {
            "claude": {"id": PARENT.to_string(), "runtime": "tui"}
        }}]
    }))
    .unwrap();
    channel_policy::install(&config).unwrap();
    crate::config_live_reload::install(config);
    let boot = channel_policy::boot().unwrap();
    boot.record_child_fixture(&serde_json::from_value(init).unwrap());
    assert_eq!(boot.policy_channel(CHANNEL), PARENT);
    assert!(!boot.dormant_channels().contains(&CHANNEL));
    let (io, ready) = (TestIo::over(&child), Arc::new(Readiness::default()));
    child.gate.acquired();
    let tasks = hosted(&child, &io, true, &ready);
    for _ in 0..180 {
        if ready.accepts(CHANNEL) && inode_fds(&old) == 0 {
            break;
        }
        polls(1).await;
        tokio::task::yield_now().await;
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(ready.accepts(CHANNEL), "the child's own writer resumed");
    assert_eq!(inode_fds(&old), 0, "the child reads dormant, as its parent");
    assert!(inode_fds(&latest) > 0);
    let store = child.channel();
    let witness = store.cleanup_witness(&old).unwrap();
    assert!(witness.is_some_and(|witness| witness.bound_to(store.cursor(&old).unwrap())));
    abort(tasks);
}
