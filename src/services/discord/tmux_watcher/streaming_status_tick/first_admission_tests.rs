use super::*;
use crate::services::tui_o::channel_policy::{Adoption, BootChannels};
use crate::services::tui_o::cutover::test_override;
use crate::test_support::wait::{WAIT_CAP, await_within};

#[test]
fn a_streaming_tick_waits_only_for_its_captured_turn_and_source() {
    let root = crate::config::TestRuntimeRootGuard::new();
    crate::services::tui_prompt_dedupe::run_serial_test(async {
        let dir = tempfile::tempdir().unwrap();
        for mismatch in ["turn", "source", "own"] {
            let mut fx = seed_row(dir.path(), 8165, false, false);
            let mut captured = load_inflight_state(&fx.provider, fx.channel.get()).unwrap();
            fx.provider = ProviderKind::Codex;
            captured.provider = fx.provider.as_str().to_owned();
            save_inflight_state(&captured).unwrap();
            let channel = fx.channel.get();
            let config = serde_json::from_value(serde_json::json!({"server":{},
                "agents":[{"id":"first", "name":"first", "channels":{"codex":{"id":channel.to_string(), "runtime":"tui"}}}],
                "tui_o":{"writer":{"channels":[channel], "late_adopt":true}}})).unwrap();
            let boot = BootChannels::validate(&config)
                .unwrap()
                .seeded(&config, |_, _| {
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
                .admit_first(channel, token.turn_nonce().unwrap(), "hi", &fx.tmux, || {
                    Ok(0)
                })
                .unwrap();
            let mut row = load_inflight_state(&fx.provider, channel).unwrap();
            row.turn_nonce = token.turn_nonce().map(str::to_owned);
            if mismatch == "turn" {
                row.started_at.push('B');
            }
            if mismatch == "source" {
                row.output_path = Some(dir.path().join("successor.jsonl").display().to_string());
            }
            save_inflight_state(&row).unwrap();
            let shared = crate::services::discord::make_shared_data_for_tests();
            let rec = recorder(fx.channel, true).await;
            let mut locals = tick_locals(&fx, None);
            let mut tick = Box::pin(run_tick_body(
                &mut locals,
                &rec,
                &shared,
                &fx,
                false,
                "",
                WatcherToolState::new(),
            ));
            let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
            let first = std::future::Future::poll(tick.as_mut(), &mut cx);
            if mismatch == "own" {
                assert!(first.is_pending());
                assert_eq!(rec.total(), 0);
                candidate.invalidate_first(token.turn_nonce(), None);
                await_within(tick, WAIT_CAP).await;
            } else if first.is_pending() {
                await_within(tick, WAIT_CAP).await;
            }
            candidate.invalidate_first(token.turn_nonce(), None);
            drop(admission);
            drop(boot_guard);
        }
    });
    drop(root);
}
