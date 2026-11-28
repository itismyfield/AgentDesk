use std::sync::Arc;

use poise::serenity_prelude::{self as serenity, ChannelId, MessageId};

use crate::services::discord::recovery_engine::RecoveryDeliveryContext;
use crate::services::discord::recovery_engine::restore_inflight::ready_without_output_fixture_tests::Fixture;
use crate::services::discord::recovery_paths::shared::RecoveryRelayOutcome;
use crate::services::discord::{inflight, mailbox_snapshot, make_shared_data_for_tests};
use crate::services::provider::ProviderKind;

#[tokio::test(flavor = "current_thread")]
async fn partial_eof_actual_controller_preserves_frozen_prefix_and_streamed_current_anchor() {
    let _boot = crate::services::tui_o::cutover::test_override::force_channels(&[]);
    use crate::services::agent_protocol::RuntimeHandoffKind;
    use crate::services::discord::formatting::ReplaceLongMessageOutcome;
    use crate::services::discord::outbound::delivery_record as dr;
    use crate::services::discord::recovery_paths::controller_cutover::{
        deliver_recovery_replace_via_controller, tests::RecoveryFakeGateway,
    };
    use crate::services::tui_prompt_dedupe as dedupe;
    let _guard = crate::config::test_env_lock::acquire_shared_test_env_lock();
    for typed_provider in [None, Some(ProviderKind::Claude), Some(ProviderKind::Codex)] {
        let typed = typed_provider.is_some();
        let native = typed_provider == Some(ProviderKind::Codex);
        let provider = typed_provider.unwrap_or(ProviderKind::Claude);
        let runtime = if native {
            RuntimeHandoffKind::CodexTui
        } else {
            RuntimeHandoffKind::ClaudeTui
        };
        #[cfg(not(unix))]
        if typed {
            continue;
        }
        let root = tempfile::tempdir().expect("runtime root");
        let env = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
            "AGENTDESK_ROOT_DIR",
            root.path(),
        );
        let mut fixture = Fixture::new(5_071_805, root, env, make_shared_data_for_tests());
        fixture.state.provider = provider.as_str().into();
        if native {
            fixture.state.turn_source = inflight::TurnSource::ExternalInput;
            fixture.state.request_owner_user_id = 1;
            fixture.state.injected_prompt_message_id = Some(fixture.state.user_msg_id);
            fixture.state.external_turn_id = Some("native-retained-original".into());
            fixture.state.relay_owner_kind = inflight::RelayOwnerKind::None;
            fixture.state.session_id = None;
        }
        fixture.state.streaming_rollover_frozen_msg_ids = vec![40];
        fixture.state.current_msg_id = 41;
        // Rollover froze prefix in 40; 41 already shows the beginning of its suffix.
        // A normal streaming edit does not advance response_sent_offset.
        let mut messages = std::collections::BTreeMap::from([
            (MessageId::new(40), "published prefix".to_string()),
            (MessageId::new(41), "unposted".to_string()),
        ]);
        if typed {
            fixture.state.runtime_kind = Some(runtime);
            fixture.state.turn_nonce = Some("typed-prefix-recovery".into());
            let assistant = serde_json::json!({"type":"assistant", "sessionId":fixture.state.session_id, "message":{"content":[
            {"type":"text", "text":fixture.state.full_response}]}});
            let terminal = serde_json::json!({"type":"result", "session_id":fixture.state.session_id, "subtype":"success", "result":fixture.state.full_response});
            let (assistant, terminal) = if native {
                (
                    serde_json::json!({"type":"response_item", "payload":{"type":"message", "role":"assistant", "content":[{"type":"output_text", "text":fixture.state.full_response}]}}),
                    serde_json::json!({"type":"event_msg", "payload":{"type":"task_complete", "last_agent_message":fixture.state.full_response}}),
                )
            } else {
                (assistant, terminal)
            };
            let output = fixture.state.output_path.as_ref().unwrap();
            std::fs::write(output, format!("{assistant}\n{terminal}\n")).unwrap();
            fixture.state.last_offset = std::fs::metadata(output).unwrap().len();
            let tmux = fixture.state.tmux_session_name.as_ref().unwrap();
            std::fs::write(
                crate::services::tmux_common::session_temp_path(tmux, "generation"),
                b"typed-prefix",
            )
            .unwrap();
            dedupe::register_tmux_runtime_binding(
                tmux,
                dedupe::TuiRuntimeBinding {
                    runtime_kind: runtime,
                    output_path: output.clone(),
                    relay_output_path: None,
                    input_fifo_path: None,
                    session_id: fixture.state.session_id.clone(),
                    last_offset: 0,
                    relay_last_offset: None,
                },
            );
        }
        fixture.claim().await;
        fixture.state = fixture.load().unwrap();
        let channel = ChannelId::new(fixture.state.channel_id);
        let original_actor = mailbox_snapshot(&fixture.shared, channel)
            .await
            .cancel_token
            .unwrap();
        let source: Option<dr::ExactJsonlSourceIdentity> = if typed {
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                let output = fixture.state.output_path.clone().unwrap();
                let file = std::fs::File::open(&output).unwrap();
                let metadata = file.metadata().unwrap();
                let tmux = fixture.state.tmux_session_name.clone().unwrap();
                let raw = fixture.state.full_response.clone();
                let frame = crate::services::discord::StreamMessage::ClaudeTuiTerminalDone {
                    result: raw.clone(),
                    session_id: fixture.state.session_id.clone(),
                    transcript_path: std::fs::canonicalize(&output)
                        .unwrap()
                        .display()
                        .to_string(),
                    tmux_session_name: tmux.clone(),
                    turn_nonce: fixture.state.turn_nonce.clone().unwrap(),
                    source_start: 0,
                    complete_record_end: metadata.len(),
                    generation_mtime_ns: dr::current_generation_mtime_ns(&tmux),
                    source_file_dev: metadata.dev(),
                    source_file_ino: metadata.ino(),
                    actor: Arc::downgrade(&original_actor),
                    kind: crate::services::agent_protocol::NativeTerminalKind::Completed,
                };
                let frame = if native {
                    crate::services::discord::StreamMessage::CodexTuiTerminalDone {
                        result: raw.clone(),
                        session_id: fixture.state.session_id.clone(),
                        rollout_path: std::fs::canonicalize(&output)
                            .unwrap()
                            .display()
                            .to_string(),
                        tmux_session_name: tmux.clone(),
                        turn_nonce: fixture.state.turn_nonce.clone().unwrap(),
                        source_start: 0,
                        complete_record_end: metadata.len(),
                        captured_source: Some(
                            crate::services::agent_protocol::CapturedTuiTerminalSource {
                                generation_mtime_ns: dr::current_generation_mtime_ns(&tmux),
                                source_file_dev: metadata.dev(),
                                source_file_ino: metadata.ino(),
                                actor: Arc::downgrade(&original_actor),
                            },
                        ),
                        kind: crate::services::agent_protocol::NativeTerminalKind::Completed,
                    }
                } else {
                    frame
                };
                let mut baseline = fixture.state.clone();
                let identity = inflight::InflightTurnIdentity::from_state(&fixture.state);
                let (_, admitted, _, _) = fixture
                    .state
                    .admit_tui_terminal_frame(
                        &mut baseline,
                        &identity,
                        true,
                        (&fixture.shared, &original_actor),
                        &raw,
                        frame,
                    )
                    .await
                    .expect("the real retained source admits with a nonzero sent prefix");
                let admitted = admitted.expect("typed source range");
                assert_eq!(admitted.result, raw);
                assert_eq!(admitted.source.range, (0, metadata.len()));
                assert_eq!(
                    fixture.load().unwrap().response_sent_offset,
                    "published prefix\n".len()
                );
                assert!(fixture.state.requires_pinned_terminal_recovery());
                Some(admitted.source)
            }
            #[cfg(not(unix))]
            {
                unreachable!()
            }
        } else {
            None
        };
        #[cfg(unix)]
        if native {
            // The admitted body survives a process loss; later raw turns are not
            // part of its saved terminal range or its Discord receipt.
            drop(original_actor);
            fixture.shared =
                crate::services::discord::make_shared_data_for_tests_with_storage(None);
            fixture.state = fixture.load().unwrap();
            let retained = fixture.state.clone();
            let mut legacy = retained.clone();
            legacy.tui_terminal_source_file_identity = None;
            assert!(
                !legacy.requires_pinned_terminal_recovery(),
                "legacy Codex generation-only terminals retain their previous path"
            );
            let output = std::path::PathBuf::from(retained.output_path.as_ref().unwrap());
            let next_raw = "{\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"user\",\"content\":[{\"type\":\"input_text\",\"text\":\"next turn\"}]}}\n";
            use std::io::Write;
            std::fs::OpenOptions::new()
                .append(true)
                .open(&output)
                .unwrap()
                .write_all(next_raw.as_bytes())
                .unwrap();
            let tmux = retained.tmux_session_name.as_deref().unwrap();
            dedupe::clear_tmux_runtime_binding(tmux);
            let rebuilt = crate::services::discord::tui_prompt_relay::rehydration::codex_tui_rehydrated_binding_from_rollout_path(
                tmux, &output, retained.session_id.clone(),
            ).unwrap();
            assert!(
                rebuilt.last_offset > retained.last_offset,
                "startup observes EOF including the next turn"
            );
            dedupe::register_rehydrated_tmux_runtime_binding(
                provider.as_str(),
                tmux,
                channel.get(),
                rebuilt,
            );
            let expected_file = std::fs::read(&output).unwrap();
            let gateway = RecoveryFakeGateway::new(ReplaceLongMessageOutcome::EditedOriginal, true);
            let http = Arc::new(serenity::Http::new("Bot test-token"));
            assert!(crate::services::discord::recovery_engine::recover_idle_partial_response_from_ready_source(
                &http, &fixture.shared, &retained, &output, &gateway,
            ).await, "the actual dormant recovery publishes the saved native range after restart and append");
            let source = source.unwrap();
            assert!(dr::confirmed_delivery_receipt_exists(
                &provider, channel, 41, &source
            ));
            assert_eq!(
                dr::read_record(&provider, channel.get())
                    .unwrap()
                    .confirmed_deliveries
                    .len(),
                1
            );
            assert!(fixture.load().is_none());
            assert_eq!(gateway.replacements.lock().unwrap().len(), 1);
            assert_eq!(
                gateway.replacements.lock().unwrap()[0].1,
                crate::services::discord::formatting::format_for_discord_with_provider(
                    "unposted suffix",
                    &provider
                )
            );
            let mut next = retained.clone();
            next.user_msg_id += 10;
            next.turn_nonce = Some("native-next-turn".into());
            next.full_response = "NEXT_RESPONSE".into();
            next.response_sent_offset = 0;
            inflight::save_inflight_state(&next).unwrap();
            let before = fixture.load().unwrap();
            assert!(!crate::services::discord::recovery_engine::recover_idle_partial_response_from_ready_source(
                &http, &fixture.shared, &retained, &output, &gateway,
            ).await, "the old captured source cannot deliver again over a successor");
            assert_eq!(fixture.load().unwrap().turn_nonce, before.turn_nonce);
            assert_eq!(gateway.replacements.lock().unwrap().len(), 1);
            assert_eq!(std::fs::read(&output).unwrap(), expected_file);
            continue;
        }
        let gateway = RecoveryFakeGateway::new(ReplaceLongMessageOutcome::EditedOriginal, true)
            .before_replace_returns({
                let shared = fixture.shared.clone();
                let actor = original_actor.clone();
                move || {
                    Box::pin(async move {
                        assert!(
                            mailbox_snapshot(&shared, channel)
                                .await
                                .cancel_token
                                .as_ref()
                                .is_some_and(|current| Arc::ptr_eq(current, &actor)),
                            "the original actor owns the turn during actual gateway transport"
                        );
                    })
                }
            });
        let http = Arc::new(serenity::Http::new("Bot test-token"));
        let context = RecoveryDeliveryContext::from_state(
            &fixture.shared,
            &ProviderKind::Claude,
            &fixture.state,
            None,
            fixture.shared.restart.current_generation,
        );
        let settled = fixture
            .settle(&fixture.state, |text| {
                let gateway = &gateway;
                let shared = &fixture.shared;
                let http = &http;
                let context = context.as_ref();
                let state = &fixture.state;
                async move {
                    if typed {
                        return crate::services::discord::recovery_engine::completion_delivery::relay_captured_recovery_terminal_notice_with_gateway(
                            http, shared, &ProviderKind::Claude, state, &text, gateway,
                        ).await;
                    }
                    deliver_recovery_replace_via_controller(
                        gateway,
                        shared,
                        &ProviderKind::Claude,
                        http,
                        channel,
                        MessageId::new(41),
                        &text,
                        context,
                        None,
                    )
                    .await.into()
                }
            },)
            .await;
        assert!(
            settled,
            "typed={typed}: captured recovery settles the original actor"
        );
        let replacements = gateway
            .replacements
            .lock()
            .expect("actual controller transport calls")
            .clone();
        let expected = crate::services::discord::formatting::format_for_discord_with_provider(
            &fixture.state.full_response[fixture.state.response_sent_offset..],
            &ProviderKind::Claude,
        );
        assert_eq!(
            replacements,
            vec![(MessageId::new(41), expected)],
            "typed={typed}: gateway receives exactly the undelivered suffix"
        );
        for (message, body) in replacements.iter() {
            messages.insert(*message, body.clone());
        }
        assert_eq!(
            messages.get(&MessageId::new(40)).unwrap(),
            "published prefix"
        );
        assert_eq!(
            messages.get(&MessageId::new(41)).unwrap(),
            &crate::services::discord::formatting::format_for_discord_with_provider(
                "unposted suffix",
                &ProviderKind::Claude,
            )
        );
        if let Some(source) = source {
            let record = dr::read_record(&ProviderKind::Claude, channel.get()).unwrap();
            assert_eq!(record.confirmed_deliveries.len(), 1);
            assert!(
                dr::confirmed_delivery_receipt_exists(&ProviderKind::Claude, channel, 41, &source),
                "the suffix publication confirms the original whole-source range"
            );
        }
        assert!(fixture.load().is_none());
        assert!(
            mailbox_snapshot(&fixture.shared, channel)
                .await
                .cancel_token
                .is_none()
        );
        let mut next = fixture.state.clone();
        next.user_msg_id += 10;
        next.turn_nonce = Some("next-prefix-input".into());
        assert!(fixture.next_input_claims(&next).await);
    }
}
