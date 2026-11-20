use super::*;
use crate::db::dispatched_sessions::hosted_execution::HostedExecution;
use crate::services::agent_protocol::StreamMessage;
use crate::services::claude::herdr_turn::{HerdrTurn, gate, observe_cold_prompt_for_tests};
use crate::services::claude_tui::host_input::busy::{BusyState, observe_herdr};
use crate::services::discord::turn_presence::activity::{self, Activity};
use crate::services::provider::{CancelToken, ProviderKind};
use crate::services::session_host::herdr_socket_rig_tests::{
    HerdrRig, NODE, PANE as RIG_PANE, SESSION,
};
use crate::services::tui_o::turn_mode::TestConfirmation;
use crate::services::tui_o::writer::adoption::{self, Hold};
use crate::services::tui_o::writer::binding::{BindingEvents, BindingLog};
use crate::services::tui_prompt_dedupe::binding_context::{
    CapturedContext, HookBindingEnvelope, context_path, execution_context,
};
use crate::test_support::wait::{WAIT_CAP, join_within, poll_within, recv_within};
use poise::serenity_prelude::ChannelId;
use std::sync::atomic::Ordering;

const PROGRESS: &str = "first turn progress";

#[test]
fn cold_launch_first_progress_resolves_presence_output_and_busy_input_pg() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .unwrap();
    let mut herdr = Herdr::with_runtime("cold-first-turn", runtime);
    let rig = HerdrRig::start();
    let _hosts = crate::config::session_hosts::force_for_test(Some(NODE), &[(herdr.channel, NODE)]);
    let at = crate::config::session_hosts::herdr_endpoint(herdr.channel).unwrap();
    let _registry = rig.registry_as_on_this_thread(NODE, &at.key, &at.socket_path, SESSION);
    let endpoint = HerdrLaunchEndpoint {
        execution_node: NODE.into(),
        config_key: at.key,
        socket_addr: at.socket_path.display().to_string(),
        herdr_session: SESSION.into(),
    };
    herdr.owner.owner_node = NODE.into();
    herdr.owner.runtime_root = crate::config::runtime_root().unwrap().display().to_string();
    let projects = herdr._root.path().join("claude-projects");
    let project = projects.join("project");
    std::fs::create_dir_all(&project).unwrap();
    let session = uuid();
    let transcript = project.join(format!("{session}.jsonl"));
    let nonce = herdr.launch_on(
        &session,
        endpoint.clone(),
        Launcher {
            pane_id: RIG_PANE,
            evidence: rig.expected("unused"),
        },
        Some(projects),
    );
    let pending = current_row(&herdr);
    assert_eq!(
        (&pending.state, pending.execution_nonce.as_str()),
        (&HostedState::Pending, nonce.as_str())
    );
    let expected = pending.expected.as_ref().unwrap();
    let reader = Pane {
        id: RIG_PANE,
        endpoint: Some(HerdrEndpointId::of(pending.location.as_ref().unwrap())),
        reading: HerdrPaneReading::Present(HerdrPaneEvidence {
            binding_nonce: Some(nonce.clone()),
            root: Some(expected.root.clone()),
            provider_process: Some(expected.provider_process.clone()),
            agent_session_id: Some(session.clone()),
        }),
    };
    rig.run_provider(&context_path("claude", &nonce).unwrap(), false);
    rig.answer(
        "pane.read",
        json!({"type":"pane_read","read":{"pane_id":RIG_PANE,
            "source":"recent_unwrapped","text":"Working\n❯ \n",
            "revision":1,"truncated":false}}),
    );
    crate::services::claude_tui::readiness::initialize();
    let envelope = HookBindingEnvelope {
        context: CapturedContext::Captured(execution_context("claude", &nonce).unwrap().into()),
        observed: Default::default(),
    }
    .encode()
    .unwrap();
    assert!(!transcript.exists());
    let hook = |event: &str, payload: Value| {
        herdr
            .ingress
            .send_envelope(
                &format!("/hooks/claude/{event}?session_id={session}"),
                &payload,
                Some(&uuid()),
                Some(&envelope),
            )
            .0
    };
    assert_eq!(
        hook(
            "SessionStart",
            json!({"session_id":session,"source":"startup",
            "transcript_path":transcript})
        ),
        202
    );
    let attached = herdr.rt.block_on(attach_launched_herdr_source(
        &herdr.pool,
        &herdr.owner,
        herdr.channel,
        &nonce,
        &session,
        &transcript,
        &reader,
    ));
    assert!(matches!(
        attached,
        HerdrSourceAttach::Published { bound: false, .. }
    ));
    assert_eq!(
        hook(
            "UserPromptSubmit",
            json!({"session_id":session,"prompt":"first prompt",
            "transcript_path":transcript})
        ),
        202
    );
    assert!(!transcript.exists());
    let launch_pending = herdr.log().pop().unwrap();
    assert_eq!(
        launch_pending.execution_nonce.as_deref(),
        Some(nonce.as_str())
    );
    assert!(matches!(launch_pending.new, BindingTarget::Pending { .. }));
    let o_log = || BindingLog.binding_events_since(herdr.channel, 0).unwrap();
    assert!(matches!(
        adoption::logged(&o_log()),
        Err(refused) if refused.hold == Hold::Binding
    ));

    let first_prompt = json!({"type":"user","uuid":uuid(),"sessionId":session,
        "message":{"role":"user","content":"first prompt"}});
    let progress = json!({"type":"assistant","uuid":uuid(),"apiBlockIndex":0,"sessionId":session,
        "message":{"id":"first-turn","role":"assistant","content":[{"type":"text","text":PROGRESS}]}});
    std::fs::write(&transcript, format!("{first_prompt}\n{progress}\n")).unwrap();

    let (sender, frames) = std::sync::mpsc::channel();
    let cancel = Arc::new(CancelToken::new());
    let pool = herdr.pool.clone();
    let owner = herdr.owner.clone();
    let channel_id = herdr.channel;
    let worker_cancel = cancel.clone();
    let path = transcript.clone();
    let native = session.clone();
    let runtime = herdr.rt.handle().clone();
    let log_root = binding_events::test_root().unwrap();
    let install = rig.thread_installer(NODE, &endpoint.config_key, &at.socket_path, SESSION);
    let reader = std::thread::spawn(move || {
        install();
        let _runtime = runtime.enter();
        binding_events::set_test_root(Some(&log_root));
        let turn = HerdrTurn {
            pool: &pool,
            owner,
            channel_id,
            endpoint,
            row: None,
            prompt: "first prompt",
            working_dir: "/tmp",
            session_id: None,
            force_fresh: true,
            system_prompt: None,
            model: None,
            hook_endpoint: None,
            cancel: Some(worker_cancel),
        };
        observe_cold_prompt_for_tests(&turn, &pending, (&native, path), sender)
    });

    // No later hook or terminal is available to repair the first-turn decision.
    let snapshot = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let next = recv_within(&frames, WAIT_CAP);
        let first_content = match next {
            StreamMessage::OutputOffset { .. } => recv_within(&frames, WAIT_CAP),
            other => other,
        };
        assert!(
            matches!(&first_content, StreamMessage::Text { content } if content == PROGRESS),
            "first nonterminal progress did not reach the relay receiver: {first_content:?}"
        );
        assert!(herdr.log().iter().any(|event| {
            matches!(&event.new, BindingTarget::Resolved { pending_seq, .. } if *pending_seq == launch_pending.seq)
                && event.execution_nonce.as_deref() == Some(nonce.as_str())
                && source_of(event).is_some_and(|source| {
                    source.session_id == session && source.path == transcript
                })
        }), "the first progress frame must resolve the actual launch Pending");

        let bound = current_row(&herdr);
        assert_eq!(
            bound.state,
            HostedState::Bound,
            "the first progress frame must bind the row"
        );
        assert_eq!(bound.execution_nonce, nonce);
        let mut shared = crate::services::discord::make_shared_data_for_tests_with_storage(Some(
            herdr.pool.clone(),
        ));
        Arc::get_mut(&mut shared).unwrap().token_hash = herdr.owner.discord_token_hash.clone();
        let channel = ChannelId::new(herdr.channel);
        let _confirmation = TestConfirmation::new(herdr.channel);
        let reading = herdr
            .rt
            .block_on(poll_within(
                async || {
                    let judged =
                        activity::reading_now(&shared, &ProviderKind::Claude, channel).await;
                    (judged.observed.reason != "catching_up")
                        .then_some(judged)
                        .ok_or_else(|| "activity is still catching up".into())
                },
                WAIT_CAP,
                "first turn presence",
            ))
            .unwrap();
        assert_eq!(
            reading.observed.activity,
            Activity::Busy,
            "{:?}",
            reading.observed
        );
        assert_eq!(
            reading.notice_observation().target_label.as_deref(),
            Some(herdr.logical())
        );

        let events = o_log();
        let (sources, _) =
            adoption::logged(&events).expect("tui_o must no longer see binding_pending");
        assert!(sources.iter().any(|source| source.path == transcript));

        let target = gate(&bound).unwrap();
        assert!(target.execution_alive());
        let busy = observe_herdr(
            (herdr.channel, herdr.logical(), &nonce),
            (
                target,
                format!("{}#{RIG_PANE}", at.socket_path.display()),
                json!({}),
            ),
            |channel| adoption::holds_output(&BindingLog, channel),
        );
        assert_eq!(
            busy.state(),
            BusyState::Busy,
            "the live busy-input gate must recognize the verified transcript without a later hook"
        );
        assert_eq!(busy.transcript(), Some(transcript.as_path()));
        assert_eq!(busy.session_id(), Some(session.as_str()));
    }));
    cancel.cancelled.store(true, Ordering::Release);
    join_within(reader, WAIT_CAP).unwrap();
    if let Err(panic) = snapshot {
        std::panic::resume_unwind(panic);
    }
}

fn current_row(herdr: &Herdr) -> HostedExecution {
    let HostedLookup::Found(row) = herdr.rt.block_on(load(&herdr.pool, &herdr.owner)) else {
        panic!("the cold launch owns a canonical PG row");
    };
    let HostedRecord::Known(record) = row.record else {
        panic!("the canonical row names a known execution");
    };
    record
}
