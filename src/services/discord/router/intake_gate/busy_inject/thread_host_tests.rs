//! The owned busy view follows the thread's next host, not the thread's own boot entry.
use super::*;
use crate::services::session_host::surface_policy::{Relation, force_relation_for_test};
use crate::services::session_host::thread_host;

#[test]
fn a_thread_judged_onto_herdr_shows_no_deferred_tmux_busy_view() {
    let (parent, child, tmux_child) = (8_118_200, 8_118_201, 8_118_202);
    let _hosts = crate::config::session_hosts::force_for_test(Some("n"), &[(parent, "n")]);
    let relation = Relation::Child { parent };
    let _child = force_relation_for_test(child, relation);
    let _tmux_child = force_relation_for_test(tmux_child, relation);
    let view = |channel: u64| {
        defer_owned_busy_view(true, &ProviderKind::Claude, ChannelId::new(channel), None)
    };
    thread_host::record_judged(child, Some(true));
    thread_host::record_judged(tmux_child, Some(false));
    if cfg!(windows) {
        let _gate = test_support::open_gate(tmux_child);
        assert!(
            !view(tmux_child),
            "Windows keeps the gate closed without flock"
        );
    }
    let _gates = [child, tmux_child, parent].map(test_support::open_gate_with_flock);
    assert!(!view(parent), "the Herdr channel itself");
    assert!(!view(child), "a thread whose execution runs on Herdr");
    assert!(view(tmux_child), "a live tmux thread under the same parent");
    // Nothing judged keeps the tmux view the thread has today.
    thread_host::record_judged(child, None);
    assert!(view(child));
    thread_host::record_judged(tmux_child, None);
}
