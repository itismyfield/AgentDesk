use super::*;
use crate::config::TestRuntimeRootGuard;
use crate::services::discord::health::turn_deliver::inject_codex_tests::QueuedCodex;
use serde_json::json;

#[tokio::test(flavor = "current_thread")]
async fn cold_submit_returns_exact_attempt_and_releases_writer_before_source_pg() {
    const CHILD: &str = "ADK_COLD_EFFECT_PG_CHILD";
    let name = concat!(
        module_path!(),
        "::cold_submit_returns_exact_attempt_and_releases_writer_before_source_pg"
    )
    .strip_prefix(concat!(env!("CARGO_CRATE_NAME"), "::"))
    .unwrap();
    if std::env::var(CHILD).as_deref() != Ok(name) {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", name, "--nocapture"])
            .env(CHILD, name)
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        println!("{stdout}\n{}", String::from_utf8_lossy(&output.stderr));
        assert!(
            output.status.success() && stdout.contains("1 passed; 0 failed; 0 ignored;"),
            "cold PG entry must run exactly one test"
        );
        return;
    }
    let _root = TestRuntimeRootGuard::new();
    let fixture = QueuedCodex::new(7_272_391).await;
    let mailbox = fixture
        .shared
        .mailbox(poise::serenity_prelude::ChannelId::new(
            fixture.pane.channel,
        ));
    let path = fixture.disk_path().with_file_name("cold-owner.json");
    mailbox.v2_adopt(path, "cold-owner".into()).await.unwrap();
    let mut observers = Vec::new();
    let mut keys = Vec::new();
    for (index, (provider, host)) in [
        ("claude", "tmux"),
        ("codex", "tmux"),
        ("claude", "herdr"),
        ("codex", "herdr"),
    ]
    .into_iter()
    .enumerate()
    {
        let mut original: crate::services::turn_orchestrator::PendingQueueItem = serde_json::from_value(json!({
            "author_id":7,"message_id":4000,"channel_id":7272391,"source_message_ids":[4000],"queued_generation":5,"text":"cold"})).unwrap();
        original.message_id = 8_000_000_000_000_004_000 + index as u64;
        original.source_message_ids = vec![original.message_id];
        original.text = format!("cold {index}\n_ literal");
        let frame = original.text.clone();
        let attempt = serde_json::from_value(json!({"nonce":format!("cold-{index}"),"provider":provider,
            "target":{"host":host,"owner":"same physical cold pane","root":"/test","execution_nonce":"known-spawn"},
            "source":{"kind":"pre_source","expected_session_id":null,"expected_transcript":null,"binding_watermark":null}})).unwrap();
        let expected = mailbox
            .v2_request(move |slot| {
                let owner = slot.as_mut().unwrap();
                if index > 0 {
                    owner.tick(index as u64 * 31_000)?;
                }
                owner
                    .enqueue_fixture(&original)
                    .map_err(|error| format!("{error:?}"))?;
                owner.reserve(
                    attempt,
                    serde_json::from_value(
                        json!({"frame":frame,"digest":"digest","attribution":{}}),
                    )
                    .unwrap(),
                )
            })
            .await
            .unwrap();
        let (tx, rx) = std::sync::mpsc::channel::<()>();
        let completion = submit(&mailbox, index as u64 * 31_000, move |frame| {
            assert!(
                crate::services::tui_input::human_frame::history::v2::scoped_source().is_some(),
                "cold effect owns its exact V2 history scope"
            );
            assert_eq!(
                frame,
                format!("cold {index}\n_ literal"),
                "cold adapter preserves frame bytes"
            );
            Ok(rx)
        })
        .await
        .unwrap_or_else(|error| panic!("cold writer must be released before Source: {error}"));
        assert_eq!(
            serde_json::to_value(&completion.attempt).unwrap(),
            serde_json::to_value(&expected).unwrap(),
            "cold completion retains exact immutable attempt"
        );
        assert!(
            completion.outcome.is_ok(),
            "next cold submit must not await the previous Source"
        );
        let snapshot = mailbox
            .v2_request(|slot| {
                let owner = slot.as_ref().unwrap();
                Ok((
                    owner
                        .active_attempt()
                        .ok()
                        .map(|key| serde_json::to_value(key).unwrap()),
                    owner.snapshot.terminal_identities.len(),
                ))
            })
            .await
            .unwrap();
        assert_eq!(
            snapshot.0,
            Some(serde_json::to_value(&completion.attempt).unwrap()),
            "cold adapter must not settle before receipt"
        );
        assert_eq!(snapshot.1, 0, "cold effect is not a native receipt");
        observers.push((tx, completion.outcome.unwrap()));
        keys.push(serde_json::to_value(&completion.attempt).unwrap());
    }
    let history = crate::services::tui_input::human_frame::history::v2::Registry::runtime()
        .unwrap()
        .json()
        .unwrap();
    assert_eq!(
        history["planned"].as_array().unwrap().len(),
        4,
        "PreSource history precedes all four cold effects"
    );
    let late = keys[0].clone();
    mailbox.v2_settle_exact(late, true, 100_000).await.unwrap();
    let active = mailbox
        .v2_request(|slot| slot.as_ref().unwrap().active_attempt())
        .await
        .unwrap();
    assert_eq!(
        serde_json::to_value(active).unwrap(),
        keys[3],
        "late cold A must never settle the current B"
    );
    assert_eq!(
        observers.len(),
        4,
        "all four cold combinations return before their Source exists"
    );
    mailbox.v2_request(|slot| {
        let owner = slot.as_mut().unwrap();
        owner.tick(124_000)?;
        let item = serde_json::from_value(json!({"author_id":7,"message_id":9999,"channel_id":7272391,"source_message_ids":[9999],"queued_generation":5,"text":"unsupported custody"})).unwrap();
        owner.enqueue_fixture(&item).map_err(|error| format!("{error:?}"))?;
        owner.reserve(serde_json::from_value(json!({"nonce":"unsupported", "provider":"remote", "target":{"host":"herdr","owner":"remote","root":"/test","execution_nonce":"spawn"},"source":{"kind":"pre_source","expected_session_id":null,"expected_transcript":null,"binding_watermark":null}})).unwrap(),
            serde_json::from_value(json!({"frame":"unsupported custody","digest":"digest","attribution":{}})).unwrap())?;
        Ok(())
    }).await.unwrap();
    let refused = submit(&mailbox, 124_000, |_| Ok(())).await;
    assert!(
        matches!(refused, Err(ref error) if error == "provider_unsupported"),
        "unsupported cold must be refused before arm"
    );
    let phase = mailbox
        .v2_request(|slot| {
            Ok(serde_json::to_value(
                slot.as_ref()
                    .unwrap()
                    .snapshot
                    .head_injection
                    .as_ref()
                    .unwrap()
                    .phase,
            )
            .unwrap())
        })
        .await
        .unwrap();
    assert_eq!(
        phase,
        json!("reserved"),
        "unsupported cold must preserve Reserved custody"
    );
}
