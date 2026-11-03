//! The drive from its wake: one run per channel, each granted front handed on once in queue order,
//! a front that moved after its grant never reserved, and a held front delivered on its next wake.
use super::*;
use crate::config::TestRuntimeRootGuard;
use crate::services::claude_tui::owner_input;
use crate::services::discord::queue_io::input_owner::admission::{AdmissionTicket, admit};
use crate::services::discord::queue_io::input_owner::effect::{
    Completion, Leaf, MODAL, PreparedPayload,
};
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
                if let Completion::NotSent(reason) | Completion::Held(reason) = &completion {
                    owner_input::vetoed(reason);
                }
                Ok(completion)
            });
            let clock = Arc::new(|| 10_000);
            let prepared = prepared(channel, &text);
            let claim = claim_turn(&mailbox, front.1.message_id, &front).await?;
            let claim = claim.ok_or("NotSent(\"front_not_granted\")")?;
            match pump::submit_front(&mailbox, Some(claim), prepared, clock, leaf).await? {
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
        settled: None,
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

/// A front cancelled or cleared after its grant is never reserved and the drive goes on; a cancel
/// behind the front keeps its grant, so it is delivered on its one handoff.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_front_that_moved_after_its_grant_is_never_reserved() {
    let _root = TestRuntimeRootGuard::new();
    let registry = ChannelMailboxRegistry::default();
    for (channel, how) in [
        (7_272_702, "cancel"),
        (7_272_703, "clear"),
        (7_272_708, "tail"),
    ] {
        let mailbox = owner(&registry, channel, 2).await;
        let ran = Arc::new(Mutex::new(Vec::new()));
        let handed = Arc::new(AtomicUsize::new(0));
        let (m, r, h) = (mailbox.clone(), ran.clone(), handed.clone());
        let before = move |front: &Front| {
            h.fetch_add(1, Ordering::SeqCst);
            let (m, first) = (m.clone(), front.1.text == "input 0");
            let id = match how {
                "tail" => original(channel, 1).message_id,
                _ => front.1.message_id,
            };
            let id = poise::serenity_prelude::MessageId::new(id);
            Box::pin(async move {
                let ctx = crate::services::turn_orchestrator::QueuePersistenceContext::new(
                    &crate::services::provider::ProviderKind::Claude,
                    "drive",
                    None,
                );
                match (first, how) {
                    (true, "cancel" | "tail") => {
                        drop(m.cancel_queued_primary_message(id, ctx).await)
                    }
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
            settled: None,
        });
        wake(drive);
        let after = settled(&mailbox).await;
        assert!(after["head_injection"].is_null(), "{how}: {after}");
        assert_eq!(after["retained_inputs"], json!([]), "{how}: {after}");
        let expected: &[&str] = match how {
            "cancel" => &["input 1"],
            "tail" => &["input 0"],
            _ => &[],
        };
        assert_eq!(*ran.lock().unwrap(), expected, "{how}");
        let handoffs = if how == "cancel" { 2 } else { 1 };
        assert_eq!(
            handed.load(Ordering::SeqCst),
            handoffs,
            "{how}: one handoff per front"
        );
    }
}

/// A hold armed while the cancelled front's watch is mid-read outlives that watch's end: the pane's
/// recovery alone hands the new front again, and it is submitted once.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_new_hold_outlives_the_watch_of_the_front_it_replaced() {
    use crate::services::claude_tui::busy_inject::Screen;
    use std::sync::{atomic::AtomicBool, mpsc};
    let _root = TestRuntimeRootGuard::new();
    let registry = ChannelMailboxRegistry::default();
    let channel = 7_272_709;
    let mailbox = owner(&registry, channel, 2).await;
    let (ready, handed) = (
        Arc::new(AtomicBool::new(false)),
        Arc::new(Mutex::new(Vec::new())),
    );
    let ((entered, on_entered), (release, on_release)) = (mpsc::channel(), mpsc::channel::<()>());
    // Only the first front's probe stops in its first read until this test releases it.
    let gate = Arc::new(Mutex::new(Some((entered, on_release))));
    let (r, h) = (ready.clone(), handed.clone());
    let answer = move |frame: &str| {
        h.lock().unwrap().push(frame.to_string());
        if r.load(Ordering::SeqCst) {
            return Completion::Submitted;
        }
        let (r, mut gate) = (r.clone(), gate.lock().unwrap().take());
        super::super::held_watch::arm(
            channel,
            Box::new(move || {
                if let Some((entered, release)) = gate.take() {
                    entered.send(()).unwrap();
                    release.recv().unwrap();
                }
                match r.load(Ordering::SeqCst) {
                    true => Screen::Ready,
                    false => Screen::Blocked,
                }
            }),
        );
        Completion::Held("CopyMode".into())
    };
    let before = |_: &Front| Box::pin(async {}) as Pin<Box<dyn Future<Output = ()> + Send>>;
    // A pass ends after the drive handed its held front to the watch.
    let passes = Arc::new(AtomicUsize::new(0));
    let passes_seen = passes.clone();
    let drive = Arc::new(Drive {
        mailbox: mailbox.clone(),
        channel,
        handoff: handoff(mailbox.clone(), channel, before, answer),
        settled: Some(Arc::new(move || {
            passes.fetch_add(1, Ordering::SeqCst);
            Box::pin(async {}) as Pin<Box<dyn Future<Output = ()> + Send>>
        })),
    });
    wake(drive.clone());
    let wait = Duration::from_secs(20);
    tokio::task::spawn_blocking(move || on_entered.recv_timeout(wait))
        .await
        .unwrap()
        .expect("the first front's watch reads its pane");
    let ctx = crate::services::turn_orchestrator::QueuePersistenceContext::new(
        &crate::services::provider::ProviderKind::Claude,
        "drive",
        None,
    );
    let first = poise::serenity_prelude::MessageId::new(original(channel, 0).message_id);
    drop(mailbox.cancel_queued_primary_message(first, ctx).await);
    wake(drive);
    for _ in 0..2_000 {
        if passes_seen.load(Ordering::SeqCst) == 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(
        *handed.lock().unwrap(),
        ["input 0", "input 1"],
        "the new front is held"
    );
    release.send(()).unwrap();
    ready.store(true, Ordering::SeqCst);
    for _ in 0..2_000 {
        if snapshot(&mailbox).await["items"] == json!([]) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let after = snapshot(&mailbox).await;
    assert_eq!(after["items"], json!([]), "the new hold was woken: {after}");
    assert_eq!(*handed.lock().unwrap(), ["input 0", "input 1", "input 1"]);
}

/// A front refused before any key for a reason a later look may clear (a screen where Enter may not
/// submit, a missing pane, a held lock) stays unretained and waits for its next wake, never time.
#[tokio::test(start_paused = true)]
async fn a_held_front_waits_for_its_next_wake_and_is_then_delivered_once() {
    let _root = TestRuntimeRootGuard::new();
    let registry = ChannelMailboxRegistry::default();
    for (channel, reason) in [
        (7_272_704, MODAL),
        (7_272_714, "ScreenUnread"),
        (7_272_724, "CopyMode"),
        (7_272_734, "PaneUnavailable"),
        (7_272_744, "LockContended"),
    ] {
        let mailbox = owner(&registry, channel, 1).await;
        let (calls, handed) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
        let held = Arc::new(Mutex::new(None));
        let c = calls.clone();
        let answer = move |_: &str| match c.fetch_add(1, Ordering::SeqCst) {
            0 => Completion::Held(reason.into()),
            _ => Completion::Submitted,
        };
        let (n, h, m) = (handed.clone(), held.clone(), mailbox.clone());
        let before = move |_: &Front| {
            let (n, h, m) = (n.clone(), h.clone(), m.clone());
            Box::pin(async move {
                if n.fetch_add(1, Ordering::SeqCst) == 1 {
                    *h.lock().unwrap() = Some(snapshot(&m).await);
                }
            }) as Pin<Box<dyn Future<Output = ()> + Send>>
        };
        let drive = Arc::new(Drive {
            mailbox: mailbox.clone(),
            channel,
            handoff: handoff(mailbox.clone(), channel, before, answer),
            settled: None,
        });
        wake(drive.clone());
        while calls.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
            std::thread::sleep(Duration::from_millis(2));
        }
        tokio::time::sleep(Duration::from_secs(25)).await;
        assert_eq!(handed.load(Ordering::SeqCst), 1, "{reason}: time alone");
        wake(drive);
        for _ in 0..2_000 {
            if snapshot(&mailbox).await["items"] == json!([]) {
                break;
            }
            tokio::task::yield_now().await;
            std::thread::sleep(Duration::from_millis(2));
        }
        let after = snapshot(&mailbox).await;
        assert_eq!(handed.load(Ordering::SeqCst), 2, "{reason}: {after}");
        assert_eq!(calls.load(Ordering::SeqCst), 2, "{reason}: {after}");
        assert_eq!(after["items"], json!([]), "{reason} delivered: {after}");
        assert_eq!(after["retained_inputs"], json!([]), "{reason}: {after}");
        let between = held.lock().unwrap().clone().unwrap();
        assert_eq!(
            between["retained_inputs"],
            json!([]),
            "{reason} held, not retained: {between}"
        );
        assert_eq!(
            between["items"][0]["text"], "input 0",
            "{reason}: {between}"
        );
    }
}

/// A turn's claim gives one reserve: a spent grant gives no second claim, and a claimed front a
/// cancel, clear or purge took before the reserve sends nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_claimed_front_the_queue_took_reserves_nothing() {
    use crate::services::turn_orchestrator::checked_purge::ExpectedQueue;
    let _root = TestRuntimeRootGuard::new();
    let registry = ChannelMailboxRegistry::default();
    for (channel, how) in [
        (7_272_705, "cancel"),
        (7_272_706, "clear"),
        (7_272_707, "purge"),
    ] {
        let mailbox = owner(&registry, channel, 1).await;
        let front = (mailbox.v2_request(|slot| {
            let owner = slot.as_mut().ok_or("no owner")?;
            let front = owner.front().ok_or("no front")?;
            owner.grant_front(&front).map(|()| front)
        }))
        .await
        .unwrap();
        let claim = claim_turn(&mailbox, front.1.message_id, &front).await;
        let claim = claim.unwrap().expect("the granted front's claim");
        let again = claim_turn(&mailbox, front.1.message_id, &front).await;
        assert!(again.unwrap().is_none(), "one claim per grant");
        let ctx = crate::services::turn_orchestrator::QueuePersistenceContext::new(
            &crate::services::provider::ProviderKind::Claude,
            "input-owner-drive",
            None,
        );
        let id = poise::serenity_prelude::MessageId::new(front.1.message_id);
        match how {
            "cancel" => assert!(
                mailbox
                    .cancel_queued_primary_message(id, ctx)
                    .await
                    .removed
                    .is_some()
            ),
            "clear" => assert!(mailbox.clear(ctx).await.persistence.failed.is_empty()),
            _ => {
                let fingerprint =
                    mailbox.v2_request(|slot| slot.as_ref().ok_or("no owner")?.fingerprint());
                let expected = ExpectedQueue::of(&mailbox.snapshot().await)
                    .with_v2(fingerprint.await.unwrap());
                let purged = mailbox
                    .try_purge_queue_if_unchanged(ctx, expected)
                    .await
                    .unwrap();
                assert!(purged.failed.is_none(), "{:?}", purged.failed);
            }
        }
        let sent = Arc::new(AtomicUsize::new(0));
        let counted = sent.clone();
        let leaf: Leaf = Box::new(move |_| {
            counted.fetch_add(1, Ordering::SeqCst);
            Ok(Completion::Submitted)
        });
        let clock = Arc::new(|| 10_000);
        let result = pump::submit_front(&mailbox, Some(claim), prepared(channel, "x"), clock, leaf);
        assert_eq!(
            result.await,
            Err("NotSent(\"front_not_granted\")".into()),
            "{how}"
        );
        assert_eq!(sent.load(Ordering::SeqCst), 0, "{how}");
    }
}

/// Two migrated inputs with the same body and no ticket: one wake hands each on once, since a
/// delivered front is progress whatever the next one looks like.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn identical_migrated_inputs_are_each_handed_on_once() {
    let _root = TestRuntimeRootGuard::new();
    let registry = ChannelMailboxRegistry::default();
    let channel = 7_272_709;
    let dir = crate::services::discord::runtime_store::discord_pending_queue_root()
        .unwrap()
        .join("claude/input-owner-drive");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(format!("{channel}.json"));
    let twin = original(channel, 0);
    std::fs::write(&path, serde_json::to_string(&[&twin, &twin]).unwrap()).unwrap();
    let mailbox = registry.handle(poise::serenity_prelude::ChannelId::new(channel));
    mailbox.v2_adopt(path, "input-owner".into()).await.unwrap();
    let handed = Arc::new(AtomicUsize::new(0));
    let h = handed.clone();
    let answer = move |_: &str| {
        h.fetch_add(1, Ordering::SeqCst);
        Completion::Submitted
    };
    let none = |_: &Front| Box::pin(async {}) as Pin<Box<dyn Future<Output = ()> + Send>>;
    let drive = Arc::new(Drive {
        mailbox: mailbox.clone(),
        channel,
        handoff: handoff(mailbox.clone(), channel, none, answer),
        settled: None,
    });
    wake(drive);
    let after = settled(&mailbox).await;
    assert!(after["items"].as_array().unwrap().is_empty(), "{after}");
    assert_eq!(handed.load(Ordering::SeqCst), 2, "{after}");
}

/// A reserve the owner cannot commit is no progress: the wake hands the front on once and stops
/// with it still queued, and a wake after the store recovers delivers it once.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_reserve_that_did_not_commit_stops_the_drive_until_the_next_wake() {
    let _root = TestRuntimeRootGuard::new();
    let registry = ChannelMailboxRegistry::default();
    let channel = 7_272_711;
    let mailbox = owner(&registry, channel, 1).await;
    let path = crate::services::discord::runtime_store::discord_pending_queue_root()
        .unwrap()
        .join(format!("claude/input-owner-drive/{channel}.json"));
    let bytes = std::fs::read(&path).unwrap();
    // A file changed outside the owner refuses every commit until it is back.
    std::fs::write(&path, [&bytes[..], b"\n"].concat()).unwrap();
    let (handed, sent) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
    let (h, n) = (handed.clone(), sent.clone());
    let before = move |_: &Front| {
        h.fetch_add(1, Ordering::SeqCst);
        Box::pin(async {}) as Pin<Box<dyn Future<Output = ()> + Send>>
    };
    let answer = move |_: &str| {
        n.fetch_add(1, Ordering::SeqCst);
        Completion::Submitted
    };
    let drive = Arc::new(Drive {
        mailbox: mailbox.clone(),
        channel,
        handoff: handoff(mailbox.clone(), channel, before, answer),
        settled: None,
    });
    wake(drive.clone());
    tokio::time::sleep(Duration::from_millis(300)).await;
    let counts = (handed.load(Ordering::SeqCst), sent.load(Ordering::SeqCst));
    assert_eq!(counts, (1, 0), "one handoff for the wake, no content");
    std::fs::write(&path, &bytes).unwrap();
    assert_eq!(
        snapshot(&mailbox).await["items"].as_array().unwrap().len(),
        1
    );
    wake(drive);
    let after = settled(&mailbox).await;
    assert!(after["items"].as_array().unwrap().is_empty(), "{after}");
    assert_eq!(sent.load(Ordering::SeqCst), 1);
}

/// A claim reserves only on the owner that issued it and only while it is that owner's current
/// claim: another owner's claim on an equal front, or an earlier one, sends nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_claim_reserves_only_as_its_owners_current_claim() {
    let _root = TestRuntimeRootGuard::new();
    let registry = ChannelMailboxRegistry::default();
    let (a, b) = (
        owner(&registry, 7_272_712, 0).await,
        owner(&registry, 7_272_713, 0).await,
    );
    let (item, ticket) = (original(7_272_712, 0), AdmissionTicket::new());
    for mailbox in [&a, &b] {
        let admitted = admit(mailbox, &item, ticket.clone()).await;
        assert!(matches!(
            admitted,
            super::super::admission::Admission::Accepted(_)
        ));
    }
    let claim = |mailbox: ChannelMailboxHandle| async move {
        let granted = mailbox.v2_request(|slot| {
            let owner = slot.as_mut().ok_or("no owner")?;
            let front = owner.front().ok_or("no front")?;
            owner.grant_front(&front).map(|()| front)
        });
        let front = granted.await.unwrap();
        claim_turn(&mailbox, front.1.message_id, &front)
            .await
            .unwrap()
            .unwrap()
    };
    let sent = Arc::new(AtomicUsize::new(0));
    let submit = |mailbox: ChannelMailboxHandle, claim: FrontClaim| {
        let counted = sent.clone();
        let leaf: Leaf = Box::new(move |_| {
            counted.fetch_add(1, Ordering::SeqCst);
            Ok(Completion::Submitted)
        });
        let prepared = prepared(7_272_713, "x");
        async move {
            pump::submit_front(&mailbox, Some(claim), prepared, Arc::new(|| 10_000), leaf).await
        }
    };
    let (of_a, earlier) = (claim(a.clone()).await, claim(b.clone()).await);
    let current = claim(b.clone()).await;
    let refused = Err("NotSent(\"front_not_granted\")".to_string());
    assert_eq!(
        submit(b.clone(), of_a).await,
        refused,
        "another owner's claim"
    );
    assert_eq!(
        submit(b.clone(), earlier).await,
        refused,
        "an earlier claim"
    );
    assert_eq!(sent.load(Ordering::SeqCst), 0);
    assert_eq!(submit(b.clone(), current).await, Ok(Completion::Submitted));
    assert_eq!(sent.load(Ordering::SeqCst), 1);
}

/// Admits `id` under its own ticket with a caller context naming it.
async fn admit_named(handle: &ChannelMailboxHandle, channel: u64, id: u64) {
    let context = json!({"caller": id});
    let admitted =
        handle.v2_admit_ticket_with_context(original(channel, id), format!("t{id}"), context);
    assert!(admitted.await.is_ok(), "{id}");
}

/// Each queued original's ticket standing and caller context, as its callers read them.
async fn standing(handle: &ChannelMailboxHandle, channel: u64) -> Vec<String> {
    let queued = snapshot(handle).await["items"].as_array().unwrap().clone();
    let mut seen = Vec::new();
    for item in queued {
        let id = item["message_id"].as_u64().unwrap() - original(channel, 0).message_id;
        let bind = handle
            .v2_exact_turn(original(channel, id), format!("t{id}"))
            .await
            .unwrap();
        let context = handle.v2_admission_context(original(channel, id), format!("t{id}"));
        seen.push(format!(
            "{id}:{bind:?}:{}",
            context.await.unwrap().unwrap_or_default()
        ));
    }
    seen
}

/// A requeue beside a live head keeps its ticket: once that head is held under a modal, it and the
/// requeued originals each keep their slot and caller context, across a re-adoption too.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_requeue_beside_a_live_head_keeps_every_ticket_through_its_hold() {
    let _root = TestRuntimeRootGuard::new();
    let channel = 7_272_791;
    let dir = crate::services::discord::runtime_store::discord_pending_queue_root().unwrap();
    let file = dir.join(format!("claude/input-owner-drive/{channel}.json"));
    let adopt = || async {
        let registry = ChannelMailboxRegistry::default();
        let handle = registry.handle(poise::serenity_prelude::ChannelId::new(channel));
        handle
            .v2_adopt(file.clone(), "input-owner".into())
            .await
            .unwrap();
        (registry, handle)
    };
    // An older binary retained the unit of 1 and 2 as proven unsent before any ask went out.
    let (registry, handle) = adopt().await;
    for id in [1, 2] {
        admit_named(&handle, channel, id).await;
    }
    let reserved = handle.v2_request(|slot| {
        let owner = slot.as_mut().ok_or("no owner")?;
        let attempt = serde_json::from_value(json!({"nonce":"old","provider":"claude",
            "target":{"host":"tmux","owner":"old-pane","root":"/old","execution_nonce":"e"},
            "source":{"kind":"pre_source","expected_session_id":null,"expected_transcript":null,
                "binding_watermark":null}}))
        .map_err(|e| e.to_string())?;
        owner.reserve_unit(attempt, |i| Some((String::new(), i.text.clone())))?;
        owner.json()
    });
    let mut state = reserved.await.unwrap();
    drop((handle, registry));
    let head = state["head_injection"].take();
    state["items"] = json!([]);
    state["retained_inputs"] = json!([{"input":head,"disposition":"known_not_sent",
        "reason":{"kind":"rejected","reason":"known_not_sent: vetoed: NotBusy"},
        "notice":{"kind":"retained_retry_required","enqueued":false}}]);
    std::fs::write(&file, serde_json::to_vec_pretty(&state).unwrap()).unwrap();
    let (registry, mailbox) = adopt().await;
    for id in [0, 3] {
        admit_named(&mailbox, channel, id).await;
    }
    // The live head 0 meets a requeue during its attempt, then a modal that holds it.
    let requeued = Arc::new(AtomicUsize::new(0));
    let (r, m) = (requeued.clone(), mailbox.clone());
    let answer = move |_: &str| {
        let moved = m.v2_request(|s| {
            s.as_mut()
                .unwrap()
                .requeue_known_not_sent()
                .map(|v| v.len())
        });
        r.store(
            tokio::runtime::Handle::current().block_on(moved).unwrap(),
            Ordering::SeqCst,
        );
        Completion::Held(MODAL.into())
    };
    let before = |_: &Front| Box::pin(async {}) as Pin<Box<dyn Future<Output = ()> + Send>>;
    let handoff = handoff(mailbox.clone(), channel, before, answer);
    let front = grant(
        &Drive {
            mailbox: mailbox.clone(),
            channel,
            handoff: handoff.clone(),
            settled: None,
        },
        false,
    )
    .await
    .unwrap();
    assert_eq!(handoff(front).await, Err("NotSent(\"Modal\")".into()));
    assert_eq!(requeued.load(Ordering::SeqCst), 1);
    let expected: Vec<String> = [0, 1, 2, 3]
        .iter()
        .map(|id| format!("{id}:NotArmed:{}", json!({"caller": id})))
        .collect();
    assert_eq!(standing(&mailbox, channel).await, expected);
    drop((mailbox, registry));
    let (_registry, mailbox) = adopt().await;
    assert_eq!(
        standing(&mailbox, channel).await,
        expected,
        "after re-adoption"
    );
}
