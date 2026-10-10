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

    let activities = [
        Activity::Output,
        Activity::Tool,
        Activity::Unclassified,
        Activity::InjectedInput,
    ];
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
