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

impl crate::services::turn_orchestrator::ChannelMailboxHandle {
    pub(crate) async fn v2_cold_fixture(
        &self,
        path: std::path::PathBuf,
        attempt: serde_json::Value,
        frame: String,
        channel: u64,
    ) -> Result<(), String> {
        self.v2_adopt(path, format!("cold-scope-{channel}")).await?;
        self.v2_request(move |slot| {
            let owner=slot.as_mut().ok_or("owner absent")?;
            let id=8_000_000_000_100_000_000u64+channel;
            let original=serde_json::from_value(serde_json::json!({"author_id":7,"channel_id":channel,"message_id":id,"source_message_ids":[id],"queued_generation":1,"text":frame})).map_err(|e|e.to_string())?;
            owner.enqueue_fixture(&original).map_err(|e|format!("{e:?}"))?;
            owner.reserve(serde_json::from_value(attempt).map_err(|e|e.to_string())?,serde_json::from_value(serde_json::json!({"frame":frame,"digest":"fixture","attribution":{}})).map_err(|e|e.to_string())?)?;
            Ok(())
        }).await
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn completion_clock_keeps_retained_a_fence_without_touching_active_b_pg() {
    use std::sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    };
    let _root = TestRuntimeRootGuard::new();
    let f = QueuedCodex::new(7_272_415).await;
    let mailbox = f
        .shared
        .mailbox(poise::serenity_prelude::ChannelId::new(f.pane.channel));
    mailbox
        .v2_adopt(f.disk_path(), "completion-clock".into())
        .await
        .unwrap();
    let (key,original)=mailbox.v2_request(|s| {
        let owner=s.as_mut().unwrap();let original=serde_json::to_value(&owner.snapshot.items[0]).map_err(|e|e.to_string())?;
        let attempt=serde_json::from_value(json!({"nonce":"clock-a","provider":"codex","target":{"host":"tmux","owner":"clock-target","root":"/test","execution_nonce":"execution"},"source":{"kind":"pre_source","expected_session_id":null,"expected_transcript":null,"binding_watermark":null}})).unwrap();
        let key=owner.reserve(attempt,serde_json::from_value(json!({"frame":"clock frame","digest":"clock","attribution":{}})).unwrap())?;
        Ok((serde_json::to_value(key).map_err(|e|e.to_string())?,original))
    }).await.unwrap();
    let clock = Arc::new(AtomicU64::new(1000));
    let read = clock.clone();
    let (entered, ready) = tokio::sync::oneshot::channel();
    let (release, wait) = std::sync::mpsc::channel();
    let handle = mailbox.clone();
    let task = tokio::spawn(async move {
        handle
            .v2_effect_clock(
                1000,
                Arc::new(move || read.load(Ordering::SeqCst)),
                move |_| {
                    entered.send(()).unwrap();
                    wait.recv().unwrap();
                    Ok(true)
                },
            )
            .await
    });
    ready.await.unwrap();
    mailbox.v2_request(|s| {let owner=s.as_mut().unwrap();owner.tick(31_000)?;
        let item=serde_json::from_value(json!({"author_id":7,"message_id":7272416,"channel_id":7272415,"source_message_ids":[7272416],"queued_generation":1,"text":"active B"})).unwrap();
        owner.enqueue_fixture(&item)?;let attempt=serde_json::from_value(json!({"nonce":"clock-b","provider":"codex","target":{"host":"tmux","owner":"clock-target","root":"/test","execution_nonce":"execution"},"source":{"kind":"pre_source","expected_session_id":null,"expected_transcript":null,"binding_watermark":null}})).unwrap();
        owner.reserve(attempt,serde_json::from_value(json!({"frame":"B","digest":"B","attribution":{}})).unwrap())?;Ok(())}).await.unwrap();
    let completed = 1000 + 25 * 60 * 60 * 1000;
    clock.store(completed, Ordering::SeqCst);
    release.send(()).unwrap();
    assert_eq!(task.await.unwrap(), Ok(true));
    let snap = mailbox
        .v2_request(|s| s.as_ref().unwrap().json())
        .await
        .unwrap();
    assert_eq!(
        snap["terminal_identities"][0]["settled_at_ms"], completed,
        "settlement must read the completion clock, not ARM time"
    );
    assert_eq!(
        snap["head_injection"]["original"]["text"], "active B",
        "late A cannot settle active B"
    );
    mailbox
        .v2_request(move |s| {
            let owner = s.as_mut().unwrap();
            owner.tick(completed + 86_399_999)?;
            let item = serde_json::from_value(original.clone()).unwrap();
            assert!(
                !owner.enqueue_fixture(&item)?,
                "completion fence lasts a full day"
            );
            owner.tick(completed + 86_400_000)?;
            assert!(
                owner.enqueue_fixture(&serde_json::from_value(original).unwrap())?,
                "completion fence expires from actual settlement"
            );
            Ok(())
        })
        .await
        .unwrap();
    assert_eq!(key["attempt"]["nonce"], "clock-a");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn live_payload_refusal_proof_preserves_raw_without_becoming_restart_authority_pg() {
    use crate::services::discord::runtime_store::durable_probe::{Plan, Step, Watch};
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    let _root = TestRuntimeRootGuard::new();
    let fixture = QueuedCodex::new(7_272_425).await;
    let mailbox = fixture
        .shared
        .mailbox(poise::serenity_prelude::ChannelId::new(
            fixture.pane.channel,
        ));
    let path = fixture.disk_path().with_file_name("payload-refusal.json");
    let attempt = json!({"nonce":"payload-refusal","provider":"codex","target":{"host":"herdr","owner":"payload-refusal","root":"/test","execution_nonce":"execution"},"source":{"kind":"pre_source","expected_session_id":null,"expected_transcript":null,"binding_watermark":null}});
    mailbox
        .v2_cold_fixture(
            path.clone(),
            attempt.clone(),
            "original kept after refusal".into(),
            7272425,
        )
        .await
        .unwrap();
    let keys = Arc::new(AtomicUsize::new(0));
    let count = keys.clone();
    let disk = path.clone();
    let (key, outcome) = mailbox
        .v2_cold_observed(1, Arc::new(|| 90000), move |_| {
            let _watch = Watch::new(
                &disk,
                Plan {
                    fail: vec![Step::ParentSync],
                    ..Default::default()
                },
            );
            crate::services::claude_tui::owner_input::payload(|| {
                count.fetch_add(1, Ordering::SeqCst);
                Ok(())
            })
        })
        .await
        .unwrap();
    assert!(outcome.result.is_err());
    assert_eq!(
        keys.load(Ordering::SeqCst),
        0,
        "failed payload commit authorizes no content I/O"
    );
    let proof = outcome
        .not_sent
        .expect("a live refusing gate proves no content was entered");
    mailbox
        .v2_request(move |s| {
            let owner = s.as_mut().unwrap();
            owner.recover()?;
            owner.retain_not_sent(
                &serde_json::from_value(key).map_err(|e| e.to_string())?,
                proof,
            )
        })
        .await
        .unwrap();
    let snap = mailbox
        .v2_request(|s| s.as_ref().unwrap().json())
        .await
        .unwrap();
    assert!(snap["head_injection"].is_null());
    assert_eq!(
        snap["retained_inputs"][0]["reason"]["kind"],
        "known_not_sent"
    );
    assert_eq!(
        snap["retained_inputs"][0]["input"]["original"]["text"],
        "original kept after refusal"
    );
    mailbox
        .v2_adopt(path, "cold-scope-7272425".into())
        .await
        .unwrap();
    assert!(
        mailbox
            .v2_effect(100000, |_| panic!("retained proof is not a replay permit"))
            .await
            .is_err()
    );
    assert_eq!(keys.load(Ordering::SeqCst), 0);
    let other = fixture
        .shared
        .mailbox(poise::serenity_prelude::ChannelId::new(7272426));
    other
        .v2_cold_fixture(
            fixture.disk_path().with_file_name("ambiguous-content.json"),
            attempt,
            "maybe sent".into(),
            7272426,
        )
        .await
        .unwrap();
    let (_, outcome) = other
        .v2_cold_observed(1, Arc::new(|| 90000), |_| {
            crate::services::claude_tui::owner_input::payload(|| {
                Err::<(), _>("ambiguous transport acknowledgement".into())
            })
        })
        .await
        .unwrap();
    assert!(outcome.result.is_err());
    assert!(
        outcome.not_sent.is_none(),
        "content entered before an error cannot become KnownNotSent"
    );
}
