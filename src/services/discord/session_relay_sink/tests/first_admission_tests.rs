use super::*;
use crate::services::tui_o::channel_policy::{Adoption, BootChannels};
use crate::services::tui_o::cutover::test_override;
use crate::test_support::wait::{WAIT_CAP, await_within};

#[test]
fn a_sink_frame_waits_only_for_its_confirmed_admission() {
    let root = crate::config::TestRuntimeRootGuard::new();
    crate::services::tui_prompt_dedupe::run_serial_test(async {
        for mismatch in ["user", "started", "offset", "missing-offset", "own"] {
            let channel = 8_165_401;
            let session = "first-sink";
            crate::services::tmux_common::write_tmux_runtime_kind_marker(
                session,
                crate::services::agent_protocol::RuntimeHandoffKind::CodexTui,
            )
            .unwrap();
            let config = serde_json::from_value(serde_json::json!({"server":{},
                "agents":[{"id":"first", "name":"first", "channels":{"codex":{"id":channel.to_string(), "runtime":"tui"}}}],
                "tui_o":{"writer":{"channels":[channel], "late_adopt":true}}})).unwrap();
            let boot = BootChannels::validate(&config)
                .unwrap()
                .seeded_for_test(&config, |_, _| {
                    Ok(std::collections::BTreeMap::from([(
                        channel,
                        Adoption::Pending,
                    )]))
                })
                .unwrap();
            let candidate = boot.candidate(channel).unwrap().clone();
            let boot_guard = test_override::force_boot(boot);
            let token = crate::services::provider::CancelToken::new();
            let admission = candidate
                .admit_first(channel, token.turn_nonce().unwrap(), "hi", session, || {
                    Ok(0)
                })
                .unwrap();
            let mut row = inflight_with_identity_offset(channel, session, 702, "B", Some(0));
            row.provider = ProviderKind::Codex.as_str().to_owned();
            row.runtime_kind = Some(crate::services::agent_protocol::RuntimeHandoffKind::CodexTui);
            row.turn_nonce = token.turn_nonce().map(str::to_owned);
            row.current_msg_id = 88_702;
            row.set_relay_owner_kind(RelayOwnerKind::SessionBoundRelay);
            super::super::super::inflight::save_inflight_state(&row).unwrap();
            let registry = Arc::new(HealthRegistry::new());
            let shared = super::super::super::make_shared_data_for_tests();
            registry
                .register(ProviderKind::Codex.as_str().to_string(), shared)
                .await;
            let gateway = Arc::new(ShortReplaceFakeGateway {
                outcome: super::super::super::formatting::ReplaceLongMessageOutcome::EditedOriginal,
                ok: true,
                replace_calls: std::sync::atomic::AtomicUsize::new(0),
                on_replace: None,
            });
            let mut sink = SessionBoundDiscordRelaySink::new(registry);
            sink.test_gateway = Some(gateway.clone());
            let mut delivery = delivery_with_fence_offset(session, None, 702, "B", Some(0));
            delivery.channel_id = channel;
            delivery.provider = ProviderKind::Codex;
            match mismatch {
                "user" => delivery.frame_turn_user_msg_id = 701,
                "started" => delivery.frame_turn_started_at = "A".into(),
                "offset" => delivery.frame_turn_start_offset = Some(1),
                "missing-offset" => delivery.frame_turn_start_offset = None,
                "own" => {}
                _ => unreachable!(),
            }
            let mut work = Box::pin(sink.deliver_response(delivery));
            let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
            let first = std::future::Future::poll(work.as_mut(), &mut cx);
            if mismatch == "own" {
                assert!(first.is_pending());
                assert_eq!(gateway.replace_calls.load(Ordering::SeqCst), 0);
                assert_eq!(candidate.sends(), (0, 0));
                candidate.invalidate_first(token.turn_nonce(), None);
                await_within(work, WAIT_CAP).await.expect("Legacy response");
                assert_eq!(gateway.replace_calls.load(Ordering::SeqCst), 1);
            } else if first.is_pending() {
                await_within(work, WAIT_CAP).await.expect("Legacy response");
            }
            // An unrelated delivery may claim Legacy, but cannot consume B's admission.
            let mut held = Box::pin(crate::services::tui_o::cutover::wait_codex_first(
                channel,
                token.turn_nonce(),
            ));
            assert!(
                std::future::Future::poll(held.as_mut(), &mut cx).is_pending() || mismatch == "own"
            );
            drop(held);
            candidate.invalidate_first(token.turn_nonce(), None);
            drop(admission);
            drop(boot_guard);
            super::super::super::inflight::clear_inflight_state(&ProviderKind::Codex, channel);
        }
    });
    drop(root);
}
