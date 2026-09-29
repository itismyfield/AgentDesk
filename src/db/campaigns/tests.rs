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
    // Pinned to the literal: the assertions above are expressed in terms of the
    // constant, so raising it would otherwise reintroduce unbounded growth green.
    assert_eq!(REVISION_RETENTION, 10);
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

#[tokio::test]
async fn postgres_campaign_revision_prune_keeps_newest_by_rank_across_gaps_pg() {
    let fixture = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = fixture.connect_and_migrate().await;
    create(&pool, "gapped".into(), input())
        .await
        .expect("create");
    let live = REVISION_RETENTION * 4;
    for revision in 1..live {
        replace(&pool, "gapped", revision, input())
            .await
            .expect("replace");
    }

    // Leave only the top two, then refill with sparse low revisions. Numbering is
    // now far from contiguous, which is the case an offset-from-MAX prune gets wrong.
    sqlx::query("DELETE FROM campaign_revisions WHERE campaign_id = $1 AND revision < $2")
        .bind("gapped")
        .bind(live - 1)
        .execute(&pool)
        .await
        .expect("clear the dense tail");
    let sparse: Vec<i64> = (2..=18).step_by(2).collect();
    for revision in &sparse {
        sqlx::query(
            "INSERT INTO campaign_revisions (campaign_id, revision, document) \
             SELECT campaign_id, $2, document FROM campaign_revisions \
             WHERE campaign_id = $1 AND revision = $3",
        )
        .bind("gapped")
        .bind(revision)
        .bind(live)
        .execute(&pool)
        .await
        .expect("insert a sparse revision");
    }

    replace(&pool, "gapped", live, input())
        .await
        .expect("replace across gaps");

    let kept: Vec<i64> = sqlx::query_scalar(
        "SELECT revision FROM campaign_revisions WHERE campaign_id = $1 ORDER BY revision DESC",
    )
    .bind("gapped")
    .fetch_all(&pool)
    .await
    .expect("kept revisions");
    let mut expected: Vec<i64> = vec![live + 1, live, live - 1];
    expected.extend(
        sparse
            .iter()
            .rev()
            .take(usize::try_from(REVISION_RETENTION).unwrap() - 3),
    );
    assert_eq!(
        kept, expected,
        "prune must keep the newest by rank, not by offset from MAX"
    );
    pool.close().await;
    fixture.drop().await;
}

#[test]
fn legacy_node_documents_load_without_glance_fields_and_keep_their_time() {
    let mut first = checkpoint("glance".into(), input(), None);
    let old = Utc::now() - chrono::Duration::days(1);
    first.nodes[1].updated_at = old;
    // Documents stored before summary/benefit existed carry neither key.
    let mut document = serde_json::to_value(&first).unwrap();
    for node in document["nodes"].as_array_mut().unwrap() {
        let node = node.as_object_mut().unwrap();
        node.remove("summary");
        node.remove("benefit");
    }
    let restored: Campaign = serde_json::from_value(document).unwrap();
    assert!(
        restored
            .nodes
            .iter()
            .all(|node| node.input.summary.is_none() && node.input.benefit.is_none())
    );
    let serialized = serde_json::to_value(&restored).unwrap();
    assert!(
        serialized["nodes"][1]["summary"].is_null() && serialized["nodes"][1]["benefit"].is_null()
    );

    // An old writer resubmitting unchanged content keeps the node time; adding a gist is a change.
    let unchanged = checkpoint("glance".into(), input(), Some(&restored));
    assert_eq!(unchanged.nodes[1].updated_at, old);
    let mut next = input();
    next.nodes[1].summary = Some("Resume work without rereading logs".into());
    next.nodes[1].benefit = Some("No lost progress after a restart".into());
    let described = checkpoint("glance".into(), next, Some(&restored));
    assert!(described.nodes[1].updated_at > old);
    let round_trip: Campaign =
        serde_json::from_value(serde_json::to_value(&described).unwrap()).unwrap();
    assert_eq!(
        round_trip.nodes[1].input.benefit.as_deref(),
        Some("No lost progress after a restart")
    );
}

#[tokio::test]
async fn postgres_live_status_reads_the_issue_card_without_touching_the_ledger_pg() {
    let fixture = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = fixture.connect_and_migrate().await;
    for statement in [
        "INSERT INTO kanban_cards (id, title, status, repo_id, github_issue_number)
         VALUES ('card-7', 'Seven', 'in_progress', 'Owner/Repo', 7)",
        "INSERT INTO task_dispatches (id, kanban_card_id, dispatch_type, status, created_at)
         VALUES ('d-old', 'card-7', 'implementation', 'completed', NOW() - INTERVAL '1 hour'),
                ('d-new', 'card-7', 'review', 'dispatched', NOW())",
        "INSERT INTO auto_queue_runs (id, repo, agent_id, status)
         VALUES ('run-1', 'Owner/Repo', 'agent-1', 'active')",
        "INSERT INTO auto_queue_entries (id, run_id, kanban_card_id, status)
         VALUES ('entry-1', 'run-1', 'card-7', 'dispatched')",
        "INSERT INTO sessions (session_key, status, active_dispatch_id, last_heartbeat)
         VALUES ('session-7', 'turn_active', 'd-new', NOW())",
    ] {
        sqlx::query(statement)
            .execute(&pool)
            .await
            .expect("seed live status");
    }
    let mut document = input();
    document.nodes[0].issue_url = Some("https://github.com/owner/repo/issues/7".into());
    document.nodes[1].issue_url = Some("https://github.com/owner/repo/issues/8".into());
    let campaign = create(&pool, "live".into(), document)
        .await
        .expect("create");

    let live = live_status(&pool, std::slice::from_ref(&campaign))
        .await
        .expect("live status");
    let nodes = &live["live"];
    assert_eq!(nodes.len(), 1, "issue 8 has no card");
    let status = &nodes["implement"];
    let session_id: String =
        sqlx::query_scalar("SELECT id::TEXT FROM sessions WHERE session_key = 'session-7'")
            .fetch_one(&pool)
            .await
            .expect("session id");
    assert_eq!(
        NodeLiveStatus {
            session_seen_at: None,
            working_session_seen_at: None,
            ..status.clone()
        },
        NodeLiveStatus {
            card_id: "card-7".into(),
            card_status: "in_progress".into(),
            dispatch_id: Some("d-new".into()),
            dispatch_type: Some("review".into()),
            dispatch_status: Some("dispatched".into()),
            session_status: Some("turn_active".into()),
            session_seen_at: None,
            working_dispatch_id: Some("d-new".into()),
            working_dispatch_type: Some("review".into()),
            working_session_id: Some(session_id),
            working_session_status: Some("turn_active".into()),
            working_session_seen_at: None,
            running: true,
            queue_status: Some("dispatched".into()),
        }
    );
    assert!(status.session_seen_at.is_some());
    assert_eq!(status.working_session_seen_at, status.session_seen_at);

    // A dispatched row whose session went quiet is not running work.
    sqlx::query("UPDATE sessions SET last_heartbeat = NOW() - INTERVAL '1 hour'")
        .execute(&pool)
        .await
        .expect("age heartbeat");
    let live = live_status(&pool, std::slice::from_ref(&campaign))
        .await
        .expect("live status");
    assert!(!live["live"]["implement"].running);
    assert_eq!(
        get(&pool, "live").await.expect("ledger").revision,
        campaign.revision
    );
    pool.close().await;
    fixture.drop().await;
}

async fn assert_live_status_keeps_older_worker(sidecar_status: &str, working_session_status: &str) {
    let fixture = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = fixture.connect_and_migrate().await;
    for statement in [
        "INSERT INTO kanban_cards (id, title, status, repo_id, github_issue_number)
         VALUES ('card-7', 'Seven', 'in_progress', 'Owner/Repo', 7)",
        "INSERT INTO task_dispatches (id, kanban_card_id, dispatch_type, status, created_at)
         VALUES ('d-working', 'card-7', 'implementation', 'dispatched', NOW() - INTERVAL '1 hour'),
                ('d-stale', 'card-7', 'review', 'dispatched', NOW() - INTERVAL '30 minutes'),
                ('d-unseen', 'card-7', 'review-decision', 'dispatched', NOW() - INTERVAL '20 minutes')",
        "INSERT INTO sessions (session_key, status, active_dispatch_id, last_heartbeat)
         VALUES ('session-stale', 'turn_active', 'd-stale', NOW() - INTERVAL '1 hour'),
                ('session-unseen', 'turn_active', 'd-unseen', NULL)",
    ] {
        sqlx::query(statement)
            .execute(&pool)
            .await
            .expect("seed older worker");
    }
    sqlx::query(
        "INSERT INTO task_dispatches (id, kanban_card_id, dispatch_type, status, created_at)
         VALUES ('d-sidecar', 'card-7', 'consultation', $1, NOW())",
    )
    .bind(sidecar_status)
    .execute(&pool)
    .await
    .expect("seed latest sidecar");
    let working_session_id: String = sqlx::query_scalar(
        "INSERT INTO sessions (session_key, status, active_dispatch_id, last_heartbeat)
         VALUES ('session-working', $1, 'd-working', NOW() - INTERVAL '1 second')
         RETURNING id::TEXT",
    )
    .bind(working_session_status)
    .fetch_one(&pool)
    .await
    .expect("seed working session");
    // A fresh session cannot make a pending or completed sidecar count as work.
    sqlx::query(
        "INSERT INTO sessions (session_key, status, active_dispatch_id, last_heartbeat)
         VALUES ('session-sidecar', 'turn_active', 'd-sidecar', NOW()),
                ('session-idle', 'idle', 'd-working', NOW())",
    )
    .execute(&pool)
    .await
    .expect("seed sidecar session");
    let mut document = input();
    document.nodes[0].status = NodeStatus::Pending;
    document.nodes[0].issue_url = Some("https://github.com/owner/repo/issues/7".into());
    let campaign = create(&pool, "live-sidecar".into(), document)
        .await
        .expect("create");
    assert_eq!(campaign.nodes[0].input.status, NodeStatus::Pending);

    let live = live_status(&pool, std::slice::from_ref(&campaign))
        .await
        .expect("live status with sidecar");
    let status = &live["live-sidecar"]["implement"];
    assert!(status.running);
    assert_eq!(status.dispatch_id.as_deref(), Some("d-sidecar"));
    assert_eq!(status.dispatch_type.as_deref(), Some("consultation"));
    assert_eq!(status.dispatch_status.as_deref(), Some(sidecar_status));
    assert_eq!(status.session_status.as_deref(), Some("turn_active"));
    assert!(status.session_seen_at.is_some());
    assert_eq!(status.working_dispatch_id.as_deref(), Some("d-working"));
    assert_eq!(
        status.working_dispatch_type.as_deref(),
        Some("implementation")
    );
    assert_eq!(
        status.working_session_id.as_deref(),
        Some(working_session_id.as_str())
    );
    assert_eq!(
        status.working_session_status.as_deref(),
        Some(working_session_status)
    );
    assert!(status.working_session_seen_at.is_some());

    // Once the only working session goes stale, the other rows must not keep it running.
    sqlx::query(
        "UPDATE sessions SET last_heartbeat = NOW() - INTERVAL '1 hour'
         WHERE session_key = 'session-working'",
    )
    .execute(&pool)
    .await
    .expect("age working heartbeat");
    let live = live_status(&pool, std::slice::from_ref(&campaign))
        .await
        .expect("live status without a worker");
    let status = &live["live-sidecar"]["implement"];
    assert!(!status.running);
    assert_eq!(status.dispatch_id.as_deref(), Some("d-sidecar"));
    assert_eq!(status.dispatch_status.as_deref(), Some(sidecar_status));
    assert_eq!(status.working_dispatch_id, None);
    assert_eq!(status.working_dispatch_type, None);
    assert_eq!(status.working_session_id, None);
    assert_eq!(status.working_session_status, None);
    assert_eq!(status.working_session_seen_at, None);
    assert_eq!(
        serde_json::to_value(get(&pool, "live-sidecar").await.expect("ledger")).unwrap(),
        serde_json::to_value(&campaign).unwrap()
    );
    assert_eq!(
        history(&pool, "live-sidecar").await.expect("history").len(),
        1
    );
    pool.close().await;
    fixture.drop().await;
}

#[tokio::test]
async fn postgres_live_status_keeps_older_worker_with_latest_pending_sidecar_pg() {
    assert_live_status_keeps_older_worker("pending", "turn_active").await;
}

#[tokio::test]
async fn postgres_live_status_keeps_older_worker_with_latest_completed_sidecar_pg() {
    assert_live_status_keeps_older_worker("completed", "awaiting_bg").await;
}
