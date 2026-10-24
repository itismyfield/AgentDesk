use super::*;
use crate::config::TestRuntimeRootGuard;
use crate::services::discord::health::turn_deliver::inject_codex_tests::QueuedCodex;
use crate::services::tui_input::human_frame::history::v2::Registry;
use serde_json::json;
use std::os::unix::fs::MetadataExt;

#[tokio::test(flavor = "current_thread")]
async fn exact_history_adapter_pastes_once_and_reconciles_after_restart_pg() {
    const PRIVATE: &str = "ADK_HISTORY_PRIVATE_PG_TEST";
    let name = concat!(
        module_path!(),
        "::exact_history_adapter_pastes_once_and_reconciles_after_restart_pg"
    )
    .strip_prefix(concat!(env!("CARGO_CRATE_NAME"), "::"))
    .unwrap();
    if std::env::var(PRIVATE).as_deref() != Ok(name) {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", name, "--nocapture"])
            .env(PRIVATE, name)
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        println!("{stdout}\n{stderr}");
        assert!(
            output.status.success()
                && stdout.contains("test result: ok. 1 passed; 0 failed; 0 ignored;"),
            "private PG history entry must execute exactly once"
        );
        return;
    }
    let _root = TestRuntimeRootGuard::new();
    let fixture = QueuedCodex::new(7_272_380).await;
    let request = HumanInputRequest {
        channel_id: ChannelId::new(fixture.pane.channel),
        provider: ProviderKind::Codex,
        text: fixture.head.text.clone(),
        author_id: 1,
        source: "queued".into(),
        metadata: None,
        channel_name_hint: Some("inject-history".into()),
    };
    let target = super::super::resolve(&fixture.shared, &request, true)
        .await
        .unwrap();
    assert!(super::super::queued_target_matches(&fixture.shared, &request, &target, true).await);
    let namespace = "history-pg";
    let mailbox = fixture.shared.mailbox(request.channel_id);
    mailbox
        .v2_adopt(fixture.disk_path(), namespace.into())
        .await
        .unwrap();
    let history = Registry::runtime().unwrap();
    let nonce = "feed0380";
    let frame = busy_inject::frame("source", "1", "history user", nonce, &fixture.head.text);
    let meta = std::fs::metadata(&target.transcript).unwrap();
    let native = QueuedProvider::Codex
        .session(&target.transcript, None)
        .unwrap();
    let encoded = json!({"nonce":nonce,"provider":"codex","target":{"host":"tmux","owner":target.session,
        "root":"/tmp","execution_nonce":"execution-history"},"source":{"kind":"existing","session_id":native,
            "transcript":target.transcript,"file":[meta.dev(),meta.ino()],"offset":meta.len()}});
    let prepared = frame.clone();
    let registry = history.clone();
    let key = mailbox
        .v2_request(move |slot| {
            let owner = slot.as_mut().unwrap();
            let key = owner.reserve(
                serde_json::from_value(encoded).unwrap(),
                serde_json::from_value(
                    json!({"frame":prepared,"digest":"prepared","attribution":{}}),
                )
                .unwrap(),
            )?;
            owner.prepare_history(&registry)?;
            Ok(key)
        })
        .await
        .unwrap();
    let adapter = QueuedProvider::Codex
        .receipt_adapter(&history, namespace, &key)
        .unwrap();
    let bound = Bound {
        session: target.session.clone(),
        path: target.transcript.clone(),
        file: (meta.dev(), meta.ino()),
        session_id: native,
    };
    let input = Input {
        channel: fixture.pane.channel,
        provider: "codex".into(),
        source: "source".into(),
        author: "1".into(),
        name: "history user".into(),
        text: fixture.head.text.clone(),
        nonce: nonce.into(),
    };
    let expected = key.clone();
    assert!(
        !mailbox
            .v2_effect(1000, move |prepared| {
                let target = target;
                assert_eq!(prepared, frame);
                let (outcome, key) = adapter.run(
                    (&super::super::pane(input.channel, &target.session), None),
                    (
                        &target.session,
                        &target.transcript,
                        target.native_turn.as_deref(),
                    ),
                    None,
                    MessageId::new(expected.identity.primary),
                    &input,
                )?;
                assert_eq!(
                    serde_json::to_value(key).unwrap(),
                    serde_json::to_value(expected).unwrap()
                );
                assert!(
                    matches!(outcome, InjectAttempt::Injected { .. }),
                    "exact adapter must consume the real tmux receipt: {outcome:?}"
                );
                assert!(adapter.proof(&bound).is_some());
                let mut wrong = bound.clone();
                wrong.file.1 += 1;
                assert!(
                    adapter.proof(&wrong).is_none(),
                    "exact receipt must reject file tuple mismatch"
                );
                wrong = bound.clone();
                wrong.session_id.push('x');
                assert!(adapter.proof(&wrong).is_none());
                wrong = bound.clone();
                wrong.path = wrong.path.with_extension("foreign");
                assert!(adapter.proof(&wrong).is_none());
                Ok(false)
            })
            .await
            .unwrap()
    );
    let count = fixture.pane.keys();
    assert_eq!(count, "paste-buffer+send-keys");
    let registry = history.clone();
    mailbox
        .v2_request(move |slot| {
            slot.as_mut().unwrap().tick(31_000)?;
            slot.as_ref().unwrap().sync_history(&registry)
        })
        .await
        .unwrap();
    mailbox
        .v2_adopt(fixture.disk_path(), namespace.into())
        .await
        .unwrap();
    let registry = history.clone();
    mailbox
        .v2_request(move |slot| slot.as_ref().unwrap().sync_history(&registry))
        .await
        .unwrap();
    // Simulate the vanished process-local ledger; durable queue/history remain intact.
    let binding = crate::services::tui_prompt_dedupe::runtime_binding_for_tmux_session(
        &fixture.pane.session(),
    )
    .unwrap();
    crate::services::tui_prompt_dedupe::reset_state_for_tests();
    crate::services::tui_prompt_dedupe::register_tmux_runtime_binding(
        &fixture.pane.session(),
        binding,
    );
    // The native client consumed the first input; the composer is empty after restart.
    std::fs::remove_file(fixture.pane.path("pasted")).unwrap();
    let target = super::super::resolve(&fixture.shared, &request, true)
        .await
        .unwrap();
    let retry_adapter = QueuedProvider::Codex
        .receipt_adapter(&history, namespace, &key)
        .unwrap();
    let retry_input = Input {
        channel: fixture.pane.channel,
        provider: "codex".into(),
        source: "source".into(),
        author: "1".into(),
        name: "history user".into(),
        text: fixture.head.text.clone(),
        nonce: nonce.into(),
    };
    let retry_primary = key.identity.primary;
    let retried = mailbox
        .v2_effect(32_000, move |_| {
            let target = target;
            let (outcome, _) = retry_adapter.run(
                (
                    &super::super::pane(retry_input.channel, &target.session),
                    None,
                ),
                (
                    &target.session,
                    &target.transcript,
                    target.native_turn.as_deref(),
                ),
                None,
                MessageId::new(retry_primary),
                &retry_input,
            )?;
            Ok(matches!(outcome, InjectAttempt::Injected { .. }))
        })
        .await;
    assert_eq!(
        fixture.pane.keys(),
        count,
        "restart reconciliation must add zero paste/Enter keys"
    );
    assert!(
        retried.is_err(),
        "an armed restored attempt cannot acquire a new writer permit"
    );
    let adapter = QueuedProvider::Codex
        .receipt_adapter(&history, namespace, &key)
        .unwrap();
    let transcript = fixture.pane.path("rollout.jsonl");
    let meta = std::fs::metadata(&transcript).unwrap();
    let bound = Bound {
        session: fixture.pane.session(),
        path: transcript,
        file: (meta.dev(), meta.ino()),
        session_id: format!("queued-codex-{}", fixture.pane.channel),
    };
    assert!(
        adapter.proof(&bound).is_some(),
        "restart recovers the exact native receipt"
    );
    mailbox
        .v2_settle_exact(serde_json::to_value(&key).unwrap(), true, 32_001)
        .await
        .unwrap();
    let registry = history.clone();
    mailbox
        .v2_request(move |slot| slot.as_ref().unwrap().sync_history(&registry))
        .await
        .unwrap();
    assert_eq!(
        fixture.pane.keys(),
        count,
        "restart reconciliation must add zero paste/Enter keys"
    );
    let snapshot = mailbox
        .v2_request(|slot| slot.as_ref().unwrap().json())
        .await
        .unwrap();
    assert!(snapshot["retained_inputs"].as_array().unwrap().is_empty());
    assert_eq!(
        history.json().unwrap()["passive"].as_array().unwrap().len(),
        1
    );

    for index in 1..=11u64 {
        let text = if index == 11 { "fresh C" } else { "same body" };
        let emit_receipt = index >= 10;
        let deliver = index == 11;
        let target = super::super::resolve(&fixture.shared, &request, true)
            .await
            .unwrap();
        let nonce = format!("feed{index:04x}");
        let frame = busy_inject::frame("source", "1", "history user", &nonce, text);
        let meta = std::fs::metadata(&target.transcript).unwrap();
        let native = QueuedProvider::Codex
            .session(&target.transcript, None)
            .unwrap();
        let attempt = json!({"nonce":nonce,"provider":"codex","target":{"host":"tmux","owner":target.session,
            "root":"/tmp","execution_nonce":"execution-history"},"source":{"kind":"existing","session_id":native,
            "transcript":target.transcript,"file":[meta.dev(),meta.ino()],"offset":meta.len()}});
        let original =
            serde_json::from_value::<crate::services::turn_orchestrator::PendingQueueItem>(json!({
                "author_id":1,"message_id":fixture.head.message_id.get()+index,
                "source_message_ids":[fixture.head.message_id.get()+index],"queued_generation":0,
                "text":text,"channel_id":fixture.pane.channel
            }))
            .unwrap();
        let registry = history.clone();
        let prepared = frame.clone();
        let key = mailbox
            .v2_request(move |slot| {
                let owner = slot.as_mut().unwrap();
                assert!(owner.enqueue(&original)?);
                let key = owner.reserve(
                    serde_json::from_value(attempt).unwrap(),
                    serde_json::from_value(
                        json!({"frame":prepared,"digest":"prepared","attribution":{}}),
                    )
                    .unwrap(),
                )?;
                owner.prepare_history(&registry)?;
                Ok(key)
            })
            .await
            .unwrap();
        let marker = fixture.pane.path("defer.receipt");
        if emit_receipt {
            let _ = std::fs::remove_file(&marker);
        } else {
            std::fs::write(&marker, "defer").unwrap();
        }
        let _ = std::fs::remove_file(fixture.pane.path("pasted"));
        let adapter = QueuedProvider::Codex
            .receipt_adapter(&history, namespace, &key)
            .unwrap();
        let input = Input {
            channel: fixture.pane.channel,
            provider: "codex".into(),
            source: "source".into(),
            author: "1".into(),
            name: "history user".into(),
            text: text.into(),
            nonce,
        };
        let primary = key.identity.primary;
        let delivered = mailbox
            .v2_effect(40_000 + index * 40_000, move |prepared| {
                let target = target;
                assert_eq!(prepared, frame);
                let (outcome, _) = adapter.run(
                    (&super::super::pane(input.channel, &target.session), None),
                    (
                        &target.session,
                        &target.transcript,
                        target.native_turn.as_deref(),
                    ),
                    None,
                    MessageId::new(primary),
                    &input,
                )?;
                assert_eq!(
                    matches!(outcome, InjectAttempt::Injected { .. }),
                    deliver,
                    "retained A cannot block fresh same-body B or C: {outcome:?}"
                );
                Ok(deliver)
            })
            .await
            .unwrap();
        assert_eq!(delivered, deliver);
        let registry = history.clone();
        mailbox
            .v2_request(move |slot| {
                let owner = slot.as_mut().unwrap();
                if !deliver {
                    owner.tick(71_000 + index * 40_000)?;
                }
                owner.sync_history(&registry)
            })
            .await
            .unwrap();
        assert_eq!(
            fixture.pane.keys().split('+').count(),
            2 * (index as usize + 1),
            "each new input performs exactly one content paste and Enter"
        );
    }
    let snapshot = mailbox
        .v2_request(|slot| slot.as_ref().unwrap().json())
        .await
        .unwrap();
    assert!(snapshot["head_injection"].is_null());
    assert_eq!(
        snapshot["retained_inputs"].as_array().unwrap().len(),
        10,
        "nine Unknown originals and ambiguous same-body B survive fresh C"
    );
    assert_eq!(
        snapshot["retained_inputs"][0]["input"]["original"]["text"],
        "same body"
    );
}
