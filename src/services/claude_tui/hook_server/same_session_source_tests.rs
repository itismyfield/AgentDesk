use super::*;
use crate::services::tui_prompt_dedupe::binding_context::{
    BINDING_HEADER, CapturedContext, HookBindingEnvelope, PreparedIncarnation,
};
use crate::services::tui_prompt_dedupe::binding_events::{claude_history, pinned_source};
use crate::services::tui_prompt_dedupe::{
    claude_session_rotation_for_tmux, register_launched_tmux_runtime_binding,
};

fn execution(ingress: &Ingress, channel: u64, session: &str) -> (String, HookBindingEnvelope) {
    let tmux = format!("source-{}", uuid());
    let prepared = PreparedIncarnation::prepare_at(
        "claude",
        &tmux,
        Some(channel),
        Some(session),
        false,
        ingress
            .dir
            .path()
            .parent()
            .map(std::path::Path::to_path_buf),
    )
    .unwrap();
    let marker = crate::services::tmux_common::session_temp_path(&tmux, "spawn_nonce");
    fs::create_dir_all(std::path::Path::new(&marker).parent().unwrap()).unwrap();
    fs::write(marker, &prepared.context.execution_nonce).unwrap();
    (
        tmux,
        HookBindingEnvelope {
            context: CapturedContext::Captured(prepared.context),
            observed: Default::default(),
        },
    )
}

fn launch(ingress: &Ingress, tmux: &str, channel: u64, session: &str) -> TuiRuntimeBinding {
    let path = ingress.transcript(session);
    let mut binding = claude(&path, session);
    binding.last_offset = 17;
    register_provider_session("claude", session, tmux);
    register_tmux_channel(tmux, channel);
    register_launched_tmux_runtime_binding(tmux, binding.clone());
    assert!(pinned_source(channel, tmux).unwrap().is_none());
    binding
}

fn observe(session: &str, event: &str, payload: &Value, envelope: &HookBindingEnvelope) {
    let mut headers = HeaderMap::new();
    headers.insert(BINDING_HEADER, envelope.encode().unwrap().parse().unwrap());
    assert_eq!(
        observe_binding_hook(
            "claude",
            event,
            Some(session),
            Some(session),
            payload,
            &headers
        ),
        IngressOutcome::Proceed(ProceedReason::NoSessionSwitch)
    );
}

fn assert_pin(channel: u64, tmux: &str, envelope: &HookBindingEnvelope) {
    let CapturedContext::Captured(context) = &envelope.context else {
        panic!("the launch has a captured execution");
    };
    let (pin, history) = claude_history(channel, tmux, Some(&context.execution_nonce)).unwrap();
    let pin = pin.expect("an authentic hook verifies the registered source");
    assert!(history.complete);
    assert!(history.awaiting.is_none());
    assert_eq!(history.current.unwrap().pin.as_ref(), Some(&pin));
}

#[test]
fn same_session_hook_pins_registered_source_once_without_moving_binding() {
    let (_root, _env) = crate::services::tui_prompt_dedupe::binding_context::tests::fixture();
    let ingress = Ingress::new();
    for (channel, event) in [(658_101, "SessionStart"), (658_102, "UserPromptSubmit")] {
        let session = uuid();
        let (tmux, envelope) = execution(&ingress, channel, &session);
        let binding = launch(&ingress, &tmux, channel, &session);
        let before = events(channel).len();
        let payload = ingress.payload(&session, Some("startup"));
        observe(&session, event, &payload, &envelope);
        assert_pin(channel, &tmux, &envelope);
        assert_eq!(events(channel).len(), before + 1);
        let verified = events(channel);
        observe(&session, event, &payload, &envelope);
        assert_eq!(
            events(channel),
            verified,
            "an already verified source adds no pin"
        );
        assert_eq!(runtime_binding_for_tmux_session(&tmux), Some(binding));
        assert!(claude_session_rotation_for_tmux(&tmux).is_none());
    }
}

#[test]
fn same_session_hook_refuses_replaced_inode_and_foreign_first_record() {
    let (_root, _env) = crate::services::tui_prompt_dedupe::binding_context::tests::fixture();
    let ingress = Ingress::new();
    for (channel, replace) in [(658_103, true), (658_104, false)] {
        let session = uuid();
        let (tmux, envelope) = execution(&ingress, channel, &session);
        let binding = launch(&ingress, &tmux, channel, &session);
        let path = ingress.path(&session);
        if replace {
            fs::rename(&path, path.with_extension("old")).unwrap();
            ingress.transcript(&session);
        } else {
            fs::write(&path, format!("{}\n", json!({"sessionId": uuid()}))).unwrap();
        }
        observe(
            &session,
            "UserPromptSubmit",
            &ingress.payload(&session, None),
            &envelope,
        );
        assert!(pinned_source(channel, &tmux).unwrap().is_none());
        assert_eq!(runtime_binding_for_tmux_session(&tmux), Some(binding));
        assert!(claude_session_rotation_for_tmux(&tmux).is_none());
    }
}

#[test]
fn concurrent_same_session_hooks_publish_one_pin() {
    let (_root, _env) = crate::services::tui_prompt_dedupe::binding_context::tests::fixture();
    let ingress = Ingress::new();
    let (channel, session) = (658_105, uuid());
    let (tmux, envelope) = execution(&ingress, channel, &session);
    let binding = launch(&ingress, &tmux, channel, &session);
    let before = events(channel).len();
    let workers: Vec<_> = (0..2)
        .map(|_| {
            let (session, envelope, payload) = (
                session.clone(),
                envelope.clone(),
                ingress.payload(&session, None),
            );
            let root = ingress._root.path().to_path_buf();
            std::thread::spawn(move || {
                set_test_root(Some(&root));
                observe(&session, "UserPromptSubmit", &payload, &envelope);
                set_test_root(None);
            })
        })
        .collect();
    for worker in workers {
        crate::test_support::wait::join_within(worker, crate::test_support::wait::WAIT_CAP);
    }
    assert_pin(channel, &tmux, &envelope);
    assert_eq!(events(channel).len(), before + 1);
    assert_eq!(runtime_binding_for_tmux_session(&tmux), Some(binding));
}

#[test]
fn same_session_hook_before_registration_waits_for_later_registered_hook() {
    let (_root, _env) = crate::services::tui_prompt_dedupe::binding_context::tests::fixture();
    let ingress = Ingress::new();
    let (channel, session) = (658_106, uuid());
    let (tmux, envelope) = execution(&ingress, channel, &session);
    ingress.transcript(&session);
    let payload = ingress.payload(&session, Some("startup"));
    observe(&session, "SessionStart", &payload, &envelope);
    assert!(pinned_source(channel, &tmux).unwrap().is_none());
    assert!(events(channel).is_empty());
    launch(&ingress, &tmux, channel, &session);
    observe(&session, "UserPromptSubmit", &payload, &envelope);
    assert_pin(channel, &tmux, &envelope);
}

#[test]
fn same_session_hook_without_registered_stat_cannot_create_first_pin() {
    let (_root, _env) = crate::services::tui_prompt_dedupe::binding_context::tests::fixture();
    let ingress = Ingress::new();
    let (channel, session) = (658_107, uuid());
    let (tmux, envelope) = execution(&ingress, channel, &session);
    let path = ingress.transcript(&session);
    register_provider_session("claude", &session, &tmux);
    register_tmux_runtime_binding(&tmux, claude(&path, &session));
    register_tmux_channel(&tmux, channel);
    observe(
        &session,
        "UserPromptSubmit",
        &ingress.payload(&session, None),
        &envelope,
    );
    assert!(pinned_source(channel, &tmux).unwrap().is_none());
    assert!(events(channel).is_empty());
}

#[test]
fn same_session_hook_failed_pin_append_is_refused_until_retry_persists() {
    let (_root, _env) = crate::services::tui_prompt_dedupe::binding_context::tests::fixture();
    let ingress = Ingress::new();
    let (channel, session) = (658_108, uuid());
    let (tmux, envelope) = execution(&ingress, channel, &session);
    let binding = launch(&ingress, &tmux, channel, &session);
    let before = events(channel);
    let payload = ingress.payload(&session, None);
    let mut headers = HeaderMap::new();
    headers.insert(BINDING_HEADER, envelope.encode().unwrap().parse().unwrap());
    APPEND_FAULT.with(|slot| slot.set(Some("write")));
    let outcome = observe_binding_hook(
        "claude",
        "UserPromptSubmit",
        Some(&session),
        Some(&session),
        &payload,
        &headers,
    );
    APPEND_FAULT.with(|slot| slot.set(None));
    assert_eq!(
        outcome,
        IngressOutcome::NotDurable(NotDurableReason::Append)
    );
    assert!(pinned_source(channel, &tmux).unwrap().is_none());
    assert_eq!(events(channel), before);
    assert_eq!(runtime_binding_for_tmux_session(&tmux), Some(binding));
    observe(&session, "UserPromptSubmit", &payload, &envelope);
    assert_pin(channel, &tmux, &envelope);
    assert_eq!(events(channel).len(), before.len() + 1);
}
