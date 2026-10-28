use super::*;
use std::cell::Cell;

fn configured(endpoint: &HerdrEndpoint) -> crate::config::session_hosts::ChannelEndpoint {
    crate::config::session_hosts::ChannelEndpoint {
        key: endpoint.config_key().into(),
        execution_node: endpoint.execution_node().into(),
        socket_path: endpoint.socket_path().into(),
        herdr_home: "/unused/test-home".into(),
        herdr_session: endpoint.herdr_session().into(),
    }
}

#[test]
fn notice_fence_closing_tombstone_survives_target_drop_and_a_new_nonce_is_independent() {
    let rig = rig();
    let endpoint = endpoint(&rig.server);
    let configured = configured(&endpoint);
    let stored = stored(&rig.server, HostedState::Bound);
    let nonce = stored.execution_nonce.clone();
    let target = HerdrTarget::new(
        endpoint.clone(),
        transport(&rig.server, Vec::new()),
        &stored,
    )
    .unwrap();
    let called = Cell::new(0);
    assert!(try_notice_fence(&configured, &nonce, || called.set(called.get() + 1)).is_ok());
    assert_eq!(called.get(), 1);
    let fence = target.termination_fence().unwrap();
    assert_eq!(
        try_notice_fence(&configured, &nonce, || called.set(called.get() + 1)),
        Err(HerdrGateRefusal::ExecutionClosing)
    );
    drop(fence);
    drop(target);
    assert_eq!(
        try_notice_fence(&configured, &nonce, || called.set(called.get() + 1)),
        Err(HerdrGateRefusal::ExecutionClosing)
    );
    assert_eq!(called.get(), 1);
    let mut successor = stored.clone();
    successor.execution_nonce = format!("{nonce}-next");
    successor.expected.as_mut().unwrap().binding_nonce = successor.execution_nonce.clone();
    let _successor =
        HerdrTarget::new(endpoint, transport(&rig.server, Vec::new()), &successor).unwrap();
    assert!(
        try_notice_fence(&configured, &successor.execution_nonce, || called
            .set(called.get() + 1))
        .is_ok()
    );
    assert_eq!(called.get(), 2);
    assert!(
        rig.server.mutations().is_empty(),
        "notice reads grant no pane mutation authority"
    );
}

#[test]
fn notice_fence_missing_duplicate_active_and_nested_reads_never_start() {
    let rig = rig();
    let endpoint = endpoint(&rig.server);
    let configured = configured(&endpoint);
    let mut stored = stored(&rig.server, HostedState::Bound);
    let nonce = stored.execution_nonce.clone();
    let called = Cell::new(0);
    assert!(try_notice_fence(&configured, &nonce, || called.set(called.get() + 1)).is_err());
    let target = HerdrTarget::new(
        endpoint.clone(),
        transport(&rig.server, Vec::new()),
        &stored,
    )
    .unwrap();
    let permit = target.0.fence.acquire(Mutation::Input).unwrap();
    assert_eq!(
        try_notice_fence(&configured, &nonce, || called.set(called.get() + 1)),
        Err(HerdrGateRefusal::MutationBusy)
    );
    drop(permit);
    assert!(
        try_notice_fence(&configured, &nonce, || {
            assert_eq!(
                try_notice_fence(&configured, &nonce, || ()),
                Err(HerdrGateRefusal::MutationBusy)
            );
            called.set(called.get() + 1);
        })
        .is_ok()
    );
    assert_eq!(called.get(), 1);
    stored.location.as_mut().unwrap().pane_id.push_str("-other");
    let _duplicate =
        HerdrTarget::new(endpoint, transport(&rig.server, Vec::new()), &stored).unwrap();
    assert!(try_notice_fence(&configured, &nonce, || called.set(called.get() + 1)).is_err());
    assert_eq!(called.get(), 1);
    assert!(rig.server.mutations().is_empty());
}
