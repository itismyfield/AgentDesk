//! How a read start waits: Ready goes on, an unrecoverable execution is settled after claim, and an
//! unrecovered or unclear read is watched by the same observation.
use super::*;

#[tokio::test]
async fn a_read_start_goes_on_is_refused_or_is_watched_by_its_own_observation() {
    let shared = crate::services::discord::make_shared_data_for_tests();
    let channel = ChannelId::new(8_422_101);
    let context = serde_json::json!({"origin":"headless","source":"wake-watch"});
    let ready = wait(Observed::Native(Screen::Ready), &shared, channel, &context);
    assert!(matches!(ready, super::super::Wait::Go));
    let refused = wait(
        Observed::Reject("owner differs".into()),
        &shared,
        channel,
        &context,
    );
    assert!(matches!(refused, super::super::Wait::Reject(reason) if reason == "owner differs"));
    for observed in [
        Observed::Unrecovered("session transition busy".into()),
        Observed::Native(Screen::Unknown),
        Observed::Native(Screen::Blocked),
    ] {
        let watched = wait(observed, &shared, channel, &context);
        assert!(matches!(watched, super::super::Wait::Watch(_)));
    }
}

#[test]
fn an_unrecovered_read_watches_as_unknown_and_a_refusal_wakes_its_judgement() {
    assert_eq!(
        Observed::Unrecovered("inflight unread".into()).native(),
        Some(Screen::Unknown)
    );
    assert_eq!(Observed::Reject("owner differs".into()).native(), None);
    assert_eq!(
        Observed::Native(Screen::Ready).native(),
        Some(Screen::Ready)
    );
}

// A closed rollout's Idle (or Busy) on a Bound row counts only while that execution runs; the
// mutant that recovers on Unknown alone keeps a dead execution's Ready.
#[test]
fn a_rollout_read_of_a_bound_row_counts_only_while_its_execution_runs() {
    for screen in [Screen::Ready, Screen::Blocked] {
        assert_eq!(
            live_rollout(screen, true, false),
            Screen::Unknown,
            "{screen:?}"
        );
        assert_eq!(live_rollout(screen, true, true), screen, "{screen:?}");
        assert_eq!(live_rollout(screen, false, false), screen, "{screen:?}");
    }
    assert_eq!(live_rollout(Screen::Unknown, true, true), Screen::Unknown);
}

#[tokio::test]
async fn a_bound_execution_reads_alive_only_while_its_provider_runs_in_its_pane() {
    use crate::db::dispatched_sessions::hosted_execution::{
        HostedExecution, HostedLocation, HostedObservation, HostedOwner, HostedState, SourceRef,
    };
    use crate::services::herdr_launch::HerdrLaunchEndpoint;
    use crate::services::session_host::herdr_socket_rig_tests::{
        HerdrRig, KEY, NODE, PANE, PROVIDER, SESSION, SHELL,
    };
    use crate::services::tui_prompt_dedupe::binding_context::{
        BindingContext, PreparedIncarnation,
    };
    use crate::services::turn_host::HerdrTurnPlan;
    let _root = crate::config::TestRuntimeRootGuard::new();
    let channel = ChannelId::new(8_422_102);
    let logical = "AgentDesk-codex-closed-rollout";
    let nonce = "0123456789abcdef0123456789abcdef";
    let rig = HerdrRig::start();
    let _registry = rig.registry_on_this_thread();
    let mut context: BindingContext =
        serde_json::from_slice(&std::fs::read(rig.context(nonce)).unwrap()).unwrap();
    context.provider = "codex".into();
    let context = PreparedIncarnation::create(context).unwrap();
    rig.run_provider(&context.path, false);
    rig.show_panes(&[PANE]);
    rig.foreground(&[PROVIDER]);
    let marker = crate::services::tmux_common::session_temp_path(logical, "host_kind");
    std::fs::create_dir_all(std::path::Path::new(&marker).parent().unwrap()).unwrap();
    std::fs::write(marker, "herdr").unwrap();
    let owner = HostedOwner {
        provider: "codex".into(),
        discord_token_hash: "fixture".into(),
        channel_id: channel.get().to_string(),
        logical_key: logical.into(),
        owner_node: NODE.into(),
        runtime_root: crate::config::runtime_root().unwrap().display().to_string(),
    };
    let mut expected = rig.expected(nonce);
    expected.binding_provider.clone_from(&owner.provider);
    let record = HostedExecution {
        schema: 1,
        state: HostedState::Bound,
        execution_nonce: nonce.into(),
        location: Some(HostedLocation {
            host: "herdr".into(),
            execution_node: NODE.into(),
            endpoint_config_key: KEY.into(),
            socket_addr: rig.socket().display().to_string(),
            named_session: SESSION.into(),
            pane_id: PANE.into(),
        }),
        expected: Some(expected),
        source_ref: SourceRef {
            runtime_root: owner.runtime_root.clone(),
            channel: owner.channel_id.clone(),
            provider: owner.provider.clone(),
            logical_key: owner.logical_key.clone(),
            execution_nonce: nonce.into(),
            initial_source: None,
            baseline_event_seq: None,
        },
        owner,
    };
    let plan = HerdrTurnPlan {
        endpoint: HerdrLaunchEndpoint {
            execution_node: NODE.into(),
            config_key: KEY.into(),
            socket_addr: rig.socket().display().to_string(),
            herdr_session: SESSION.into(),
        },
        row: Some(HostedObservation::known_fixture(record)),
        bootstrap: false,
    };
    let host = TurnHost::Herdr(Box::new(plan));
    assert!(
        execution_alive(&host).await,
        "a running provider keeps the fast path"
    );
    rig.show_panes(&[]);
    assert!(!execution_alive(&host).await, "a missing pane is not alive");
    rig.show_panes(&[PANE]);
    rig.foreground(&[SHELL]);
    assert!(
        !execution_alive(&host).await,
        "a lone root shell is not alive"
    );
    assert!(
        !execution_alive(&TurnHost::Tmux).await,
        "no Herdr execution, nothing alive"
    );
}
