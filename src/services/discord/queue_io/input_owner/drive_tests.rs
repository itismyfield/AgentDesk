//! The drive from its wake: one run per channel, each granted front handed on once in queue order,
//! a front that moved after its grant never reserved, and a modal front delivered after release.
use super::*;
use crate::config::TestRuntimeRootGuard;
use crate::services::claude_tui::owner_input;
use crate::services::discord::queue_io::input_owner::admission::{AdmissionTicket, admit};
use crate::services::discord::queue_io::input_owner::effect::{Leaf, PreparedPayload};
use crate::services::discord::queue_io::input_owner::pump;
use crate::services::turn_orchestrator::ChannelMailboxRegistry;
use serde_json::json;
use std::sync::atomic::AtomicUsize;

fn original(channel: u64, n: u64) -> PendingQueueItem {
    let id = 8_000_000_000_000_006_000 + channel % 100 * 10 + n;
    serde_json::from_value(json!({"author_id":7,"message_id":id,"channel_id":channel,
        "source_message_ids":[id],"queued_generation":1,"text":format!("input {n}")}))
    .unwrap()
}

/// An adopted owner on `channel` with `count` admitted originals.
async fn owner(
    registry: &ChannelMailboxRegistry,
    channel: u64,
    count: u64,
) -> ChannelMailboxHandle {
    let dir = crate::services::discord::runtime_store::discord_pending_queue_root()
        .unwrap()
        .join("claude/input-owner-drive");
    std::fs::create_dir_all(&dir).unwrap();
    let handle = registry.handle(poise::serenity_prelude::ChannelId::new(channel));
    let path = dir.join(format!("{channel}.json"));
    handle.v2_adopt(path, "input-owner".into()).await.unwrap();
    for n in 0..count {
        let admitted = admit(&handle, &original(channel, n), AdmissionTicket::new()).await;
        assert!(matches!(
            admitted,
            super::super::admission::Admission::Accepted(_)
        ));
    }
    handle
}

fn prepared(channel: u64, text: &str) -> PreparedPayload {
    let nonce = uuid::Uuid::new_v4().simple().to_string();
    PreparedPayload {
        frame: text.into(),
        sources: vec![],
        attempt: json!({"nonce":nonce,"provider":"claude",
            "target":{"host":"herdr","owner":format!("drive-{channel}"),"root":"/drive",
                "execution_nonce":nonce},
            "source":{"kind":"pre_source","expected_session_id":null,
                "expected_transcript":null,"binding_watermark":null}}),
    }
}

/// Hands each front to the pump with a leaf that answers from `answer`, after `before` ran.
fn handoff(
    mailbox: ChannelMailboxHandle,
    channel: u64,
    before: impl Fn(&Front) -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync + 'static,
    answer: impl Fn(&str) -> Completion + Send + Sync + 'static,
) -> Handoff {
    let (before, answer) = (Arc::new(before), Arc::new(answer));
    Arc::new(move |front: Front| {
        let (mailbox, before, answer) = (mailbox.clone(), before.clone(), answer.clone());
        Box::pin(async move {
            before(&front).await;
            let text = front.1.text.clone();
            let leaf: Leaf = Box::new(move |frame| {
                let completion = answer(&frame);
                if let Completion::NotSent(reason) = &completion {
                    owner_input::vetoed(reason);
                }
                Ok(completion)
            });
            let clock = Arc::new(|| 10_000);
            let prepared = prepared(channel, &text);
            match pump::submit_front(&mailbox, Some(front), prepared, clock, leaf).await? {
                Completion::Submitted => Ok(()),
                other => Err(format!("{other:?}")),
            }
        })
    })
}

async fn snapshot(handle: &ChannelMailboxHandle) -> serde_json::Value {
    (handle.v2_request(|s| s.as_ref().unwrap().json()).await).unwrap()
}

async fn settled(handle: &ChannelMailboxHandle) -> serde_json::Value {
    for _ in 0..200 {
        let now = snapshot(handle).await;
        if now["items"].as_array().is_some_and(Vec::is_empty) && now["head_injection"].is_null() {
            return now;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    snapshot(handle).await
}

/// Three wakes at once start one run: each front is handed on once, in queue order, never two at
/// a time on the channel.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn one_drive_per_channel_hands_each_front_on_once_in_order() {
    let _root = TestRuntimeRootGuard::new();
    let registry = ChannelMailboxRegistry::default();
    let channel = 7_272_701;
    let mailbox = owner(&registry, channel, 3).await;
    let (busy, peak) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
    let seen = Arc::new(Mutex::new(Vec::new()));
    let (b, p, s) = (busy.clone(), peak.clone(), seen.clone());
    let before = move |front: &Front| {
        let (b, p, s) = (b.clone(), p.clone(), s.clone());
        let text = front.1.text.clone();
        Box::pin(async move {
            let now = b.fetch_add(1, Ordering::SeqCst) + 1;
            p.fetch_max(now, Ordering::SeqCst);
            s.lock().unwrap().push(text);
            tokio::time::sleep(Duration::from_millis(30)).await;
            b.fetch_sub(1, Ordering::SeqCst);
        }) as Pin<Box<dyn Future<Output = ()> + Send>>
    };
    let drive = Arc::new(Drive {
        mailbox: mailbox.clone(),
        channel,
        handoff: handoff(mailbox.clone(), channel, before, |_| Completion::Submitted),
        recheck: Duration::from_millis(20),
    });
    for _ in 0..3 {
        let drive = drive.clone();
        tokio::spawn(async move { wake(drive) });
    }
    let after = settled(&mailbox).await;
    assert!(after["items"].as_array().unwrap().is_empty(), "{after}");
    assert_eq!(*seen.lock().unwrap(), ["input 0", "input 1", "input 2"]);
    assert_eq!(
        peak.load(Ordering::SeqCst),
        1,
        "never two handoffs at a time"
    );
}

/// A front cancelled after its grant and before its claim is never reserved, and the drive goes
/// on with the front that took its place; a clear before the claim leaves nothing reserved.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_front_that_moved_after_its_grant_is_never_reserved() {
    let _root = TestRuntimeRootGuard::new();
    let registry = ChannelMailboxRegistry::default();
    for (channel, how) in [(7_272_702, "cancel"), (7_272_703, "clear")] {
        let mailbox = owner(&registry, channel, 2).await;
        let ran = Arc::new(Mutex::new(Vec::new()));
        let (m, r) = (mailbox.clone(), ran.clone());
        let before = move |front: &Front| {
            let (m, first) = (m.clone(), front.1.text == "input 0");
            let id = poise::serenity_prelude::MessageId::new(front.1.message_id);
            Box::pin(async move {
                let ctx = crate::services::turn_orchestrator::QueuePersistenceContext::new(
                    &crate::services::provider::ProviderKind::Claude,
                    "drive",
                    None,
                );
                match (first, how) {
                    (true, "cancel") => drop(m.cancel_queued_primary_message(id, ctx).await),
                    (true, _) => drop(m.clear(ctx).await),
                    _ => {}
                }
            }) as Pin<Box<dyn Future<Output = ()> + Send>>
        };
        let answer = move |frame: &str| {
            r.lock().unwrap().push(frame.to_string());
            Completion::Submitted
        };
        let drive = Arc::new(Drive {
            mailbox: mailbox.clone(),
            channel,
            handoff: handoff(mailbox.clone(), channel, before, answer),
            recheck: Duration::from_millis(20),
        });
        wake(drive);
        let after = settled(&mailbox).await;
        assert!(after["head_injection"].is_null(), "{how}: {after}");
        assert_eq!(after["retained_inputs"], json!([]), "{how}: {after}");
        let expected: &[&str] = if how == "cancel" { &["input 1"] } else { &[] };
        assert_eq!(*ran.lock().unwrap(), expected, "{how}");
    }
}

/// A modal refusal holds the front in place, unretained and owing no notice, and the drive's own
/// recheck delivers it once the modal is gone, with no new input or wake.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_modal_front_is_held_and_delivered_after_release_without_new_input() {
    let _root = TestRuntimeRootGuard::new();
    let registry = ChannelMailboxRegistry::default();
    let channel = 7_272_704;
    let mailbox = owner(&registry, channel, 1).await;
    let calls = Arc::new(AtomicUsize::new(0));
    let held = Arc::new(Mutex::new(None));
    let (c, h, m) = (calls.clone(), held.clone(), mailbox.clone());
    let answer = move |_: &str| match c.fetch_add(1, Ordering::SeqCst) {
        0 => Completion::NotSent(MODAL.into()),
        _ => {
            let state = tokio::runtime::Handle::current().block_on(snapshot(&m));
            *h.lock().unwrap() = Some(state);
            Completion::Submitted
        }
    };
    let none = |_: &Front| Box::pin(async {}) as Pin<Box<dyn Future<Output = ()> + Send>>;
    let drive = Arc::new(Drive {
        mailbox: mailbox.clone(),
        channel,
        handoff: handoff(mailbox.clone(), channel, none, answer),
        recheck: Duration::from_millis(50),
    });
    wake(drive);
    let after = settled(&mailbox).await;
    assert_eq!(calls.load(Ordering::SeqCst), 2, "{after}");
    assert!(
        after["items"].as_array().unwrap().is_empty(),
        "delivered: {after}"
    );
    assert_eq!(
        after["retained_inputs"],
        json!([]),
        "never retained: {after}"
    );
    let between = held.lock().unwrap().clone().unwrap();
    assert_eq!(
        between["retained_inputs"],
        json!([]),
        "held, not retained: {between}"
    );
    assert_eq!(between["items"][0]["text"], "input 0", "{between}");
}
