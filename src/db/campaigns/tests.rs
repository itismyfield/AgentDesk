use super::*;

fn input() -> CampaignInput {
    serde_json::from_value(serde_json::json!({
        "title": "Session continuity", "status": "active", "round": 2,
        "nodes": [
            {"id": "implement", "title": "Persist checkpoints", "status": "completed",
             "stage": "implement", "round": 2, "evidence": ["Tests passed"],
             "details": "Keep canonical progress across context clears.",
             "acceptance": ["Concurrent updates cannot lose progress"],
             "findings": ["Process-local state cannot survive provider quota termination"],
             "evidence_records": [{"summary": "CAS is fenced", "command": "cargo test campaigns",
                  "result": "passed", "head_sha": "abc123", "references": []}]},
            {"id": "review", "title": "Review checkpoint", "status": "pending",
             "stage": "review", "round": 2, "dependencies": ["implement"],
             "next_action": "Review the persisted CAS evidence and run the concurrent-write test"}
        ]
    }))
    .expect("campaign fixture")
}

#[test]
fn campaign_validation_rejects_missing_duplicate_and_cyclic_dependencies() {
    let valid = input();
    assert!(validate(&valid).is_ok());
    let mut bad = valid.clone();
    bad.nodes[0].dependencies = vec!["absent".into()];
    assert!(
        validate(&bad)
            .unwrap_err()
            .to_string()
            .contains("missing dependency")
    );
    bad.nodes[0].dependencies = vec!["review".into()];
    assert!(validate(&bad).unwrap_err().to_string().contains("cycle"));
    bad.nodes[0].dependencies = vec!["implement".into()];
    assert!(validate(&bad).unwrap_err().to_string().contains("cycle"));
    bad = valid.clone();
    bad.nodes[1].dependencies.push("implement".into());
    assert!(
        validate(&bad)
            .unwrap_err()
            .to_string()
            .contains("repeats dependency")
    );
    bad = valid;
    bad.nodes[1].id = "implement".into();
    assert!(
        validate(&bad)
            .unwrap_err()
            .to_string()
            .contains("duplicate node id")
    );
}

#[test]
fn campaign_validation_rejects_false_completion_and_bad_identity() {
    let mut campaign = input();
    campaign.status = CampaignStatus::Completed;
    assert!(validate(&campaign).is_err());
    campaign.nodes[1].status = NodeStatus::Skipped;
    assert!(validate(&campaign).is_ok());
    campaign.nodes.clear();
    assert!(validate(&campaign).is_err());
    for id in ["", "white space", "../path", "한글"] {
        assert!(validate_id(id).is_err());
    }
}

#[test]
fn campaign_checkpoint_keeps_unchanged_node_time_and_resume_context() {
    let mut first = checkpoint("campaign".into(), input(), None);
    let old = Utc::now() - chrono::Duration::days(1);
    first.created_at = old;
    for node in &mut first.nodes {
        node.updated_at = old;
    }
    let mut next = input();
    next.nodes[1].session_id = Some("new-session-after-clear".into());
    let second = checkpoint("campaign".into(), next, Some(&first));
    assert_eq!(second.revision, 2);
    assert_eq!(second.created_at, old);
    assert_eq!(second.nodes[0].updated_at, old);
    assert!(second.nodes[1].updated_at > old);
    let serialized = serde_json::to_value(&second).expect("serialize");
    assert_eq!(
        serialized["nodes"][0]["evidence_records"][0]["result"],
        "passed"
    );
    assert_eq!(
        serialized["nodes"][1]["session_id"],
        "new-session-after-clear"
    );
    assert_eq!(
        serialized["nodes"][0]["acceptance"][0],
        "Concurrent updates cannot lose progress"
    );
}

#[tokio::test]
async fn postgres_campaign_concurrent_cas_and_reconnect_preserve_canonical_history_pg() {
    let fixture = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = fixture.connect_and_migrate().await;
    let first = create(&pool, "continuity".into(), input())
        .await
        .expect("create");
    assert_eq!(first.revision, 1);
    assert!(matches!(
        create(&pool, "continuity".into(), input()).await,
        Err(CampaignError::Conflict)
    ));
    let mut left_input = input();
    left_input.nodes[1].session_id = Some("session-left".into());
    let mut right_input = input();
    right_input.nodes[1].session_id = Some("session-right".into());
    let (left, right) = tokio::join!(
        replace(&pool, "continuity", 1, left_input),
        replace(&pool, "continuity", 1, right_input)
    );
    assert_eq!(usize::from(left.is_ok()) + usize::from(right.is_ok()), 1);
    let (winner, loser) = if left.is_ok() {
        (left, right)
    } else {
        (right, left)
    };
    let winner = winner.expect("one winner");
    assert!(matches!(loser, Err(CampaignError::Conflict)));
    assert_eq!(winner.revision, 2);
    let mut invalid = input();
    invalid.nodes[0].dependencies.push("review".into());
    assert!(matches!(
        replace(&pool, "continuity", 2, invalid).await,
        Err(CampaignError::Validation(_))
    ));
    pool.close().await;
    // A fresh pool represents a fresh server/session: recovery is a normal DB
    // read, with no file replay or process-local source of truth.
    let restarted = fixture.connect_and_migrate().await;
    let restored = get(&restarted, "continuity").await.expect("restore");
    assert_eq!(
        serde_json::to_value(restored).unwrap(),
        serde_json::to_value(winner).unwrap()
    );
    let revisions = history(&restarted, "continuity").await.expect("history");
    assert_eq!(
        revisions.iter().map(|v| v.revision).collect::<Vec<_>>(),
        [2, 1]
    );
    assert_eq!(list(&restarted, 100, 0).await.expect("list").len(), 1);
    restarted.close().await;
    fixture.drop().await;
}
