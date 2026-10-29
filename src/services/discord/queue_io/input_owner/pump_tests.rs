use super::*;
use crate::config::TestRuntimeRootGuard;
use crate::services::discord::queue_io::input_owner::admission::{
    AdmissionTicket, TurnBind, admit, exact_turn,
};
use crate::services::discord::runtime_store::discord_pending_queue_root;
use crate::services::turn_orchestrator::{ChannelMailboxRegistry, PendingQueueItem};
use poise::serenity_prelude::ChannelId;
use serde_json::json;
use std::time::Duration;

fn original(channel: u64, text: &str) -> PendingQueueItem {
    let id = 8_000_000_000_000_004_000 + channel % 1000;
    serde_json::from_value(json!({"author_id":7,"message_id":id,"channel_id":channel,
        "source_message_ids":[id],"queued_generation":1,"text":text}))
    .unwrap()
}

/// An adopted owner on `channel` holding one admitted original under a fresh ticket.
async fn admitted(
    registry: &ChannelMailboxRegistry,
    channel: u64,
    text: &str,
) -> (ChannelMailboxHandle, AdmissionTicket, PendingQueueItem) {
    let dir = discord_pending_queue_root()
        .unwrap()
        .join("claude/input-owner-pump");
    std::fs::create_dir_all(&dir).unwrap();
    let handle = registry.handle(ChannelId::new(channel));
    let path = dir.join(format!("{channel}.json"));
    handle.v2_adopt(path, "input-owner".into()).await.unwrap();
    let (ticket, item) = (AdmissionTicket::new(), original(channel, text));
    let admission = admit(&handle, &item, ticket.clone()).await;
    assert!(matches!(
        admission,
        super::super::admission::Admission::Accepted(_)
    ));
    (handle, ticket, item)
}

/// A Herdr target, so the scope reads no tmux pane; `pane` names the physical writer.
fn prepared(nonce: &str, pane: &str, frame: &str, sources: &[&str]) -> PreparedPayload {
    PreparedPayload {
        frame: frame.into(),
        sources: sources.iter().map(|s| s.to_string()).collect(),
        attempt: json!({"nonce":nonce,"provider":"claude",
            "target":{"host":"herdr","owner":pane,"root":"/pump","execution_nonce":nonce},
            "source":{"kind":"pre_source","expected_session_id":null,
                "expected_transcript":null,"binding_watermark":null}}),
    }
}

fn clock() -> Clock {
    Arc::new(|| 10_000)
}

async fn head(handle: &ChannelMailboxHandle) -> serde_json::Value {
    let snapshot = handle
        .v2_request(|s| s.as_ref().unwrap().json())
        .await
        .unwrap();
    snapshot["head_injection"].clone()
}

/// The frame two sources were combined into reaches the leaf once and whole, and the owner's
/// head names both sources; the leaf runs while the admitted ticket's bind is armed to it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pump_runs_one_attempt_with_a_multi_source_frame() {
    let _root = TestRuntimeRootGuard::new();
    let registry = ChannelMailboxRegistry::default();
    let frame = "[first person] one\n[second person] two";
    let (handle, ticket, item) = admitted(&registry, 7_272_481, frame).await;
    let frames = Arc::new(Mutex::new(Vec::new()));
    let (seen, mailbox) = (frames.clone(), handle.clone());
    let (t, i) = (ticket.clone(), item.clone());
    let leaf: Leaf = Box::new(move |frame| {
        seen.lock().unwrap().push(frame);
        let bind = tokio::runtime::Handle::current().block_on(exact_turn(&mailbox, &t, &i));
        assert!(
            matches!(&bind, Ok(TurnBind::Armed(turn)) if turn.attempt == "pump-multi"),
            "the effect runs under an armed exact turn: {bind:?}"
        );
        Ok(Completion::Submitted)
    });
    let completion = submit(
        &handle,
        prepared("pump-multi", "pane-multi", frame, &["s1", "s2"]),
        clock(),
        leaf,
    )
    .await;
    assert_eq!(completion, Ok(Completion::Submitted));
    assert_eq!(
        *frames.lock().unwrap(),
        [frame],
        "one effect, the frame unsplit"
    );
    assert!(
        head(&handle).await.is_null(),
        "a delivered head leaves custody"
    );
    let bind = exact_turn(&handle, &ticket, &item).await.unwrap();
    assert!(
        matches!(&bind, TurnBind::Settled(turn) if turn.attempt == "pump-multi"),
        "{bind:?}"
    );
}

/// Two owners writing one pane take turns and both deliver; on another pane they overlap.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn same_target_attempts_run_one_at_a_time_and_other_targets_overlap() {
    let _root = TestRuntimeRootGuard::new();
    let registry = ChannelMailboxRegistry::default();
    for (same, channels) in [
        (true, [7_272_482, 7_272_483]),
        (false, [7_272_484, 7_272_485]),
    ] {
        let (a, ..) = admitted(&registry, channels[0], "first").await;
        let (b, ..) = admitted(&registry, channels[1], "second").await;
        let running = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let overlapped = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let leaf = || -> Leaf {
            let (running, overlapped) = (running.clone(), overlapped.clone());
            Box::new(move |_| {
                use std::sync::atomic::Ordering::SeqCst;
                if running.fetch_add(1, SeqCst) > 0 {
                    overlapped.store(true, SeqCst);
                }
                let deadline = std::time::Instant::now() + Duration::from_millis(600);
                while std::time::Instant::now() < deadline && !overlapped.load(SeqCst) {
                    std::thread::sleep(Duration::from_millis(10));
                }
                running.fetch_sub(1, SeqCst);
                Ok(Completion::Submitted)
            })
        };
        let pane_b = if same { "pane-shared" } else { "pane-other" };
        let (first, second) = tokio::join!(
            submit(
                &a,
                prepared(&format!("lane{same}a"), "pane-shared", "first", &["a"]),
                clock(),
                leaf()
            ),
            submit(
                &b,
                prepared(&format!("lane{same}b"), pane_b, "second", &["b"]),
                clock(),
                leaf()
            ),
        );
        let submitted = Ok(Completion::Submitted);
        assert_eq!(
            (first, second),
            (submitted.clone(), submitted),
            "same={same}"
        );
        assert_eq!(
            overlapped.load(std::sync::atomic::Ordering::SeqCst),
            !same,
            "same={same}"
        );
    }
}
