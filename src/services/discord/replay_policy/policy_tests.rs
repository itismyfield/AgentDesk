use super::live::{Activity, RawObservation, Seen, TerminalKind};
use super::{ReplayDecision, decide_stale};
use crate::db::auto_queue::test_support::TestPostgresDb;
use crate::db::replay_disposition::write::{
    self, BeginFrom, CanonicalInput, EffectProjection, ExactAttempt,
};
use sqlx::PgPool;

async fn start(pool: &PgPool, key: &str) -> ExactAttempt {
    let request = CanonicalInput {
        request_key: key.into(),
        provider: "claude".into(),
        channel: "chan".into(),
        sources: vec![format!("{key}-source")],
        original_text: "original".into(),
        owner_id: "user".into(),
        agent_id: "agent".into(),
        attachments: serde_json::json!([]),
        reply_context: None,
        provenance: "new_input",
    };
    let receipt = write::register_or_reuse(pool, &request, "node-a")
        .await
        .unwrap();
    let nonce = receipt.episode_nonce.unwrap();
    let binding = serde_json::json!({});
    let effect = EffectProjection {
        effect_target: "target",
        input_hash: "input",
        binding: &binding,
    };
    let from = BeginFrom::Registered { nonce: &nonce };
    let ack = write::begin(pool, receipt.id, from, &effect, "incarnation-a")
        .await
        .unwrap();
    ack.into_parts().0
}

async fn stored(pool: &PgPool, id: i64) -> (String, Option<String>, serde_json::Value) {
    sqlx::query_as(
        "SELECT replay_disposition, replay_hold_reason, replay_preserved FROM intake_outbox WHERE id = $1",
    )
    .bind(id)
    .fetch_one(pool)
    .await
    .unwrap()
}

fn observed(attempt: &ExactAttempt, extra: &[Seen], complete: bool) -> RawObservation {
    let mut observation = RawObservation::for_attempt(attempt);
    let base: &[Seen] = if complete {
        &[
            Seen::StdoutEof,
            Seen::Exited { waited: true },
            Seen::StderrDrained,
        ]
    } else {
        &[Seen::StdoutEof, Seen::Exited { waited: true }]
    };
    for seen in base.iter().chain(extra) {
        observation.record(*seen);
    }
    observation
}

#[tokio::test(flavor = "current_thread")]
async fn stale_attempt_retries_only_on_committed_no_effect_evidence_pg() {
    let fixture = TestPostgresDb::create().await;
    let pool = fixture.connect_and_migrate_with_max_connections(4).await;
    let probe = start(&pool, "legacy-probe").await;
    let before = stored(&pool, probe.receipt_id()).await;
    let legacy = observed(&probe, &[], true);
    assert!(matches!(
        decide_stale(&pool, None, &legacy).await,
        ReplayDecision::NotApplicable
    ));
    assert_eq!(
        stored(&pool, probe.receipt_id()).await,
        before,
        "no attempt, no write"
    );

    let quiet = start(&pool, "quiet").await;
    let (id, previous) = (quiet.receipt_id(), quiet.nonce().to_string());
    let observation = observed(&quiet, &[Seen::Terminal(TerminalKind::Error)], true);
    let ReplayDecision::AllowStartupRetry(permit) =
        decide_stale(&pool, Some(quiet), &observation).await
    else {
        panic!("complete no-effect evidence must allow one retry");
    };
    assert_eq!(
        (permit.receipt_id(), permit.previous_nonce()),
        (id, previous.as_str())
    );
    assert_ne!(permit.next_nonce(), previous);
    assert_eq!(stored(&pool, id).await.0, "startup_failed_no_effect");

    let activities = [Activity::Output, Activity::Tool, Activity::Unclassified];
    let cases = activities
        .iter()
        .map(|activity| {
            (
                vec![Seen::Activity(*activity)],
                true,
                "activity after start",
            )
        })
        .chain([
            (vec![], false, "incomplete or unclassified observation"),
            (
                vec![Seen::Terminal(TerminalKind::Done)],
                true,
                "incomplete or unclassified observation",
            ),
        ]);
    for (n, (extra, complete, reason)) in cases.enumerate() {
        let attempt = start(&pool, &format!("held-{n}")).await;
        let (id, nonce) = (attempt.receipt_id(), attempt.nonce().to_string());
        let observation = observed(&attempt, &extra, complete);
        let decision = decide_stale(&pool, Some(attempt), &observation).await;
        assert!(
            matches!(decision, ReplayDecision::WithholdReplay { persisted: true }),
            "{extra:?} complete={complete}: {decision:?}"
        );
        let (disposition, hold_reason, preserved) = stored(&pool, id).await;
        assert_eq!(
            (disposition.as_str(), hold_reason.as_deref()),
            ("withheld", Some(reason))
        );
        assert_eq!(preserved["hold"]["nonce"], nonce.as_str());
    }
    pool.close().await;
    fixture.drop().await;
}

async fn fail_commit_of(pool: &PgPool, disposition: &str) {
    sqlx::raw_sql(&format!(
        "CREATE OR REPLACE FUNCTION lose_commit() RETURNS trigger LANGUAGE plpgsql AS $$
         BEGIN RAISE EXCEPTION 'commit acknowledgement lost'; END $$;
         CREATE CONSTRAINT TRIGGER lose_{disposition} AFTER UPDATE ON intake_outbox
             DEFERRABLE INITIALLY DEFERRED FOR EACH ROW
             WHEN (NEW.replay_disposition = '{disposition}')
             EXECUTE FUNCTION lose_commit();"
    ))
    .execute(pool)
    .await
    .unwrap();
}

// A stale decision whose classification misses its attempt, loses its commit or cannot store
// its hold reports an unpersisted hold and never a retry permit.
#[tokio::test(flavor = "current_thread")]
async fn stale_decision_failures_hold_without_a_permit_pg() {
    let fixture = TestPostgresDb::create().await;
    let pool = fixture.connect_and_migrate_with_max_connections(4).await;

    let elsewhere = start(&pool, "held-elsewhere").await;
    let id = elsewhere.receipt_id();
    let preserved = serde_json::json!({ "hold": "elsewhere" });
    write::withhold(&pool, &elsewhere, "held elsewhere", &preserved)
        .await
        .unwrap();
    let quiet = observed(&elsewhere, &[], true);
    let decision = decide_stale(&pool, Some(elsewhere), &quiet).await;
    assert!(
        matches!(
            decision,
            ReplayDecision::WithholdReplay { persisted: false }
        ),
        "CAS miss: {decision:?}"
    );
    assert_eq!(stored(&pool, id).await.1.as_deref(), Some("held elsewhere"));

    fail_commit_of(&pool, "startup_failed_no_effect").await;
    let lost = start(&pool, "lost-no-effect").await;
    let id = lost.receipt_id();
    let quiet = observed(&lost, &[], true);
    let decision = decide_stale(&pool, Some(lost), &quiet).await;
    assert!(
        matches!(
            decision,
            ReplayDecision::WithholdReplay { persisted: false }
        ),
        "lost no-effect commit: {decision:?}"
    );
    assert_eq!(stored(&pool, id).await.0, "started_unclassified");

    fail_commit_of(&pool, "withheld").await;
    let active = start(&pool, "lost-hold").await;
    let id = active.receipt_id();
    let busy = observed(&active, &[Seen::Activity(Activity::Tool)], true);
    let decision = decide_stale(&pool, Some(active), &busy).await;
    assert!(
        matches!(
            decision,
            ReplayDecision::WithholdReplay { persisted: false }
        ),
        "lost hold commit: {decision:?}"
    );
    assert_eq!(stored(&pool, id).await.0, "started_unclassified");
    pool.close().await;
    fixture.drop().await;
}
