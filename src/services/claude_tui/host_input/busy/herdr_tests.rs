//! Direct source, ownership and captured-target contracts on the existing typed Herdr rig.
use super::*;
use crate::db::dispatched_sessions::hosted_execution::{
    HostedExecution, HostedLocation, HostedObservation, HostedOwner, HostedRecord, HostedState,
    SourceRef,
};
use crate::services::discord::observe_owned_claude_busy_for_test as observe_busy;
use crate::services::herdr_admission::{Admission, force_for_test};
use crate::services::herdr_launch::HerdrLaunchEndpoint;
use crate::services::session_host::herdr_socket_rig_tests::{HerdrRig, KEY, NODE, PANE, SESSION};
use crate::services::tui_o::cutover::{self, test_override};
use crate::services::tui_prompt_dedupe::hosted_source::tests::{
    CHANNEL, Fixture as BindingFixture, LOGICAL, N1, N2,
};
use crate::services::turn_host::HerdrTurnPlan;
use binding_events::{BindingCause, CauseSource, HookSignal, Proposal};
use serde_json::json;
use std::io::Write;

fn binding(source: &SourceId) {
    tui_prompt_dedupe::register_tmux_runtime_binding(
        LOGICAL,
        tui_prompt_dedupe::TuiRuntimeBinding {
            runtime_kind: RuntimeHandoffKind::ClaudeTui,
            output_path: source.path.display().to_string(),
            relay_output_path: None,
            input_fifo_path: None,
            session_id: Some(source.session_id.clone()),
            last_offset: 0,
            relay_last_offset: None,
        },
    );
}

fn live_source(fx: &BindingFixture) -> SourceId {
    let source = fx.source(N1, "18114000-0000-4000-8000-000000000001");
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(&source.path)
        .unwrap();
    writeln!(
        file,
        "{}",
        json!({"type":"assistant", "sessionId":source.session_id,
        "message":{"role":"assistant", "content":[{"type":"text","text":"working"}]}})
    )
    .unwrap();
    binding(&source);
    source
}

fn host(rig: &HerdrRig) -> crate::services::turn_host::TurnHost {
    let owner = HostedOwner {
        provider: "claude".into(),
        discord_token_hash: "observation-fixture".into(),
        channel_id: CHANNEL.to_string(),
        logical_key: LOGICAL.into(),
        owner_node: NODE.into(),
        runtime_root: crate::config::runtime_root().unwrap().display().to_string(),
    };
    let record = HostedExecution {
        schema: 1,
        state: HostedState::Bound,
        execution_nonce: N1.into(),
        location: Some(HostedLocation {
            host: "herdr".into(),
            execution_node: NODE.into(),
            endpoint_config_key: KEY.into(),
            socket_addr: rig.socket().display().to_string(),
            named_session: SESSION.into(),
            pane_id: PANE.into(),
        }),
        expected: Some(rig.expected(N1)),
        source_ref: SourceRef {
            runtime_root: owner.runtime_root.clone(),
            channel: owner.channel_id.clone(),
            provider: owner.provider.clone(),
            logical_key: owner.logical_key.clone(),
            execution_nonce: N1.into(),
            initial_source: None,
            baseline_event_seq: None,
        },
        owner,
    };
    crate::services::turn_host::TurnHost::Herdr(Box::new(HerdrTurnPlan {
        endpoint: HerdrLaunchEndpoint {
            execution_node: NODE.into(),
            config_key: KEY.into(),
            socket_addr: rig.socket().display().to_string(),
            herdr_session: SESSION.into(),
        },
        row: Some(HostedObservation::known_fixture(record)),
    }))
}

fn pane(rig: &HerdrRig) {
    rig.run_provider(&rig.context(N1), false);
    let marker = crate::services::tmux_common::session_temp_path(LOGICAL, "host_kind");
    std::fs::create_dir_all(std::path::Path::new(&marker).parent().unwrap()).unwrap();
    std::fs::write(marker, "herdr").unwrap();
    rig.answer(
        "pane.read",
        json!({"type":"pane_read", "read":{
        "pane_id":PANE,"source":"recent_unwrapped","format":"text",
        "text":"Working\n❯ \n", "revision":1,"truncated":false }}),
    );
}

fn current(context: &Context) -> Option<Source> {
    crate::services::tmux_common::with_tmux_source_authority(LOGICAL, |authority| {
        context.current_source(authority)
    })
}

#[test]
fn current_source_requires_the_bound_nonce_and_current_runtime_binding() {
    let fx = BindingFixture::new();
    let source = live_source(&fx);
    let rig = HerdrRig::start();
    let _registry = rig.registry_on_this_thread();
    pane(&rig);
    let observation =
        observe_busy(CHANNEL, LOGICAL, &host(&rig), "observation-fixture");
    let context = observation.context.as_ref().unwrap();
    assert_eq!(current(context).unwrap().id, source);
    let wrong_nonce = Context {
        channel: CHANNEL,
        logical: LOGICAL.into(),
        nonce: N2.into(),
        output_ready: context.output_ready,
        target: context.target.clone(),
        writer: context.writer.clone(),
        attempt_target: context.attempt_target.clone(),
    };
    assert!(
        current(&wrong_nonce).is_none(),
        "another nonce cannot use the global pin"
    );
    tui_prompt_dedupe::clear_tmux_runtime_binding(LOGICAL);
    assert!(tui_prompt_dedupe::runtime_binding_for_tmux_session(LOGICAL).is_none());
    assert!(
        current(context).is_none(),
        "a missing current projection is no source permission"
    );
    assert!(rig.sends().is_empty());
}

#[test]
fn current_source_rejects_a_replaced_transcript_inode() {
    let fx = BindingFixture::new();
    let source = live_source(&fx);
    let rig = HerdrRig::start();
    let _registry = rig.registry_on_this_thread();
    pane(&rig);
    let observation =
        observe_busy(CHANNEL, LOGICAL, &host(&rig), "observation-fixture");
    let context = observation.context.as_ref().unwrap();
    assert_eq!(current(context).unwrap().id, source);
    let replacement = source.path.with_extension("replacement");
    std::fs::copy(&source.path, &replacement).unwrap();
    std::fs::rename(&replacement, &source.path).unwrap();
    assert!(
        current(context).is_none(),
        "same path and content do not prove the pinned inode"
    );
    assert!(rig.sends().is_empty());
}

#[test]
fn an_owned_live_source_waits_while_its_actual_output_writer_is_absent() {
    let fx = BindingFixture::new();
    let source = live_source(&fx);
    let rig = HerdrRig::start();
    let _registry = rig.registry_on_this_thread();
    let _selected = test_override::force_channels(&[(CHANNEL, RuntimeHandoffKind::ClaudeTui)]);
    pane(&rig);
    assert_eq!(
        cutover::peek_o_owns_tui_output_for_channel(CHANNEL, Some(RuntimeHandoffKind::ClaudeTui)),
        Ok(true)
    );
    assert!(!crate::services::tui_o::writer::host::channel_accepts(
        CHANNEL
    ));
    let observation =
        observe_busy(CHANNEL, LOGICAL, &host(&rig), "observation-fixture");
    let context = observation.context.as_ref().unwrap();
    assert!(context.target.execution_alive());
    assert_eq!(current(context).unwrap().id, source);
    assert!(
        crate::services::tui_turn_state::observe_claude_jsonl_turn_state(&source.path).is_busy()
    );
    assert_eq!(observation.state(), BusyState::Held);
    assert!(matches!(observation.probe()(), Screen::Unknown));
    assert!(rig.sends().is_empty());
}

#[test]
fn a_captured_target_rejects_the_other_execution_running_in_its_pane() {
    let fx = BindingFixture::new();
    live_source(&fx);
    let rig = HerdrRig::start();
    let _registry = rig.registry_on_this_thread();
    let stop = fx.root.path().join("no-stop");
    let _admission = force_for_test(Admission::new(true, Some(stop)));
    pane(&rig);
    let observation =
        observe_busy(CHANNEL, LOGICAL, &host(&rig), "observation-fixture");
    let context = observation.context.as_ref().unwrap();
    assert!(context.target.execution_alive());
    context.target.pin(HerdrMutation::Input).unwrap();
    context.target.discard_pin();
    rig.run_provider(&rig.context(N2), false);
    assert!(!context.target.execution_alive());
    assert_eq!(
        context.target.pin(HerdrMutation::Input),
        Err(crate::services::session_host::HerdrGateRefusal::OtherNonce)
    );
    assert!(matches!(observation.probe()(), Screen::Unknown));
    assert!(rig.sends().is_empty());
}

#[test]
fn a_held_source_probe_wakes_verified_clear_before_its_new_transcript_exists() {
    let fx = BindingFixture::new();
    let source = live_source(&fx);
    let rig = HerdrRig::start();
    let _registry = rig.registry_on_this_thread();
    pane(&rig);
    std::fs::remove_file(&source.path).unwrap();
    let observation =
        observe_busy(CHANNEL, LOGICAL, &host(&rig), "observation-fixture");
    assert_eq!(observation.state(), BusyState::Held);
    let mut probe = observation.probe();
    assert!(matches!(probe(), Screen::Unknown));
    let native = "18114000-0000-4000-8000-000000000002";
    let new_path = fx.root.path().join(format!("{native}.jsonl"));
    let now = chrono::Utc::now();
    let hook = HookSignal {
        event: "session_start".into(),
        source: Some("clear".into()),
        transcript_path: Some(new_path.display().to_string()),
        received_at: now,
        published_at: Some(now),
    };
    binding_events::record_pending(&Proposal {
        channel_id: CHANNEL,
        provider: "claude",
        tmux_session: LOGICAL,
        session_id: Some(native),
        path: new_path.to_str().unwrap(),
        replaced: Some((
            source.path.to_str().unwrap(),
            Some(source.session_id.as_str()),
        )),
        cause: CauseSource::Hook(BindingCause::Clear),
        hook: Some(&hook),
    })
    .unwrap();
    assert!(crate::services::claude::herdr_turn::awaited_clear(CHANNEL, LOGICAL, N1).is_some());
    assert!(!new_path.exists());
    assert!(
        matches!(probe(), Screen::Ready),
        "verified clear delegates without granting Busy"
    );
    assert_eq!(
        observe_busy(CHANNEL, LOGICAL, &host(&rig), "observation-fixture").state(),
        BusyState::Idle
    );
    assert!(rig.sends().is_empty());
}

#[test]
fn a_bound_observation_requires_this_accounts_node_and_runtime_owner() {
    let fx = BindingFixture::new();
    live_source(&fx);
    let rig = HerdrRig::start();
    let _registry = rig.registry_on_this_thread();
    pane(&rig);
    for case in ["token", "node", "runtime", "missing-shared"] {
        let mut stale = host(&rig);
        let crate::services::turn_host::TurnHost::Herdr(plan) = &mut stale else {
            unreachable!()
        };
        let HostedRecord::Known(mut record) = plan.row.as_ref().unwrap().record.clone() else {
            unreachable!()
        };
        match case {
            "token" => record.owner.discord_token_hash = "another-account".into(),
            "node" => record.owner.owner_node = "another-node".into(),
            "runtime" => {
                record.owner.runtime_root =
                    fx.root.path().join("another-runtime").display().to_string();
                record.source_ref.runtime_root = record.owner.runtime_root.clone();
            }
            _ => {}
        }
        plan.row = Some(HostedObservation::known_fixture(record));
        let token = if case == "missing-shared" {
            ""
        } else {
            "observation-fixture"
        };
        let observation = observe_busy(CHANNEL, LOGICAL, &stale, token);
        assert_eq!(observation.state(), BusyState::Held, "{case}");
        assert!(
            observation.herdr().is_none(),
            "{case} must not obtain a guessed target"
        );
        assert!(observation.target().is_none(), "{case}");
        assert!(matches!(observation.probe()(), Screen::Unknown));
    }
    assert!(rig.sends().is_empty());
}

#[test]
fn an_idle_relay_does_not_delegate_a_busy_input_source() {
    use crate::services::claude_tui::readiness;
    let fx = BindingFixture::new();
    let source = live_source(&fx);
    let rig = HerdrRig::start();
    let registry = rig.registry_on_this_thread();
    let context: tui_prompt_dedupe::binding_context::BindingContext =
        serde_json::from_value(json!({
            "schema":1,"provider":"claude","created_at":chrono::Utc::now(),
            "execution_nonce":N1,"tmux_session":LOGICAL,"channel_id":CHANNEL,
            "owner_runtime_root":crate::services::tmux_common::current_tmux_owner_marker(),
            "expected_native_session_id":source.session_id,"launch_mode":"resume",
            "provider_root":fx.root.path()
        }))
        .unwrap();
    let prepared =
        tui_prompt_dedupe::binding_context::PreparedIncarnation::create(context).unwrap();
    rig.run_provider(&prepared.path, false);
    let marker = crate::services::tmux_common::session_temp_path(LOGICAL, "host_kind");
    std::fs::write(marker, "herdr").unwrap();
    rig.answer(
        "pane.read",
        json!({"type":"pane_read","read":{
        "pane_id":PANE,"source":"recent_unwrapped","format":"text",
        "text":"Working\n❯ \n","revision":1,"truncated":false}}),
    );
    let relay = fx.root.path().join("idle-relay.jsonl");
    std::fs::File::create(&relay).unwrap();
    let mut projection = tui_prompt_dedupe::runtime_binding_for_tmux_session(LOGICAL).unwrap();
    projection.relay_output_path = Some(relay.display().to_string());
    tui_prompt_dedupe::register_tmux_runtime_binding(LOGICAL, projection);
    assert!(readiness::observe(LOGICAL, Some(&relay)).is_none());
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(&relay)
        .unwrap();
    for row in [
        json!({"type":"user","sessionId":source.session_id,"timestamp":chrono::Utc::now(),
            "message":{"role":"user","content":"the relay's prior input"}}),
        json!({"type":"assistant","sessionId":source.session_id,"timestamp":chrono::Utc::now(),
            "apiBlockIndex":0,"message":{"id":"relay-answer","role":"assistant",
            "content":[{"type":"text","text":"the relay's prior answer"}]}}),
        json!({"type":"system","subtype":"turn_duration","sessionId":source.session_id,
            "timestamp":chrono::Utc::now()}),
    ] {
        writeln!(file, "{row}").unwrap();
    }
    assert!(
        readiness::observe(LOGICAL, None).is_some(),
        "the other relay has a valid public Idle proof"
    );
    assert!(readiness::observe(LOGICAL, Some(&source.path)).is_none());
    assert!(
        crate::services::tui_turn_state::observe_claude_jsonl_turn_state(&source.path).is_busy()
    );
    let observation =
        observe_busy(CHANNEL, LOGICAL, &host(&rig), "observation-fixture");
    assert_eq!(
        observation.state(),
        BusyState::Held,
        "a different relay's Idle proof cannot delegate this busy input path"
    );
    assert_eq!(
        current(observation.context.as_ref().unwrap()).unwrap().id,
        source
    );
    assert!(rig.sends().is_empty());
    drop(registry);
}
