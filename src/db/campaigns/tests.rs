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
    let mut initial = input();
    initial.nodes[0].group = Some("  구현  ".into());
    initial.nodes[1].group = Some(" \n\t ".into());
    let first = create(&pool, "continuity".into(), initial)
        .await
        .expect("create");
    assert_eq!(first.revision, 1);
    assert_eq!(first.nodes[0].input.group.as_deref(), Some("구현"));
    assert_eq!(first.nodes[1].input.group, None);
    assert!(matches!(
        create(&pool, "continuity".into(), input()).await,
        Err(CampaignError::Conflict)
    ));
    let mut left_input = input();
    left_input.nodes[1].session_id = Some("session-left".into());
    left_input.nodes[0].group = Some("  backend  ".into());
    let mut right_input = input();
    right_input.nodes[1].session_id = Some("session-right".into());
    right_input.nodes[0].group = Some("  review  ".into());
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
    assert!(matches!(
        winner.nodes[0].input.group.as_deref(),
        Some("backend" | "review")
    ));
    let winning_group = winner.nodes[0].input.group.clone();
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
    assert_eq!(revisions[0].nodes[0].input.group, winning_group);
    assert_eq!(revisions[1].nodes[0].input.group.as_deref(), Some("구현"));
    // An explicit blank clears the optional label without altering history.
    let mut cleared_input = input();
    cleared_input.nodes[0].group = Some(" \t ".into());
    let cleared = replace(&restarted, "continuity", 2, cleared_input)
        .await
        .expect("clear group");
    assert_eq!(cleared.nodes[0].input.group, None);
    let revisions = history(&restarted, "continuity")
        .await
        .expect("group history");
    assert_eq!(revisions[0].revision, 3);
    assert_eq!(revisions[1].nodes[0].input.group, winning_group);
    assert_eq!(list(&restarted, 100, 0).await.expect("list").len(), 1);
    restarted.close().await;
    fixture.drop().await;
}

#[test]
fn campaign_checkpoint_normalizes_optional_groups_without_inference() {
    let legacy = input();
    assert!(legacy.nodes.iter().all(|node| node.group.is_none()));
    let mut first = checkpoint("legacy".into(), legacy, None);
    assert!(serde_json::to_value(&first).unwrap()["nodes"][0]["group"].is_null());
    // Old stored documents have no group key; deserializing them must retain
    // every other field and expose an unclassified node, never infer a label.
    let mut document = serde_json::to_value(&first).unwrap();
    for node in document["nodes"].as_array_mut().unwrap() {
        node.as_object_mut().unwrap().remove("group");
    }
    let restored: Campaign = serde_json::from_value(document).unwrap();
    assert!(restored.nodes.iter().all(|node| node.input.group.is_none()));

    let old = Utc::now() - chrono::Duration::days(1);
    first.nodes[0].input.group = Some("QA 팀".into());
    first.nodes[0].updated_at = old;
    let mut next = input();
    next.nodes[0].group = Some(" \n QA 팀 \t".into());
    next.nodes[1].group = Some(" \n ".into());
    let normalized = checkpoint("legacy".into(), next, Some(&first));
    assert_eq!(normalized.nodes[0].input.group.as_deref(), Some("QA 팀"));
    assert_eq!(normalized.nodes[0].updated_at, old);
    assert_eq!(normalized.nodes[1].input.group, None);
    assert_eq!(normalized.nodes[0].input.stage, "implement");
    assert_eq!(normalized.nodes[0].input.status, NodeStatus::Completed);
}

#[tokio::test]
async fn postgres_campaign_revision_history_stays_bounded_by_retention_pg() {
    let fixture = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = fixture.connect_and_migrate().await;
    create(&pool, "bounded".into(), input())
        .await
        .expect("create");
    let writes = REVISION_RETENTION + 5;
    for revision in 1..=writes {
        let mut next = input();
        next.nodes[1].next_action = Some(format!("write {revision}"));
        replace(&pool, "bounded", revision, next)
            .await
            .expect("replace");
    }
    // Counted straight from the table: `history` caps its own read, so it would
    // look bounded even if nothing were pruned.
    let stored: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM campaign_revisions WHERE campaign_id = $1")
            .bind("bounded")
            .fetch_one(&pool)
            .await
            .expect("stored revision count");
    assert_eq!(stored, REVISION_RETENTION);
    let retained = history(&pool, "bounded").await.expect("history");
    assert_eq!(retained.len(), usize::try_from(REVISION_RETENTION).unwrap());
    let newest = writes + 1;
    let oldest_kept = newest - REVISION_RETENTION + 1;
    assert_eq!(retained.first().expect("newest").revision, newest);
    assert_eq!(retained.last().expect("oldest kept").revision, oldest_kept);

    // Pruning must never reach the live checkpoint itself.
    assert_eq!(get(&pool, "bounded").await.expect("live").revision, newest);
    pool.close().await;
    fixture.drop().await;
}
