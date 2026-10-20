use super::*;
use crate::services::discord::tui_direct_pending_start::{
    self as pending, PendingStartState, TuiDirectPendingStart,
};
use crate::services::tui_prompt_dedupe::hosted_source::{
    self,
    tests::{CHANNEL, Fixture, LOGICAL, N1, N2},
};

#[test]
fn an_output_source_survives_lease_and_pending_restore_without_restamping() {
    let fx = Fixture::new();
    let source = fx.source(N1, "native-one");
    fx.source(N2, "native-two");
    let shared = crate::services::discord::make_shared_data_for_tests();
    let lease = record_external_turn_lease_for_output(
        &shared,
        &ProviderKind::Claude,
        ChannelId::new(CHANNEL),
        LOGICAL,
        RuntimeHandoffKind::ClaudeTui,
        &source.path,
        chrono::Utc::now(),
    );
    assert_eq!(lease.hosted_source.as_ref().unwrap().execution_nonce, N1);
    assert_eq!(lease.hosted_source.as_ref().unwrap().source, source);
    let record = TuiDirectPendingStart {
        provider: "claude".into(),
        channel_id: CHANNEL,
        tmux_session_name: LOGICAL.into(),
        prompt_text: "original input".into(),
        anchor_message_id: CHANNEL + 1,
        lease_relay_owner: lease.relay_owner.as_str().into(),
        lease_runtime_kind: lease.runtime_kind.map(|kind| kind.as_str().to_owned()),
        lease_turn_id: lease.turn_id,
        lease_session_key: lease.session_key,
        generation: shared.restart.current_generation,
        created_at_ms: 0,
        observed_at_ms: 0,
        state: PendingStartState::Waiting,
        attempt_count: 0,
        captured_source: Some((source.path.display().to_string(), 0)),
        native_turn_id: None,
        hosted_source: lease.hosted_source,
        hosted_source_checked: false,
    };
    pending::persist(&record).unwrap();
    let restored = pending::load_all()
        .into_iter()
        .find(|saved| saved.anchor_message_id == record.anchor_message_id)
        .unwrap();
    assert_eq!(restored.hosted_source, record.hosted_source);
    assert_eq!(restored.hosted_source.as_ref().unwrap().execution_nonce, N1);
    let mut old_format = serde_json::to_value(&record).unwrap();
    old_format.as_object_mut().unwrap().remove("hosted_source");
    let old: TuiDirectPendingStart = serde_json::from_value(old_format).unwrap();
    assert!(old.hosted_source.is_none());
    assert_eq!(
        hosted_source::restore(
            "claude",
            LOGICAL,
            CHANNEL,
            old.captured_source.as_ref().unwrap(),
            ("native-one", N1),
        ),
        record.hosted_source,
        "the original native session and nonce restore N1 even while N2 is latest"
    );
    assert!(
        hosted_source::restore(
            "claude",
            LOGICAL,
            CHANNEL,
            old.captured_source.as_ref().unwrap(),
            ("native-one", "unrecorded-execution"),
        )
        .is_none()
    );
}

#[tokio::test(flavor = "current_thread")]
async fn the_claude_idle_scanner_carries_the_transcript_it_actually_read() {
    use crate::services::discord::tui_prompt_relay::relay_e2e::n1a_turn_mode::claude_idle_o_transition::fixture::{
        Pane, append,
    };
    let fx = Fixture::new();
    let _mode = crate::services::tui_o::cutover::test_override::force_channels(&[]);
    let pane = Pane::with_tmux(fx.root.path(), CHANNEL, LOGICAL.into());
    let _tmux = crate::services::provider_teardown::tests::test_support::FakeTmux::install(LOGICAL);
    let original = hosted_source::capture("claude", LOGICAL, CHANNEL, &pane.path, Some(&pane.a))
        .expect("the fixture durably bound its actual transcript");
    append(
        &pane.path,
        &format!(
            "{}\n",
            serde_json::json!({"type":"user", "sessionId":pane.a, "uuid":"source-entry",
                "message":{"role":"user", "content":[{"type":"text", "text":"scanned input"}]}})
        ),
    );
    let mut observed = crate::services::tui_prompt_dedupe::subscribe_observed_prompts();
    pane.idle().await;
    let prompt = observed
        .try_recv()
        .expect("the production transcript scanner published its observation");
    assert_eq!(prompt.prompt, "scanned input");
    assert_eq!(prompt.source_event_id.as_deref(), Some("source-entry"));
    assert_eq!(prompt.hosted_source.as_ref(), Some(&original));

    let mut context = crate::services::tui_prompt_dedupe::binding_context::execution_context(
        "claude",
        &original.execution_nonce,
    )
    .unwrap();
    context.execution_nonce = N2.into();
    context.created_at = chrono::Utc::now();
    context.launch_mode = "resume".into();
    let prepared =
        crate::services::tui_prompt_dedupe::binding_context::PreparedIncarnation::create(
            context.clone(),
        )
        .unwrap();
    std::fs::write(
        crate::services::tmux_common::session_temp_path(LOGICAL, "spawn_nonce"),
        N2,
    )
    .unwrap();
    std::fs::write(
        crate::services::tmux_common::session_temp_path(
            LOGICAL,
            crate::services::tmux_common::CLAUDE_TUI_LAUNCH_SCRIPT_TEMP_EXT,
        ),
        format!(
            "{}cd '{}'\nexec 'claude' '--resume' '{}'\n",
            prepared.env_lines(),
            fx.root.path().join("project").display(),
            pane.a
        ),
    )
    .unwrap();
    crate::services::tui_prompt_dedupe::binding_events::record_verified(
        &crate::services::tui_prompt_dedupe::binding_events::Proposal {
            channel_id: CHANNEL,
            provider: "claude",
            tmux_session: LOGICAL,
            session_id: Some(&pane.a),
            path: pane.path.to_str().unwrap(),
            replaced: None,
            cause: crate::services::tui_prompt_dedupe::binding_events::CauseSource::Observed,
            hook: None,
        },
        &original.source,
    )
    .unwrap();
    for (key, text, created, expected_nonce) in [
        (
            "resumed-entry",
            "new resumed input",
            Some(context.created_at),
            Some(N2),
        ),
        (
            "old-entry",
            "buffered previous input",
            Some(context.created_at - chrono::TimeDelta::seconds(1)),
            None,
        ),
        ("untimed-entry", "untimed previous input", None, None),
    ] {
        append(
            &pane.path,
            &format!(
                "{}\n",
                serde_json::json!({"type":"user", "sessionId":pane.a,
            "uuid":key, "timestamp":created.map(|at|at.to_rfc3339()),
            "message":{"role":"user", "content":[{"type":"text", "text":text}]}})
            ),
        );
        pane.idle().await;
        let prompt = observed
            .try_recv()
            .expect("the actual resumed scanner published its input");
        assert_eq!(prompt.source_event_id.as_deref(), Some(key));
        assert_eq!(
            prompt
                .hosted_source
                .as_ref()
                .map(|pin| pin.execution_nonce.as_str()),
            expected_nonce
        );
    }
}
