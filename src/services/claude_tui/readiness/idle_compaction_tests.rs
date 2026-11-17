use super::*;
use crate::services::tui_turn_state::idle_compaction_tests::{end_turn, tail};

#[test]
fn idle_compaction_proves_readiness_without_a_later_stop_hook() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let r = Rig::new();
    r.append(end_turn());
    for record in tail() {
        r.append(record);
    }
    let proof = observe(&r.session, Some(&r.path)).expect("completed reply stays ready");
    let reservation = reserve(proof).expect("same canonical tail can be reserved");
    assert!(reservation.current());
    reservation.not_sent();
}

#[test]
fn idle_compaction_readiness_refuses_activity_after_the_completed_reply() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    for active in [
        json!({"type":"assistant","message":{"stop_reason":null,"content":[{"type":"text","text":"working"}]}}),
        json!({"type":"assistant","message":{"stop_reason":"tool_use","content":[{"type":"tool_use","id":"tool","name":"Read","input":{}}]}}),
        json!({"type":"progress","data":{"type":"hook_progress"}}),
        json!({"type":"system","subtype":"compact_boundary","compactMetadata":{"trigger":"auto"}}),
        json!({"type":"user","isCompactSummary":true,"message":{"content":"summary"}}),
    ] {
        let r = Rig::new();
        r.append(end_turn());
        r.append(active.clone());
        for record in tail() {
            r.append(record);
        }
        assert!(!r.ready(), "{active}");
    }
}
