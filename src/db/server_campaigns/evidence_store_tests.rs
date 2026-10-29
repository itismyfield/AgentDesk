use super::*;
use crate::db::server_campaigns::tests::Harness;
use crate::services::campaign_execution::{
    control::tests as domain,
    evidence::EvidenceEnvelope,
    evidence_gate::{
        GATE_RULE_VERSION,
        tests::{OUTPUT, claim, seal, workspace},
    },
};
use sqlx::PgPool;

fn envelope(output: &str) -> EvidenceEnvelope {
    claim(&domain::binding(&domain::running()), output)
}

async fn record_in(pool: &PgPool, stored: &StoredEvidence) -> Result<StoredEvidence> {
    let mut tx = pool.begin().await?;
    let recorded = record(&mut tx, "campaign", "run", stored).await?;
    tx.commit().await?;
    Ok(recorded)
}

async fn decide_in(pool: &PgPool, id: &str, status: EvidenceStatus) -> Result<StoredEvidence> {
    let decision = EvidenceDecision {
        status,
        reasons: Vec::new(),
        prerequisites: Vec::new(),
        rule_version: GATE_RULE_VERSION.into(),
    };
    let mut tx = pool.begin().await?;
    let decided = decide(&mut tx, "campaign", "run", id, &decision).await?;
    tx.commit().await?;
    Ok(decided)
}

async fn rows(pool: &PgPool, id: &str) -> Vec<serde_json::Value> {
    sqlx::query_scalar::<_, Json<serde_json::Value>>(
        "SELECT document FROM server_campaign_evidence WHERE campaign_id = 'campaign' AND evidence_id = $1",
    )
    .bind(id)
    .fetch_all(pool)
    .await
    .unwrap()
    .into_iter()
    .map(|v| v.0)
    .collect()
}

#[tokio::test]
async fn same_evidence_id_keeps_the_first_identity_under_concurrent_submission_pg() {
    let h = Harness::new().await;
    h.seed(domain::running()).await;
    let first = StoredEvidence::recorded(envelope(OUTPUT));
    let (a, b) = tokio::join!(record_in(&h.pool, &first), record_in(&h.pool, &first));
    assert_eq!((a.unwrap(), b.unwrap()), (first.clone(), first.clone()));
    let id = "tui:attempt:output";
    // Same ID, different digest: whichever order the race takes, the stored row stays the first one.
    let other = StoredEvidence::recorded(envelope("output-h2"));
    let (x, y) = tokio::join!(record_in(&h.pool, &other), record_in(&h.pool, &first));
    assert!(matches!(x, Err(EvidenceStoreError::Conflict)));
    assert_eq!(y.unwrap(), first);
    assert_eq!(
        rows(&h.pool, id).await,
        vec![serde_json::to_value(&first).unwrap()]
    );
    let binding = domain::binding(&domain::running());
    let sealed = StoredEvidence::Seal {
        seal: Box::new(seal(&binding)),
    };
    record_in(&h.pool, &sealed).await.unwrap();
    let mut later = seal(&binding);
    later.sealed_at += chrono::Duration::minutes(3);
    later.workspace.observed_at = later.sealed_at;
    let retry = StoredEvidence::Seal {
        seal: Box::new(later),
    };
    assert_eq!(record_in(&h.pool, &retry).await.unwrap(), sealed);
    let mut moved = seal(&binding);
    moved.output_sha = "output-h2".into();
    moved.workspace = workspace("output-h2");
    let replace = StoredEvidence::Seal {
        seal: Box::new(moved),
    };
    assert!(matches!(
        record_in(&h.pool, &replace).await,
        Err(EvidenceStoreError::Conflict)
    ));
    assert_eq!(
        rows(&h.pool, "seal:attempt").await,
        vec![serde_json::to_value(&sealed).unwrap()]
    );
    h.close().await;
}

#[tokio::test]
async fn legacy_rows_are_kept_and_decisions_move_one_way_pg() {
    let h = Harness::new().await;
    h.seed(domain::running()).await;
    let legacy =
        serde_json::json!({"id": "tui:attempt:output", "kind": "test", "source": "fixture-log"});
    sqlx::query(
        "INSERT INTO server_campaign_evidence (campaign_id, run_id, attempt_id, evidence_id, document) \
         VALUES ('campaign', 'run', 'attempt', 'tui:attempt:output', $1)",
    )
    .bind(Json(&legacy))
    .execute(&h.pool)
    .await
    .unwrap();
    let submitted = StoredEvidence::recorded(envelope(OUTPUT));
    assert!(matches!(
        record_in(&h.pool, &submitted).await,
        Err(EvidenceStoreError::Conflict)
    ));
    assert!(matches!(
        decide_in(&h.pool, "tui:attempt:output", EvidenceStatus::Accepted).await,
        Err(EvidenceStoreError::Conflict)
    ));
    assert_eq!(rows(&h.pool, "tui:attempt:output").await, vec![legacy]);
    let mut forged = envelope(OUTPUT);
    forged.evidence_id = "tui:attempt:forged".into();
    forged.payload_digest = envelope("output-h2").payload_digest;
    assert!(matches!(
        record_in(&h.pool, &StoredEvidence::recorded(forged)).await,
        Err(EvidenceStoreError::Conflict)
    ));
    assert!(rows(&h.pool, "tui:attempt:forged").await.is_empty());
    let mut request = envelope(OUTPUT);
    request.evidence_id = "tui:attempt:request".into();
    record_in(&h.pool, &StoredEvidence::recorded(request))
        .await
        .unwrap();
    decide_in(&h.pool, "tui:attempt:request", EvidenceStatus::Recorded)
        .await
        .unwrap();
    assert!(matches!(
        decide_in(&h.pool, "tui:attempt:request", EvidenceStatus::Accepted).await,
        Err(EvidenceStoreError::Transition)
    ));
    let mut fresh = envelope(OUTPUT);
    fresh.evidence_id = "tui:attempt:fresh".into();
    let id = fresh.evidence_id.clone();
    record_in(&h.pool, &StoredEvidence::recorded(fresh.clone()))
        .await
        .unwrap();
    use EvidenceStatus::*;
    for (status, allowed) in [
        (Accepted, true),
        (Accepted, true),
        (Rejected, false),
        (Superseded, true),
        (Accepted, false),
    ] {
        let outcome = decide_in(&h.pool, &id, status).await;
        assert_eq!(outcome.is_ok(), allowed, "{status:?}");
    }
    let stored: StoredEvidence =
        serde_json::from_value(rows(&h.pool, &id).await.remove(0)).unwrap();
    let StoredEvidence::Evidence {
        envelope, status, ..
    } = stored
    else {
        unreachable!()
    };
    assert_eq!((*envelope, status), (fresh, Superseded));
    h.close().await;
}
