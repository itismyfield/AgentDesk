use super::*;
use crate::db::turns::{TerminalOutcome, TurnReceipt, record_turn_receipt};
use serde_json::json;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

async fn end_turn(pool: &PgPool, turn_id: &str, origins: &[&str], outcome: TerminalOutcome) {
    let ids: Vec<String> = origins.iter().map(|o| o.to_string()).collect();
    let receipt = TurnReceipt {
        turn_id,
        channel_id: "7340901",
        agent_id: Some("worker-a"),
        provider: "antigravity",
        session_key: None,
        started_at: None,
        consumed_origin_ids: &ids,
        outcome,
        confirmed_at: chrono::Utc::now(),
    };
    record_turn_receipt(pool, &receipt).await.unwrap();
}

/// Every terminal end of a delegated execution, whichever writer committed it, stages one card in
/// the requesting channel; repeated or late terminals and a later pass after a restart add none.
#[tokio::test(flavor = "current_thread")]
async fn each_ended_delegation_gets_one_result_card_in_its_requesting_channel_pg() {
    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let origin = |worker| DelegationOrigin::new(7_000_001, worker).unwrap().id();
    let (done, stopped, failed) = (origin("worker-a"), origin("worker-a"), origin("worker-b"));
    let events = json!([{"kind": "result", "status": "success", "content": "Final: 42"}]);
    sqlx::query(
        "INSERT INTO session_transcripts (turn_id, channel_id, provider, events_json)
         VALUES ('discord:7340901:1', '7340901', 'antigravity', $1)",
    )
    .bind(events)
    .execute(&pool)
    .await
    .unwrap();
    end_turn(
        &pool,
        "discord:7340901:1",
        &[&done],
        TerminalOutcome::Completed,
    )
    .await;
    end_turn(
        &pool,
        "discord:7340901:2",
        &[&stopped],
        TerminalOutcome::Cancelled,
    )
    .await;
    end_turn(
        &pool,
        "discord:7340901:3",
        &[&failed],
        TerminalOutcome::Failed,
    )
    .await;
    end_turn(
        &pool,
        "discord:7340901:4",
        &["G-plain"],
        TerminalOutcome::Completed,
    )
    .await;
    let old = origin("worker-a");
    end_turn(
        &pool,
        "discord:7340901:5",
        &[&old],
        TerminalOutcome::Completed,
    )
    .await;
    sqlx::query("UPDATE turns SET finished_at = NOW() - INTERVAL '30 hours' WHERE turn_id = $1")
        .bind("discord:7340901:5")
        .execute(&pool)
        .await
        .unwrap();

    assert_eq!(publish_result_cards(&pool).await.unwrap(), 3);
    let cards = || async {
        let rows: Vec<(String, String, String)> = sqlx::query_as(
            "SELECT target, bot, content FROM message_outbox
              WHERE reason_code = 'delegation.result' ORDER BY id",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        rows
    };
    let staged = cards().await;
    assert_eq!(staged.len(), 3, "{staged:?}");
    for (target, bot, _) in &staged {
        assert_eq!(
            (target.as_str(), bot.as_str()),
            ("channel:7000001", "notify")
        );
    }
    let expect = [
        ("✅ 완료", "Final: 42", &done),
        ("⏹ 중단됨", "<#7340901>", &stopped),
        ("❌ 실패", "<#7340901>", &failed),
    ];
    for ((_, _, content), (outcome, body, origin)) in staged.iter().zip(expect) {
        let has = |part: &str| content.contains(part);
        assert!(has(outcome) && has(body) && has(origin), "{content}");
    }

    // A repeated terminal, a late completion of the stopped delegation, and a pass after the cards
    // went out (as after a restart) stage nothing more.
    end_turn(
        &pool,
        "discord:7340901:1",
        &[&done],
        TerminalOutcome::Completed,
    )
    .await;
    end_turn(
        &pool,
        "discord:7340901:6",
        &[&stopped],
        TerminalOutcome::Completed,
    )
    .await;
    assert_eq!(publish_result_cards(&pool).await.unwrap(), 0);
    sqlx::query("UPDATE message_outbox SET status = 'sent', sent_at = NOW()")
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(publish_result_cards(&pool).await.unwrap(), 0);
    assert_eq!(cards().await, staged);

    pool.close().await;
    db.drop().await;
}

/// A delegation handoff starts with an origin the server minted from the requesting agent's own
/// channel, on the metadata path the execution reads; a plain handoff's origin passes through.
#[tokio::test(flavor = "current_thread")]
async fn a_delegation_handoff_carries_a_server_minted_origin_pg() {
    use super::super::input_result::metadata_origin_id;
    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    sqlx::query(
        "INSERT INTO agents (id, name, provider, discord_channel_id)
         VALUES ('sec', 'sec', 'antigravity', '7100001')",
    )
    .execute(&pool)
    .await
    .unwrap();

    let started = handoff_origin(
        &pool,
        "sec",
        "worker-a",
        true,
        None,
        None,
        Some(json!({"k": 1})),
    );
    let (metadata, delegation) = started.await.unwrap();
    let delegation = delegation.unwrap();
    let id = delegation.origin.id();
    assert_eq!(metadata_origin_id(metadata.as_ref()), Some(id.clone()));
    assert_eq!(metadata.unwrap()["k"], 1);
    assert_eq!(
        DelegationOrigin::parse(&id),
        Some(delegation.origin.clone())
    );
    assert_eq!(
        (
            delegation.origin.requester_channel,
            delegation.origin.worker_agent.as_str()
        ),
        (7_100_001, "worker-a")
    );
    assert_eq!(delegation.requester_provider, ProviderKind::Antigravity);

    let refused = handoff_origin(&pool, "sec", "worker-a", true, None, Some("G-1"), None).await;
    assert_eq!(refused.unwrap_err(), (400, "origin_id_conflict"));
    let unknown = handoff_origin(&pool, "nobody", "worker-a", true, None, None, None).await;
    assert_eq!(unknown.unwrap_err().0, 404);
    let plain = handoff_origin(&pool, "sec", "worker-a", false, None, Some("G-9"), None).await;
    let (metadata, delegation) = plain.unwrap();
    assert_eq!(
        (metadata_origin_id(metadata.as_ref()).as_deref(), delegation),
        (Some("G-9"), None)
    );

    pool.close().await;
    db.drop().await;
}

#[derive(Default)]
struct FakeControl {
    state: Mutex<Option<Value>>,
    reads: AtomicUsize,
    stops: AtomicUsize,
}

#[async_trait::async_trait]
impl DelegationControl for FakeControl {
    async fn input_result(&self, _: &str, _: &str) -> Result<Value, String> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        self.state
            .lock()
            .unwrap()
            .clone()
            .ok_or("input_result_unavailable".into())
    }
    async fn stop(&self, _: &str) -> Result<Value, String> {
        self.stops.fetch_add(1, Ordering::SeqCst);
        Ok(json!({"ok": true}))
    }
}

/// Only the owner in the requesting channel reaches the APIs; `[중단]` calls turn/stop only while
/// input-result reports the delegation running, never after it ends or on an unreadable state.
#[tokio::test(flavor = "current_thread")]
async fn card_controls_obey_only_the_owner_and_stop_only_a_running_delegation() {
    let origin = DelegationOrigin::new(42, "worker-a").unwrap();
    assert_eq!(
        parse_click(&Click::Stop.custom_id(&origin)),
        Some((Click::Stop, origin.clone()))
    );
    for forged in [
        "dlg-stop|dlg:42:zz:worker-a",
        "dlg-kill|dlg:42",
        "dlg-status|G-1",
    ] {
        assert_eq!(parse_click(forged), None, "{forged}");
    }
    let control = FakeControl::default();
    let press = |click, user, channel| {
        let actor = Actor {
            owner: Some(7),
            user,
            channel,
        };
        let (control, origin) = (&control, &origin);
        async move { click_reply(control, click, origin, &actor).await }
    };
    let count = |n: &AtomicUsize| n.load(Ordering::SeqCst);

    for (user, channel) in [(8, 42), (7, 43)] {
        assert!(press(Click::Stop, user, channel).await.contains("소유자만"));
    }
    assert_eq!((count(&control.reads), count(&control.stops)), (0, 0));

    assert!(
        press(Click::Stop, 7, 42)
            .await
            .contains("중단하지 않았습니다")
    );
    let running = json!({"consumption": "consumed", "channel_id": "9", "terminal": null});
    *control.state.lock().unwrap() = Some(running);
    assert!(press(Click::Status, 7, 42).await.contains("실행 중"));
    assert!(
        press(Click::Stop, 7, 42)
            .await
            .contains("중단을 요청했습니다")
    );
    assert_eq!(count(&control.stops), 1);

    let ended = json!({"consumption": "consumed", "channel_id": "9",
        "terminal": {"outcome": "cancelled", "confirmed_at": "2026-11-21T00:00:00Z"}});
    *control.state.lock().unwrap() = Some(ended);
    let again = press(Click::Stop, 7, 42).await;
    assert!(
        again.contains("중단할 실행이 없습니다") && again.contains("중단됨"),
        "{again}"
    );
    assert_eq!(count(&control.stops), 1);
}
