//! A request's own claim of the Decision source is refused at every entry that takes a source,
//! and an approved event settles once: after its write, after a failure and after a restart.
use super::*;
use crate::db::auto_queue::test_support::TestPostgresDb;
use crate::db::decisions::EventKind;
use crate::db::decisions::test_support::{insert_event, row as pg_row};
use poise::serenity_prelude::ChannelId;
use serde_json::json;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;

#[test]
fn decision_source_named_by_a_request_is_self_declared() {
    let declared = [
        (Some("decision"), None),
        (Some(" Decision "), None),
        (None, Some(json!({"human_input": {"source": "decision"}}))),
        (
            Some("external"),
            Some(json!({"human_input": {"origin_id": "decision:anything"}})),
        ),
    ];
    for (source, metadata) in declared {
        assert!(self_declared(source, metadata.as_ref()), "{source:?}");
        assert_eq!(
            reject_handoff_source(source, metadata.as_ref()),
            Err(SOURCE_REFUSED)
        );
    }
    let plain = [
        (None, None),
        (Some("imessage"), None),
        (Some("decisions"), None),
        (
            Some("external"),
            Some(json!({"human_input": {"origin_id": "guid-1"}})),
        ),
    ];
    for (source, metadata) in plain {
        assert_eq!(reject_handoff_source(source, metadata.as_ref()), Ok(()));
    }
}

/// The service entry refuses before it reads the agent, so a caller that skips the HTTP route
/// reaches no backend either: the lazy pool below would fail any query.
#[tokio::test]
async fn decision_handoff_service_entry_refuses_before_any_backend() {
    use crate::services::discord::agent_handoff::{
        AgentHandoffChannelKind, start_agent_handoff_turn,
    };
    let registry = crate::services::discord::health::HealthRegistry::new();
    let pool = sqlx::postgres::PgPoolOptions::new()
        .connect_lazy("postgres://decision-handoff@127.0.0.1:1/none")
        .unwrap();
    let cases = [
        (Some("decision".to_string()), None),
        (None, Some(json!({"human_input": {"source": "decision"}}))),
    ];
    for (source, metadata) in cases {
        let started = start_agent_handoff_turn(
            &registry,
            &pool,
            "from-agent",
            "to-agent",
            "prompt",
            AgentHandoffChannelKind::Cc,
            true,
            None,
            source,
            metadata,
        )
        .await;
        let Err(refused) = started else {
            panic!("a decision handoff started a turn");
        };
        assert_eq!(refused.status(), axum::http::StatusCode::BAD_REQUEST);
        assert_eq!(refused.body()["error"], SOURCE_REFUSED);
    }
}

const CHANNEL: u64 = 7_821_101;
const TEXT: &str = "[사용자 결정 ref=r1] 질문 → 선택: A";

struct Rig {
    db: TestPostgresDb,
    pool: PgPool,
    file: std::path::PathBuf,
    mailbox: ChannelMailboxHandle,
    _root: crate::config::TestRuntimeRootGuard,
}

impl Rig {
    /// An isolated PG and a channel whose input owner holds an empty queue file.
    async fn new() -> Self {
        let _root = crate::config::TestRuntimeRootGuard::new();
        let db = TestPostgresDb::create().await;
        let pool = db.connect_and_migrate().await;
        let root = crate::services::discord::runtime_store::discord_pending_queue_root().unwrap();
        let file = root
            .join("claude")
            .join("decision")
            .join(format!("{CHANNEL}.json"));
        let mailbox = Self::adopt(&file).await;
        Self {
            db,
            pool,
            file,
            mailbox,
            _root,
        }
    }

    /// A new owner over the same queue file, as a restarted process adopts it.
    async fn adopt(file: &std::path::Path) -> ChannelMailboxHandle {
        let registry = ChannelMailboxRegistry::default();
        let mailbox = registry.handle(ChannelId::new(CHANNEL));
        let adopted = mailbox.v2_adopt(file.to_path_buf(), "decision".into());
        adopted.await.unwrap();
        mailbox
    }

    /// An approved event in `state` and, when `queued`, its original admitted under its ticket.
    async fn event(&self, state: &str, queued: bool) -> Unsettled {
        let event = (EventKind::Selection, CHANNEL, 7);
        let event_id = insert_event(&self.pool, event, TEXT, state).await;
        let origin = format!("decision:{event_id}");
        if queued {
            let metadata = json!({"human_input": {"origin_id": origin}});
            let (message, ticket) =
                InputOrigin::decision_identity(CHANNEL, Some(SOURCE), Some(&metadata)).unwrap();
            let original = json!({"author_id":7,"message_id":message,"channel_id":CHANNEL,
                "source_message_ids":[message],"queued_generation":5,"text":TEXT});
            let original = serde_json::from_value(original).unwrap();
            // The first attempt stored this ticket when it reserved the event.
            sqlx::query(
                "UPDATE decision_delivery_attempts SET workflow_ticket = $2 WHERE event_id = $1",
            )
            .bind(event_id)
            .bind(&ticket)
            .execute(&self.pool)
            .await
            .unwrap();
            self.mailbox
                .v2_admit_ticket(original, ticket)
                .await
                .unwrap();
        }
        Unsettled {
            event_id,
            origin,
            agent_id: "decision-agent".into(),
            channel_id: CHANNEL,
            state: AttemptState::parse(state).unwrap(),
            stale: false,
        }
    }

    /// Another user's ordinary input behind whatever the queue holds.
    async fn other_input(&self, n: u64) -> u64 {
        let message = 1_490_000_000_000_008_000 + n;
        let original = json!({"author_id":9,"message_id":message,"channel_id":CHANNEL,
            "source_message_ids":[message],"queued_generation":5,"text":format!("other {n}")});
        let original = serde_json::from_value(original).unwrap();
        let ticket = format!("other-{n}");
        self.mailbox
            .v2_admit_ticket(original, ticket)
            .await
            .unwrap();
        message
    }

    async fn state(&self, row: &Unsettled) -> (String, Option<String>) {
        let read = pg_row(&self.pool, row.event_id).await;
        (read.0, read.4)
    }

    async fn queue(&self) -> serde_json::Value {
        let read = self
            .mailbox
            .v2_request(|slot| slot.as_ref().ok_or("no owner")?.json());
        read.await.unwrap()
    }

    async fn queued_ids(&self) -> Vec<u64> {
        let queue = self.queue().await;
        let items = queue["items"].as_array().unwrap().iter();
        items
            .map(|item| item["message_id"].as_u64().unwrap())
            .collect()
    }

    #[cfg(unix)]
    async fn front(&self) -> Front {
        let read = self
            .mailbox
            .v2_request(|slot| Ok(slot.as_ref().and_then(|owner| owner.front())));
        read.await.unwrap().expect("a front")
    }

    /// Reserves and arms the queue front; `true` lets its confirmation window lapse unconfirmed.
    async fn attempt_front(&self, nonce: &str, lapse: bool) {
        let attempt = json!({"nonce":nonce,"provider":"claude",
            "target":{"host":"tmux","owner":"decision-pane","root":"/test","execution_nonce":"e"},
            "source":{"kind":"pre_source","expected_session_id":null,"expected_transcript":null,
                "binding_watermark":null}});
        let armed = self.mailbox.v2_request(move |slot| {
            let owner = slot.as_mut().ok_or("no owner")?;
            let attempt = serde_json::from_value(attempt).map_err(|e| e.to_string())?;
            let key =
                owner.reserve_unit(attempt, |item| Some((String::new(), item.text.clone())))?;
            drop(owner.arm(&key, 1)?);
            match lapse {
                true => owner.tick(40_000),
                false => Ok(()),
            }
        });
        armed.await.unwrap();
    }

    async fn notices(&self) -> i64 {
        sqlx::query_scalar("SELECT COUNT(*) FROM message_outbox WHERE target = $1")
            .bind(format!("channel:{CHANNEL}"))
            .fetch_one(&self.pool)
            .await
            .unwrap()
    }

    async fn receipt(&self, row: &Unsettled, outcome: crate::db::turns::TerminalOutcome) {
        let ids = vec![row.origin.clone()];
        let done = crate::db::turns::TurnReceipt {
            turn_id: &format!("discord:{CHANNEL}:{}", row.event_id.as_u128() as u32),
            channel_id: &CHANNEL.to_string(),
            agent_id: Some("decision-agent"),
            provider: "claude",
            session_key: None,
            started_at: None,
            consumed_origin_ids: &ids,
            outcome,
            confirmed_at: chrono::Utc::now(),
        };
        crate::db::turns::record_turn_receipt(&self.pool, &done)
            .await
            .unwrap();
    }

    /// The ids of the originals `owner` keeps, and whether its file still stores the Decision text.
    async fn kept(owner: &ChannelMailboxHandle) -> (Vec<u64>, bool) {
        let queue = owner.v2_request(|slot| slot.as_ref().ok_or("no owner")?.json());
        let queue = queue.await.unwrap();
        let kept = (queue["retained_inputs"].as_array().unwrap().iter())
            .map(|entry| entry["input"]["original"]["message_id"].as_u64().unwrap())
            .collect();
        (kept, queue.to_string().contains(TEXT))
    }

    async fn close(self) {
        self.pool.close().await;
        self.db.drop().await;
    }
}

fn context(row: &Unsettled) -> serde_json::Value {
    json!({"origin":"headless","source":"decision",
        "metadata":{"human_input":{"origin_id":row.origin}}})
}

/// An idle handoff that answers `result` and counts its calls; it reads the journal it runs under.
#[cfg(unix)]
fn idle(
    pool: &PgPool,
    row: &Unsettled,
    result: Result<(), String>,
) -> (
    Handoff,
    Arc<AtomicUsize>,
    Arc<std::sync::Mutex<Vec<String>>>,
) {
    let calls = Arc::new(AtomicUsize::new(0));
    let seen: Arc<std::sync::Mutex<Vec<String>>> = Arc::default();
    let (counted, journal, pool, event) =
        (calls.clone(), Arc::clone(&seen), pool.clone(), row.event_id);
    let handoff: Handoff = Arc::new(move |_front| {
        let (counted, journal, pool, result) = (
            counted.clone(),
            journal.clone(),
            pool.clone(),
            result.clone(),
        );
        Box::pin(async move {
            counted.fetch_add(1, Ordering::SeqCst);
            let state = pg_row(&pool, event).await.0;
            journal.lock().unwrap().push(state);
            result
        })
    });
    (handoff, calls, seen)
}

/// A Decision front is written under the PG journal taken just before it and never twice; an
/// input box in use only makes the same original wait for its one later write.
#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn decision_front_is_journaled_before_its_one_write_pg() {
    let rig = Rig::new().await;
    let row = rig.event("normal_wait", true).await;
    let front = rig.front().await;
    // The pane is in use: the leaf proved no key was sent and the original stays queued.
    let held = Err("NotSent(\"Modal\")".to_string());
    let (busy, busy_calls, journal) = idle(&rig.pool, &row, held.clone());
    let kept = context(&row);
    let waited = hand_idle(
        Some(&rig.pool),
        &rig.mailbox,
        Some(&kept),
        front.clone(),
        &busy,
    )
    .await;
    assert_eq!(waited, held);
    assert_eq!(*journal.lock().unwrap(), ["may_have_sent"]);
    assert_eq!(
        rig.state(&row).await.0,
        "normal_wait",
        "proven unsent: it waits again"
    );
    assert_eq!(rig.queued_ids().await, [front.1.message_id]);
    // The input is ready: the same original is written once and the event ends delivered.
    let (ready, ready_calls, journal) = idle(&rig.pool, &row, Ok(()));
    let sent = hand_idle(
        Some(&rig.pool),
        &rig.mailbox,
        Some(&kept),
        front.clone(),
        &ready,
    )
    .await;
    assert_eq!(sent, Ok(()));
    assert_eq!(*journal.lock().unwrap(), ["may_have_sent"]);
    assert_eq!(rig.state(&row).await.0, "delivered");
    // The same selection arrives again: nothing is written and the leftover original leaves.
    let again = hand_idle(
        Some(&rig.pool),
        &rig.mailbox,
        Some(&kept),
        front.clone(),
        &ready,
    )
    .await;
    assert_eq!(again, Err(CLOSED.to_string()));
    assert_eq!(
        (
            busy_calls.load(Ordering::SeqCst),
            ready_calls.load(Ordering::SeqCst)
        ),
        (1, 1)
    );
    assert_eq!(rig.queued_ids().await, [0u64; 0]);
    assert_eq!(rig.state(&row).await.0, "delivered");
    // An ordinary input passes straight through and reads no PG.
    let other = rig.other_input(1).await;
    let front = rig.front().await;
    assert_eq!(front.1.message_id, other);
    let (plain, plain_calls, _) = idle(&rig.pool, &row, Ok(()));
    assert_eq!(
        hand_idle(None, &rig.mailbox, None, front, &plain).await,
        Ok(())
    );
    assert_eq!(plain_calls.load(Ordering::SeqCst), 1);
    rig.close().await;
}

/// The owner writes a Decision front only as PG approved it: the event's own origin, channel,
/// actor and body. Any other claim of the source writes nothing and moves no event.
#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn decision_front_is_written_only_as_its_event_approved_it_pg() {
    use crate::services::turn_orchestrator::PendingQueueItem;
    let rig = Rig::new().await;
    let row = rig.event("normal_wait", true).await;
    let genuine = rig.front().await;
    let claim = |origin: String| {
        json!({"origin":"headless","source":"decision",
            "metadata":{"human_input":{"origin_id":origin}}})
    };
    let altered = |edit: fn(&mut PendingQueueItem)| {
        let mut front = genuine.clone();
        edit(&mut front.1);
        front
    };
    let unregistered = format!("decision:{}", uuid::Uuid::new_v4());
    let cases: Vec<(&str, serde_json::Value, Front)> = vec![
        ("unregistered event", claim(unregistered), genuine.clone()),
        (
            "no origin",
            json!({"origin":"headless","source":"decision","metadata":{}}),
            genuine.clone(),
        ),
        (
            "malformed origin",
            claim("decision:not-an-event".into()),
            genuine.clone(),
        ),
        (
            "another opinion of the event",
            claim(format!("{}:opinion:5", row.origin)),
            genuine.clone(),
        ),
        (
            "another body",
            context(&row),
            altered(|item| item.text.push('!')),
        ),
        (
            "another channel",
            context(&row),
            altered(|item| item.channel_id = Some(CHANNEL + 1)),
        ),
        (
            "another actor",
            context(&row),
            altered(|item| item.author_id = 8),
        ),
    ];
    for (name, kept, front) in cases {
        let (runs, calls, _) = idle(&rig.pool, &row, Ok(()));
        let refused = hand_idle(Some(&rig.pool), &rig.mailbox, Some(&kept), front, &runs).await;
        assert!(refused.is_err(), "{name}: {refused:?}");
        assert_eq!(calls.load(Ordering::SeqCst), 0, "{name}: wrote");
        assert_eq!(rig.state(&row).await.0, "normal_wait", "{name}: moved");
    }
    let (runs, calls, _) = idle(&rig.pool, &row, Ok(()));
    let kept = context(&row);
    let sent = hand_idle(Some(&rig.pool), &rig.mailbox, Some(&kept), genuine, &runs).await;
    assert_eq!(sent, Ok(()));
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(rig.state(&row).await.0, "delivered");
    rig.close().await;
}

/// A closed event's original never runs: whichever closed state PG holds, the front is removed
/// unrun, another user's input behind it stays, and no notice asks anyone to send it again.
#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn decision_closed_event_is_removed_unrun_and_never_asks_for_a_resend_pg() {
    let rig = Rig::new().await;
    let other = rig.other_input(1).await;
    for (state, ends) in [
        ("closing", "failed"),
        ("failed", "failed"),
        ("unknown_closed", "unknown_closed"),
        ("may_have_sent", "unknown_closed"),
    ] {
        let row = rig.event(state, true).await;
        let message = message_of(CHANNEL, &row.origin);
        assert_eq!(rig.queued_ids().await, [other, message], "{state}");
        let queued = rig.queue().await["items"][1].clone();
        let front: Front = (None, serde_json::from_value(queued).unwrap());
        let (runs, calls, _) = idle(&rig.pool, &row, Ok(()));
        let kept = context(&row);
        let refused = hand_idle(Some(&rig.pool), &rig.mailbox, Some(&kept), front, &runs).await;
        assert_eq!(refused, Err(CLOSED.to_string()), "{state}");
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "{state}: a closed event wrote"
        );
        assert_eq!(
            rig.queued_ids().await,
            [other],
            "{state}: only its original left"
        );
        assert_eq!(rig.state(&row).await.0, ends, "{state}");
    }
    // A wake and a restart later, nothing of the closed events is queued, kept or announced.
    settle_channel(&rig.pool, &rig.mailbox, CHANNEL).await;
    let restarted = Rig::adopt(&rig.file).await;
    let requeued = restarted.v2_request(|slot| {
        let owner = slot.as_mut().ok_or("no owner")?;
        owner.requeue_known_not_sent().map(|moved| moved.len())
    });
    assert_eq!(requeued.await, Ok(0));
    let drain = crate::services::discord::queue_io::input_owner::notice::drain;
    assert_eq!(drain(&rig.pool, &restarted, "claude", CHANNEL).await, Ok(0));
    assert_eq!(rig.notices().await, 0);
    let queue = restarted.v2_request(|slot| slot.as_ref().ok_or("no owner")?.json());
    let queue = queue.await.unwrap();
    assert_eq!(queue["items"].as_array().unwrap().len(), 1, "{queue}");
    assert_eq!(queue["items"][0]["message_id"], other);
    assert_eq!(queue["retained_inputs"], json!([]));
    rig.close().await;
}

/// A failed event is fenced and exactly its original removed, an unconfirmed Decision write
/// announces nothing, and another user's input keeps its own notice.
#[tokio::test(flavor = "current_thread")]
async fn decision_failure_discards_its_original_and_keeps_other_inputs_pg() {
    let rig = Rig::new().await;
    let drain = crate::services::discord::queue_io::input_owner::notice::drain;
    // The first attempt saw the owner answer pending while the owner had committed the original.
    let pending = rig.event("resolving", true).await;
    let other = rig.other_input(1).await;
    fail(
        &rig.pool,
        Some(&rig.mailbox),
        &pending,
        "owner_admission_pending",
    )
    .await
    .unwrap();
    let ended = rig.state(&pending).await;
    assert_eq!(
        ended,
        ("failed".into(), Some("owner_admission_pending".into()))
    );
    assert_eq!(rig.queued_ids().await, [other]);
    // The other user's input ends unconfirmed and gets its notice, as it always did.
    note_open();
    rig.attempt_front("other", true).await;
    assert_eq!(
        drain(&rig.pool, &rig.mailbox, "claude", CHANNEL).await,
        Ok(1)
    );
    assert_eq!(rig.notices().await, 1);
    // A Decision write whose confirmation never came is closed unknown and announces nothing.
    let unknown = rig.event("may_have_sent", true).await;
    rig.attempt_front("decision", true).await;
    assert_eq!(
        drain(&rig.pool, &rig.mailbox, "claude", CHANNEL).await,
        Ok(0)
    );
    assert_eq!(rig.state(&unknown).await.0, "unknown_closed");
    assert_eq!(
        rig.notices().await,
        1,
        "no resend notice for a decision input"
    );
    assert_eq!(rig.queued_ids().await, [0u64; 0]);
    // Its kept original left the owner, body included, and stays gone after a restart; the other
    // user's unconfirmed original is still kept.
    for owner in [rig.mailbox.clone(), Rig::adopt(&rig.file).await] {
        assert_eq!(drain(&rig.pool, &owner, "claude", CHANNEL).await, Ok(0));
        let queue = owner.v2_request(|slot| slot.as_ref().ok_or("no owner")?.json());
        let queue = queue.await.unwrap();
        let kept: Vec<u64> = (queue["retained_inputs"].as_array().unwrap().iter())
            .map(|entry| entry["input"]["original"]["message_id"].as_u64().unwrap())
            .collect();
        assert_eq!(kept, [other], "only the decision original left: {queue}");
        assert!(!queue.to_string().contains(TEXT), "{queue}");
    }
    // The crash window: the fence is written and the process dies before the removal.
    let fenced = rig.event("normal_wait", true).await;
    assert!(
        decisions::close(&rig.pool, fenced.event_id, "start_refused")
            .await
            .unwrap()
    );
    let restarted = Rig::adopt(&rig.file).await;
    let boot = decisions::clock(&rig.pool).await.unwrap();
    let recovered = recover_before(&rig.pool, boot, |_| Some(restarted.clone())).await;
    assert_eq!(recovered.unwrap(), 1);
    assert_eq!(
        rig.state(&fenced).await,
        ("failed".into(), Some("start_refused".into()))
    );
    let queue = restarted.v2_request(|slot| slot.as_ref().ok_or("no owner")?.json());
    assert_eq!(queue.await.unwrap()["items"], json!([]));
    rig.close().await;
}

/// Boot recovery settles each row an earlier process left mid-flight exactly once and writes
/// nothing: only a completed receipt delivers, and a waiting original keeps waiting.
#[tokio::test(flavor = "current_thread")]
async fn decision_boot_recovery_settles_each_interrupted_event_exactly_once_pg() {
    let rig = Rig::new().await;
    let resolving = rig.event("resolving", false).await;
    let normalizing = rig.event("normalizing", false).await;
    let unproven = rig.event("may_have_sent", false).await;
    let received = rig.event("may_have_sent", false).await;
    rig.receipt(&received, crate::db::turns::TerminalOutcome::Completed)
        .await;
    let incomplete = rig.event("may_have_sent", false).await;
    rig.receipt(&incomplete, crate::db::turns::TerminalOutcome::Failed)
        .await;
    let waiting = rig.event("normal_wait", true).await;
    let unstarted = rig.event("not_started", false).await;
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    let boot = decisions::clock(&rig.pool).await.unwrap();
    // This process's own first attempt, started after boot, is not an interrupted one.
    let live = rig.event("resolving", false).await;
    let restarted = Rig::adopt(&rig.file).await;
    let expected = [
        (&resolving, "failed", Some(INTERRUPTED)),
        (&normalizing, "failed", Some(INTERRUPTED)),
        (&unproven, "unknown_closed", None),
        (&received, "delivered", None),
        (&incomplete, "unknown_closed", None),
        (&waiting, "normal_wait", None),
        (&unstarted, "not_started", None),
        (&live, "resolving", None),
    ];
    let mut stamps = Vec::new();
    for round in 0..2 {
        let read = recover_before(&rig.pool, boot, |_| Some(restarted.clone())).await;
        // The second run finds only the rows that are still open.
        assert_eq!(read.unwrap(), [7, 2][round], "round {round}");
        for (row, state, detail) in expected {
            let (now, reason) = rig.state(row).await;
            assert_eq!(now, state, "round {round} {}", row.origin);
            assert_eq!(reason.as_deref(), detail, "round {round} {}", row.origin);
        }
        let stamp: Vec<chrono::DateTime<chrono::Utc>> = sqlx::query_scalar(
            "SELECT updated_at FROM decision_delivery_attempts ORDER BY event_id",
        )
        .fetch_all(&rig.pool)
        .await
        .unwrap();
        stamps.push(stamp);
    }
    assert_eq!(stamps[0], stamps[1], "a settled row is not written again");
    let queue = restarted.v2_request(|slot| slot.as_ref().ok_or("no owner")?.json());
    let queue = queue.await.unwrap();
    assert_eq!(queue["items"].as_array().unwrap().len(), 1, "{queue}");
    assert!(
        queue["head_injection"].is_null(),
        "recovery reserves nothing"
    );
    rig.close().await;
}

/// Boot recovery closed the event before any owner held the channel. The owner that adopts the
/// queue file later still removes the closed event's kept original, before any notice.
#[tokio::test(flavor = "current_thread")]
async fn decision_original_closed_before_its_owner_adopted_leaves_at_adoption_pg() {
    let rig = Rig::new().await;
    let drain = crate::services::discord::queue_io::input_owner::notice::drain;
    // The earlier process journaled a write it never confirmed, and another user's as well.
    let unknown = rig.event("may_have_sent", true).await;
    rig.attempt_front("decision", true).await;
    let other = rig.other_input(1).await;
    rig.attempt_front("other", true).await;
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    let boot = decisions::clock(&rig.pool).await.unwrap();
    assert_eq!(recover_before(&rig.pool, boot, |_| None).await.unwrap(), 1);
    assert_eq!(rig.state(&unknown).await.0, "unknown_closed");
    let adopted = Rig::adopt(&rig.file).await;
    for owed in [1, 0] {
        assert_eq!(
            drain(&rig.pool, &adopted, "claude", CHANNEL).await,
            Ok(owed)
        );
        assert_eq!(rig.notices().await, 1, "only the other user's notice");
        assert_eq!(Rig::kept(&adopted).await, (vec![other], false));
    }
    rig.close().await;
}

/// The close is recorded and the removal of the original fails. The next drains find the closed
/// event's original again: its notice waits meanwhile and another user's is still posted.
#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn decision_cleanup_that_failed_after_its_close_is_found_again_pg() {
    let mut rig = Rig::new().await;
    let drain = crate::services::discord::queue_io::input_owner::notice::drain;
    async fn fail_commits(owner: &ChannelMailboxHandle) -> Result<(), String> {
        let set = owner.v2_request(|slot| {
            slot.as_mut()
                .ok_or("no owner")?
                .cancel_fault_for_tests(true);
            Ok(())
        });
        set.await
    }
    let unknown = rig.event("may_have_sent", true).await;
    rig.attempt_front("decision", true).await;
    let other = rig.other_input(1).await;
    note_open();
    fail_commits(&rig.mailbox).await.unwrap();
    let first = drain(&rig.pool, &rig.mailbox, "claude", CHANNEL).await;
    assert_eq!(rig.state(&unknown).await.0, "unknown_closed");
    assert_eq!(rig.notices().await, 0, "its notice did not wait: {first:?}");
    // Another user's write goes unconfirmed while the removal still fails.
    rig.mailbox = Rig::adopt(&rig.file).await;
    rig.attempt_front("other", true).await;
    fail_commits(&rig.mailbox).await.unwrap();
    let _ = drain(&rig.pool, &rig.mailbox, "claude", CHANNEL).await;
    assert_eq!(rig.notices().await, 1, "the other user's notice is posted");
    // The storage works again.
    rig.mailbox = Rig::adopt(&rig.file).await;
    for owed in [1, 0] {
        let drained = drain(&rig.pool, &rig.mailbox, "claude", CHANNEL).await;
        assert_eq!(drained, Ok(owed));
        assert_eq!(rig.notices().await, 1, "no notice for the closed decision");
        assert_eq!(Rig::kept(&rig.mailbox).await, (vec![other], false));
    }
    rig.close().await;
}

/// A booting node ends only attempts whose holder's run is over. Another node that is still
/// online keeps its attempts whatever their age, and they settle once it is listed offline.
#[tokio::test(flavor = "current_thread")]
async fn decision_boot_recovery_leaves_a_live_nodes_attempts_alone_pg() {
    const NODE_A: &str = "decision-node-a";
    let rig = Rig::new().await;
    sqlx::query(
        "INSERT INTO worker_nodes (instance_id, status, last_heartbeat_at)
         VALUES ($1, 'online', NOW())",
    )
    .bind(NODE_A)
    .execute(&rig.pool)
    .await
    .unwrap();
    // Node A journaled these long ago and is still running them.
    let rows = [
        rig.event("resolving", false).await,
        rig.event("normalizing", false).await,
        rig.event("may_have_sent", false).await,
    ];
    sqlx::query(
        "UPDATE decision_delivery_attempts
         SET holder_instance_id = $1, updated_at = NOW() - INTERVAL '1 day'",
    )
    .bind(NODE_A)
    .execute(&rig.pool)
    .await
    .unwrap();
    type Row = (String, chrono::DateTime<chrono::Utc>, Option<String>);
    async fn read(pool: &PgPool) -> Vec<Row> {
        sqlx::query_as(
            "SELECT state, updated_at, detail FROM decision_delivery_attempts ORDER BY event_id",
        )
        .fetch_all(pool)
        .await
        .unwrap()
    }
    let before = read(&rig.pool).await;
    // Node B boots: its own mailbox registry holds none of A's originals.
    recover_boot(&rig.pool).await.unwrap();
    let boot = decisions::clock(&rig.pool).await.unwrap();
    for _ in 0..2 {
        recover_before(&rig.pool, boot, |_| None).await.unwrap();
    }
    assert_eq!(
        read(&rig.pool).await,
        before,
        "a live node's attempts were ended"
    );
    // A row read as abandoned just before its holder moved it is not ended either.
    for row in &rows {
        let judged = Unsettled {
            stale: true,
            ..row.clone()
        };
        settle(&rig.pool, None, &judged, None).await.unwrap();
    }
    assert_eq!(
        read(&rig.pool).await,
        before,
        "the closing update ignored the holder"
    );
    // A's run is over: the cluster lists it offline.
    sqlx::query("UPDATE worker_nodes SET status = 'offline' WHERE instance_id = $1")
        .bind(NODE_A)
        .execute(&rig.pool)
        .await
        .unwrap();
    assert_eq!(recover_before(&rig.pool, boot, |_| None).await.unwrap(), 3);
    let ends = [
        ("failed", Some(INTERRUPTED)),
        ("failed", Some(INTERRUPTED)),
        ("unknown_closed", None),
    ];
    for (row, (state, detail)) in rows.iter().zip(ends) {
        let ended = (state.to_string(), detail.map(str::to_string));
        assert_eq!(rig.state(row).await, ended, "{}", row.origin);
    }
    rig.close().await;
}

/// A start is no delivery: without an owner the journaled row waits for the receipt of the turn
/// that consumed its origin. A completed one delivers it; any other end closes it unknown.
#[tokio::test(flavor = "current_thread")]
async fn decision_started_event_settles_only_on_the_receipt_of_its_turn_pg() {
    use crate::db::turns::TerminalOutcome;
    let rig = Rig::new().await;
    let completed = rig.event("may_have_sent", false).await;
    let failed = rig.event("may_have_sent", false).await;
    for row in [&completed, &failed] {
        settle(&rig.pool, None, row, None).await.unwrap();
        assert_eq!(rig.state(row).await.0, "may_have_sent", "no receipt yet");
    }
    rig.receipt(&completed, TerminalOutcome::Completed).await;
    rig.receipt(&failed, TerminalOutcome::Failed).await;
    settle(&rig.pool, None, &completed, None).await.unwrap();
    settle(&rig.pool, None, &failed, None).await.unwrap();
    assert_eq!(rig.state(&completed).await.0, "delivered");
    assert_eq!(rig.state(&failed).await.0, "unknown_closed");
    rig.close().await;
}
