use super::*;
use crate::db::dispatched_sessions::hosted_execution::HostedLookup;
use crate::services::discord::commands::control::{
    ClearReport, clear_channel_session_state_with_session_key,
};

fn clear(case: &Case) -> ClearReport {
    let endpoint = crate::config::session_hosts::herdr_endpoint(CHANNEL);
    let _registry = endpoint.map(|endpoint| {
        case.fx.rig.registry_as_on_this_thread(
            &endpoint.execution_node,
            &endpoint.key,
            &endpoint.socket_path,
            &endpoint.herdr_session,
        )
    });
    let key = super::super::super::session_key(&case.fx.owner);
    case.fx
        .rt
        .block_on(clear_channel_session_state_with_session_key(
            &Arc::new(poise::serenity_prelude::Http::new("")),
            &case.shared,
            &ProviderKind::Claude,
            ChannelId::new(CHANNEL),
            "!clear",
            Some(&key),
        ))
        .expect("the public clear completes")
}

fn legacy_end(missing: bool) {
    let case = Case::launched(if missing {
        "rr-legacy-missing"
    } else {
        "rr-legacy-shell"
    });
    let _intake = IntakeGate::configure(&case);
    let mut live = (*crate::config_live_reload::current().unwrap()).clone();
    live.runtime.herdr_terminate_enabled = Some(true);
    crate::config_live_reload::install(live);
    if missing {
        case.fx.rig.show_panes(&[]);
    } else {
        case.fx.rig.provider_seen(&context_of(&case.nonce), false);
        case.fx.rig.exists(SHELL, true);
        case.fx.rig.exists(PROVIDER, false);
        case.fx.rig.foreground(&[SHELL]);
    }
    let effects = case.effects();
    let row = crate::db::dispatched_sessions::hosted_execution::HostedLookupKey::SessionKey;
    let key = super::super::super::session_key(&case.fx.owner);
    let original = case.fx.rt.block_on(
        crate::db::dispatched_sessions::hosted_execution::load_hosted_execution_pg(
            &case.fx.pool,
            row(&key),
        ),
    );
    let HostedLookup::Found(original) = original else {
        panic!("the original Bound row exists");
    };
    let endpoint = crate::config::session_hosts::herdr_endpoint(CHANNEL).unwrap();
    let _registry = case.fx.rig.registry_as_on_this_thread(
        &endpoint.execution_node,
        &endpoint.key,
        &endpoint.socket_path,
        &endpoint.herdr_session,
    );
    let probe =
        crate::services::discord::recovery_engine::herdr_reader::prepare_root_replaced_candidate(
            &original,
        )
        .unwrap();
    assert!(
        !matches!(probe.observe(), Ok(Some(_))),
        "the fixture cannot authorize RootReplaced recovery"
    );
    let report = clear(&case);
    assert_eq!(
        case.fx.row(),
        Some(HostedState::Retired),
        "a noncandidate clear preserves the legacy confirmed-ended retirement"
    );
    assert_eq!(
        report.reply(),
        format!(
            "{}\n대기 메시지 0건을 비웠어요.",
            crate::services::discord::commands::SESSION_CLEARED_RESPONSE
        )
    );
    assert_eq!(
        case.cleanups(),
        0,
        "legacy end does not invoke recovery cleanup"
    );
    assert!(recovery::CAS_ATTEMPTS.with(|count| count.get() == 0));
    assert_eq!(
        case.effects(),
        effects,
        "an ended execution needs no pane write"
    );
}

#[test]
fn clear_same_root_lone_shell_keeps_legacy_retirement_pg() {
    legacy_end(false);
}

#[test]
fn clear_missing_pane_keeps_legacy_retirement_pg() {
    legacy_end(true);
}

#[test]
fn clear_retired_without_endpoint_keeps_legacy_managed_end_pg() {
    use crate::services::provider_teardown::tests::test_support::FakeTmux;
    use crate::services::session_backend::{SessionHandle, insert_process_session};

    let case = Case::launched("rr-legacy-offline");
    let _intake = IntakeGate::configure(&case);
    let HostedRecord::Known(mut retired) = case.fx.record() else {
        panic!("the fixture has a Bound row");
    };
    retired.state = HostedState::Retired;
    case.fx.store(&retired);
    let _hosts = crate::config::session_hosts::force_for_test(Some(NODE), &[]);
    assert!(crate::config::session_hosts::herdr_endpoint(CHANNEL).is_none());
    let name = case.fx.logical().to_owned();
    let tmux = FakeTmux::install(&name);
    tmux.become_missing_after_kill();
    let alive = Arc::new(std::sync::atomic::AtomicBool::new(true));
    insert_process_session(
        name.clone(),
        SessionHandle::TestProcess {
            pid: 7_186_933,
            alive: alive.clone(),
        },
    );
    let effects = case.effects();
    let report = clear(&case);
    assert!(
        !alive.load(Ordering::SeqCst),
        "without an endpoint, the clear ends the legacy managed process"
    );
    assert!(!crate::services::platform::tmux::has_session(&name));
    assert_eq!(
        report.reply(),
        format!(
            "{}\n대기 메시지 0건을 비웠어요.",
            crate::services::discord::commands::SESSION_CLEARED_RESPONSE
        )
    );
    assert_eq!(case.fx.record(), HostedRecord::Known(retired));
    assert_eq!(case.effects(), effects);
    assert_eq!(case.cleanups(), 0);
}
