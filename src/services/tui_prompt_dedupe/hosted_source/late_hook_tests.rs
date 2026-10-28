use super::tests::{CHANNEL, Fixture, LOGICAL, N1, N2};
use super::*;
use crate::services::tui_prompt_dedupe as dedupe;

fn hosted(nonce: &str) {
    std::fs::write(
        crate::services::tmux_common::session_temp_path(LOGICAL, "host_kind"),
        "herdr",
    )
    .unwrap();
    dedupe::install_herdr_execution(LOGICAL, nonce);
    dedupe::admit_herdr_execution(LOGICAL, nonce);
}

fn hook(source: &SourceId, text: &str) -> dedupe::PromptObservation {
    observe_hook_at(
        "claude",
        LOGICAL,
        text,
        (None, None),
        chrono::Utc::now(),
        &serde_json::json!({"session_id":source.session_id,"transcript_path":source.path}),
        None,
    )
}

fn unowned() {
    assert!(!dedupe::external_input_relay_lease_present(
        "claude", LOGICAL, CHANNEL
    ));
    assert!(!dedupe::is_ssh_direct_observation_pending(
        "claude", LOGICAL
    ));
}

#[test]
fn a_hook_without_context_keeps_its_unambiguous_admitted_source() {
    let fx = Fixture::new();
    let source = fx.source(N1, "unambiguous-native");
    hosted(N1);
    let mut observed = dedupe::subscribe_observed_prompts();
    assert_eq!(
        hook(&source, "old-format direct input"),
        dedupe::PromptObservation::PublishedSshDirect
    );
    let prompt = observed
        .try_recv()
        .expect("matching admitted hook actually published");
    assert_eq!(prompt.hosted_source.unwrap().execution_nonce, N1);
    assert!(dedupe::external_input_relay_lease_present(
        "claude", LOGICAL, CHANNEL
    ));
    assert!(dedupe::is_ssh_direct_observation_pending("claude", LOGICAL));
}

#[test]
fn a_hook_without_context_cannot_choose_between_two_nonces_of_one_file() {
    use std::io::Write;
    let fx = Fixture::new();
    let source = fx.source(N1, "reused-native");
    let original = capture(
        "claude",
        LOGICAL,
        CHANNEL,
        &source.path,
        Some(&source.session_id),
    )
    .unwrap();
    let path = fx
        .root
        .path()
        .join(binding_events::BINDING_EVENTS_DIR)
        .join(format!("{CHANNEL}.log"));
    let records = binding_events::records_strict(CHANNEL).unwrap().unwrap();
    let event = binding_events::BindingEvent {
        seq: records.len() as u64 + 1,
        channel_id: CHANNEL,
        provider: "claude".into(),
        tmux_session: LOGICAL.into(),
        execution_nonce: Some(N2.into()),
        old: Some(source.clone()),
        new: BindingTarget::Source(source.clone()),
        cause: binding_events::BindingCause::Resume,
        parent_hint: None,
        evidence: binding_events::BindingEvidence {
            hook_event: Some("session_start".into()),
            received_at: chrono::Utc::now(),
        },
        committed_at: chrono::Utc::now(),
    };
    let mut log = std::fs::OpenOptions::new().append(true).open(path).unwrap();
    writeln!(log, "{}", serde_json::to_string(&event).unwrap()).unwrap();
    std::fs::write(
        crate::services::tmux_common::session_temp_path(LOGICAL, "spawn_nonce"),
        N2,
    )
    .unwrap();
    hosted(N2);
    let mut observed = dedupe::subscribe_observed_prompts();
    assert_eq!(
        hook(&source, "ambiguous old-format direct input"),
        dedupe::PromptObservation::Ignored
    );
    assert!(observed.try_recv().is_err());
    unowned();
    assert!(
        validate(&original),
        "adoption refusal keeps the already admitted historical source usable"
    );
}

#[test]
fn the_spawn_marker_alone_cannot_replace_exact_admission() {
    let fx = Fixture::new();
    let source = fx.source(N1, "admission-native");
    hosted(N2);
    assert_eq!(
        hook(&source, "wrong admission nonce"),
        dedupe::PromptObservation::Ignored
    );
    unowned();
    dedupe::install_herdr_execution(LOGICAL, N1);
    assert_eq!(
        hook(&source, "withheld admission"),
        dedupe::PromptObservation::Ignored
    );
    unowned();
    dedupe::admit_herdr_execution(LOGICAL, N1);
    assert_eq!(
        hook(&source, "now admitted"),
        dedupe::PromptObservation::PublishedSshDirect
    );
    assert!(dedupe::external_input_relay_lease_present(
        "claude", LOGICAL, CHANNEL
    ));
}

#[test]
fn an_ordinary_tmux_hook_without_context_keeps_its_existing_observation_path() {
    let fx = Fixture::new();
    let source = fx.source(N1, "tmux-native");
    assert_eq!(herdr_evidence(CHANNEL, LOGICAL, None), Ok(false));
    assert_eq!(
        hook(&source, "ordinary tmux input"),
        dedupe::PromptObservation::PublishedSshDirect
    );
    assert!(dedupe::external_input_relay_lease_present(
        "claude", LOGICAL, CHANNEL
    ));
}
