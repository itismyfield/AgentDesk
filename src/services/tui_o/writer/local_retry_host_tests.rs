use super::*;
use crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui;
use crate::services::tui_o::cutover::test_override;
use crate::services::tui_o::shadow::binding_reader::source_id_for;
use crate::services::tui_o::store::fault::{self, Keep, Step as At};
use crate::services::tui_o::store::ledger::{LocalRetryMark, PieceOutcome};
use crate::services::tui_o::store::{Initialized, OStore, StoreConfig};
use sha2::Digest;

async fn ticks(seconds: u64) {
    tokio::time::sleep(std::time::Duration::from_secs(seconds)).await;
}

fn row(text: &str) -> Vec<u8> {
    let value = serde_json::json!({"type":"assistant", "uuid":text, "apiBlockIndex":0, "message":{"id":text,"content":[{"type":"text","text":text}]}});
    [serde_json::to_vec(&value).unwrap(), b"\n".to_vec()].concat()
}

#[tokio::test(start_paused = true)]
async fn local_retry_host_recovers_after_transient_halts_and_a_stopped_host_sends_nothing() {
    use std::io::Write;
    let channel = 63_254_100;
    let _selected = test_override::force_channels(&[(channel, ClaudeTui)]);
    let runtime = tempfile::tempdir().unwrap();
    let path = runtime.path().join("transcript.jsonl");
    std::fs::write(&path, []).unwrap();
    let source = source_id_for("retry", &path).unwrap();
    let store = OStore::open_if_enabled(&StoreConfig { enabled: true }, runtime.path())
        .unwrap()
        .unwrap();
    let era = store
        .begin_era(&[channel], chrono::Utc::now(), |channel| {
            Ok(Initialized {
                channel,
                sources: vec![crate::services::tui_o::store::InitSource {
                    source_id: source.clone(),
                    delivery_start: 0,
                    prefix_hash: hex::encode(sha2::Sha256::digest(b"")),
                }],
                initial_anchor: 100,
                build_digest: "test".into(),
                at: chrono::Utc::now(),
            })
        })
        .unwrap();
    let io = TestHost::new([(channel, source)]);
    locked(&io.posts.lost).push_back(true);
    let gate = Arc::new(OwnershipGate::default());
    gate.acquired();
    let ready = Arc::new(Readiness::default());
    let start = || {
        super::super::start_managed(
            ShadowProvider::Claude,
            true,
            crate::services::tui_o::cutover::boot_ownership(),
            || HostParts {
                io: io.clone(),
                runtime_root: Some(runtime.path().to_path_buf()),
                gate: gate.clone(),
                readiness: ready.clone(),
            },
        )
    };
    let mut hosts = start();
    assert_eq!(hosts.len(), 1);
    ticks(3).await;
    std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap()
        .write_all(&row("durable payload"))
        .unwrap();
    // Stop between the NotFound poll and the next tick, before consuming its durable budget.
    for _ in 0..4000 {
        if locked(&io.alarms.0)
            .iter()
            .any(|(_, alarm)| matches!(alarm, WriterAlarm::NotFound { .. }))
        {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(
        locked(&io.alarms.0)
            .iter()
            .any(|(_, alarm)| matches!(alarm, WriterAlarm::NotFound { .. }))
    );
    hosts.pop().unwrap().stop_and_join().await.unwrap();
    ticks(70).await;
    assert_eq!(io.posts.to(channel), ["durable payload"]);
    let dir = runtime.path().join("o_store").join(channel.to_string());
    let watch = fault::watch(&dir);
    // Fail the child Prepared's append twice: host recover_after must withdraw it, retaining budget.
    let failures = fault::plant(
        &dir.join("ledger.jsonl"),
        At::Append(Keep::All),
        std::io::ErrorKind::StorageFull,
        Some(2),
    );
    let mut hosts = start();
    ticks(240).await;
    assert!(
        watch.opens().len() >= 3,
        "two real host recoveries: {:?}",
        watch.opens()
    );
    assert_eq!(watch.withdrawals(), 2);
    assert_eq!(io.posts.to(channel), ["durable payload", "durable payload"]);
    hosts.pop().unwrap().stop_and_join().await.unwrap();
    drop(failures);
    for _ in 0..2 {
        let mut hosts = start();
        ticks(70).await;
        hosts.pop().unwrap().stop_and_join().await.unwrap();
    }
    assert_eq!(io.posts.to(channel), ["durable payload", "durable payload"]);
    let reopened = store.open_channel(&era, channel).unwrap().unwrap();
    assert_eq!(reopened.ledger().next_serial(), 2);
    assert_eq!(
        reopened.ledger().piece(1).unwrap().local_retry,
        Some(LocalRetryMark::Attempt { root_serial: 0 })
    );
    assert!(matches!(
        reopened.ledger().piece(1).unwrap().outcome,
        Some(PieceOutcome::Posted(_))
    ));
}
