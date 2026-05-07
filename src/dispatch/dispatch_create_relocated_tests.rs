use super::create_dispatch_record_sqlite_test as create_dispatch_record_test;
use super::*;
use crate::dispatch::test_support::*;
use crate::dispatch::{DispatchCreateOptions, complete_dispatch};
use serde_json::json;

#[test]
fn create_dispatch_inserts_and_updates_card() {
    let db = test_db();
    let engine = test_engine(&db);
    seed_card(&db, "card-1", "ready");

    let dispatch = create_dispatch(
        &db,
        &engine,
        "card-1",
        "agent-1",
        "implementation",
        "Do the thing",
        &json!({"key": "value"}),
    )
    .unwrap();

    assert_eq!(dispatch["status"], "pending");
    assert_eq!(dispatch["kanban_card_id"], "card-1");
    assert_eq!(dispatch["to_agent_id"], "agent-1");
    assert_eq!(dispatch["dispatch_type"], "implementation");
    assert_eq!(dispatch["title"], "Do the thing");

    // Card should be updated — #255: ready→requested is free, so kickoff_for("ready")
    // falls back to first dispatchable state target = "in_progress"
    let conn = db.separate_conn().unwrap();
    let (card_status, latest_dispatch_id): (String, String) = conn
        .query_row(
            "SELECT status, latest_dispatch_id FROM kanban_cards WHERE id = 'card-1'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(card_status, "in_progress");
    assert_eq!(latest_dispatch_id, dispatch["id"].as_str().unwrap());
}

#[test]
fn create_dispatch_for_nonexistent_card_fails() {
    let db = test_db();
    let engine = test_engine(&db);

    let result = create_dispatch(
        &db,
        &engine,
        "nonexistent",
        "agent-1",
        "implementation",
        "title",
        &json!({}),
    );
    assert!(result.is_err());
}

#[test]
fn create_dispatch_records_origin_main_baseline_commit() {
    let db = test_db();
    let engine = test_engine(&db);
    seed_card(&db, "card-baseline", "ready");
    let (repo, _origin, _override_guard) = setup_test_repo_with_origin();
    let repo_dir = repo.path().to_str().unwrap();

    run_git(
        repo_dir,
        &["commit", "--allow-empty", "-m", "local main only"],
    );
    let expected_baseline =
        crate::services::platform::shell::git_dispatch_baseline_commit(repo_dir)
            .expect("origin/main baseline");

    let dispatch = create_dispatch(
        &db,
        &engine,
        "card-baseline",
        "agent-1",
        "implementation",
        "title",
        &json!({}),
    )
    .unwrap();

    assert_eq!(
        dispatch["context"]["baseline_commit"].as_str(),
        Some(expected_baseline.as_str())
    );
}

#[test]
fn create_review_dispatch_for_done_card_rejected() {
    let db = test_db();
    let engine = test_engine(&db);
    seed_card(&db, "card-done", "done");

    for dispatch_type in &["review", "review-decision", "rework"] {
        let result = create_dispatch(
            &db,
            &engine,
            "card-done",
            "agent-1",
            dispatch_type,
            "Should fail",
            &json!({}),
        );
        assert!(
            result.is_err(),
            "{} dispatch should not be created for done card",
            dispatch_type
        );
    }

    // All dispatch types for done cards should be rejected
    let result = create_dispatch(
        &db,
        &engine,
        "card-done",
        "agent-1",
        "implementation",
        "Reopen work",
        &json!({}),
    );
    assert!(
        result.is_err(),
        "implementation dispatch should be rejected for done card"
    );
}

#[test]
fn create_sidecar_phase_gate_for_terminal_card_preserves_card_state() {
    let db = test_db();
    let engine = test_engine(&db);
    seed_card(&db, "card-phase-gate", "done");

    let dispatch = create_dispatch(
        &db,
        &engine,
        "card-phase-gate",
        "agent-1",
        "phase-gate",
        "Phase Gate",
        &json!({
            "phase_gate": {
                "run_id": "run-sidecar",
                "batch_phase": 2,
                "pass_verdict": "phase_gate_passed",
            }
        }),
    )
    .expect("phase gate sidecar dispatch should be allowed for terminal cards");

    let dispatch_id = dispatch["id"].as_str().unwrap().to_string();
    let conn = db.separate_conn().unwrap();
    let (card_status, latest_dispatch_id): (String, Option<String>) = conn
        .query_row(
            "SELECT status, latest_dispatch_id FROM kanban_cards WHERE id = 'card-phase-gate'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(card_status, "done");
    assert!(
        latest_dispatch_id.is_none(),
        "sidecar phase gate must not replace latest_dispatch_id"
    );
    assert_eq!(
        count_notify_outbox(&conn, &dispatch_id),
        1,
        "sidecar phase gate must still enqueue a notify outbox row"
    );
    assert_eq!(
        load_dispatch_events(&conn, &dispatch_id),
        vec![(None, "pending".to_string(), "create_dispatch".to_string())],
        "sidecar dispatch creation should still be audited"
    );
}

#[test]
fn create_sidecar_phase_gate_skips_repo_lookup_without_explicit_worktree() {
    let db = test_db();
    let engine = test_engine(&db);
    seed_card(&db, "card-phase-gate-repo", "done");
    set_card_issue_number(&db, "card-phase-gate-repo", 685);
    set_card_repo_id(&db, "card-phase-gate-repo", "test/repo");

    let dispatch = create_dispatch(
        &db,
        &engine,
        "card-phase-gate-repo",
        "agent-1",
        "phase-gate",
        "Phase Gate Repo",
        &json!({
            "phase_gate": {
                "run_id": "run-sidecar-repo",
                "batch_phase": 0,
                "pass_verdict": "phase_gate_passed"
            }
        }),
    )
    .expect("phase gate sidecar should not require repo_dirs mapping");

    let dispatch_id = dispatch["id"].as_str().unwrap().to_string();
    let conn = db.separate_conn().unwrap();
    let context: String = conn
        .query_row(
            "SELECT context FROM task_dispatches WHERE id = ?1",
            [&dispatch_id],
            |row| row.get(0),
        )
        .unwrap();
    let context_json: serde_json::Value = serde_json::from_str(&context).unwrap();
    assert!(
        context_json.get("worktree_path").is_none(),
        "phase gate sidecar should not synthesize a repo-derived worktree path"
    );
    assert_eq!(
        context_json["phase_gate"]["run_id"], "run-sidecar-repo",
        "phase gate payload must remain intact"
    );
}

#[test]
fn concurrent_dispatches_for_different_cards_have_distinct_ids() {
    // Regression: concurrent dispatches from different cards must not share
    // dispatch IDs or card state — each must be independently routable.
    let db = test_db();
    let engine = test_engine(&db);
    seed_card(&db, "card-a", "ready");
    seed_card(&db, "card-b", "ready");

    let dispatch_a = create_dispatch(
        &db,
        &engine,
        "card-a",
        "agent-1",
        "implementation",
        "Task A",
        &json!({}),
    )
    .unwrap();

    let dispatch_b = create_dispatch(
        &db,
        &engine,
        "card-b",
        "agent-2",
        "implementation",
        "Task B",
        &json!({}),
    )
    .unwrap();

    let id_a = dispatch_a["id"].as_str().unwrap();
    let id_b = dispatch_b["id"].as_str().unwrap();
    assert_ne!(id_a, id_b, "dispatch IDs must be unique");
    assert_eq!(dispatch_a["kanban_card_id"], "card-a");
    assert_eq!(dispatch_b["kanban_card_id"], "card-b");

    // Each card's latest_dispatch_id points to its own dispatch
    let conn = db.separate_conn().unwrap();
    let latest_a: String = conn
        .query_row(
            "SELECT latest_dispatch_id FROM kanban_cards WHERE id = 'card-a'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let latest_b: String = conn
        .query_row(
            "SELECT latest_dispatch_id FROM kanban_cards WHERE id = 'card-b'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(latest_a, id_a);
    assert_eq!(latest_b, id_b);
    assert_ne!(latest_a, latest_b, "card dispatch IDs must not cross");
}

#[test]
fn dedup_same_card_same_type_returns_existing_dispatch() {
    let db = test_db();
    let engine = test_engine(&db);
    seed_card(&db, "card-dup", "ready");

    let d1 = create_dispatch(
        &db,
        &engine,
        "card-dup",
        "agent-1",
        "implementation",
        "First",
        &json!({}),
    )
    .unwrap();
    let id1 = d1["id"].as_str().unwrap();

    // Second call with same card + same type → should return existing
    let d2 = create_dispatch(
        &db,
        &engine,
        "card-dup",
        "agent-1",
        "implementation",
        "Second",
        &json!({}),
    )
    .unwrap();
    let id2 = d2["id"].as_str().unwrap();

    assert_eq!(id1, id2, "dedup must return existing dispatch_id");
    assert_eq!(d2["status"], "pending");

    // Only 1 row in DB
    let conn = db.separate_conn().unwrap();
    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM task_dispatches \
             WHERE kanban_card_id = 'card-dup' AND dispatch_type = 'implementation' \
             AND status IN ('pending', 'dispatched')",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(count, 1, "only one pending dispatch must exist");
}

#[test]
fn dedup_same_review_card_returns_existing_dispatch() {
    let (_repo, _override_guard) = setup_test_repo();
    let db = test_db();
    let engine = test_engine(&db);
    seed_card(&db, "card-review-dup", "review");

    let d1 = create_dispatch(
        &db,
        &engine,
        "card-review-dup",
        "agent-1",
        "review",
        "First review",
        &json!({}),
    )
    .unwrap();
    let id1 = d1["id"].as_str().unwrap();

    let d2 = create_dispatch(
        &db,
        &engine,
        "card-review-dup",
        "agent-1",
        "review",
        "Second review",
        &json!({}),
    )
    .unwrap();
    let id2 = d2["id"].as_str().unwrap();

    assert_eq!(id1, id2, "review dedup must return existing dispatch_id");
    assert_eq!(d2["status"], "pending");

    let conn = db.separate_conn().unwrap();
    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM task_dispatches \
             WHERE kanban_card_id = 'card-review-dup' AND dispatch_type = 'review' \
             AND status IN ('pending', 'dispatched')",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(count, 1, "only one active review dispatch must exist");
}

#[test]
fn dedup_same_card_different_type_allows_creation() {
    let (_repo, _override_guard) = setup_test_repo();
    let db = test_db();
    let engine = test_engine(&db);
    seed_card(&db, "card-diff", "review");

    // Create review dispatch
    let d1 = create_dispatch(
        &db,
        &engine,
        "card-diff",
        "agent-1",
        "review",
        "Review",
        &json!({}),
    )
    .unwrap();

    // Create review-decision for same card → different type, should succeed
    let d2 = create_dispatch(
        &db,
        &engine,
        "card-diff",
        "agent-1",
        "review-decision",
        "Decision",
        &json!({}),
    )
    .unwrap();

    assert_ne!(
        d1["id"].as_str().unwrap(),
        d2["id"].as_str().unwrap(),
        "different types must create distinct dispatches"
    );
}

#[test]
fn dedup_completed_dispatch_allows_new_creation() {
    let db = test_db();
    let engine = test_engine(&db);
    seed_card(&db, "card-reopen", "ready");

    let d1 = create_dispatch(
        &db,
        &engine,
        "card-reopen",
        "agent-1",
        "implementation",
        "First",
        &json!({}),
    )
    .unwrap();
    let id1 = d1["id"].as_str().unwrap().to_string();

    // Complete the first dispatch
    seed_assistant_response_for_dispatch(&db, &id1, "implemented first attempt");
    complete_dispatch(&db, &engine, &id1, &json!({"output": "done"})).unwrap();

    // New dispatch of same type → should succeed (old one is completed)
    let d2 = create_dispatch(
        &db,
        &engine,
        "card-reopen",
        "agent-1",
        "implementation",
        "Second",
        &json!({}),
    )
    .unwrap();

    assert_ne!(
        id1,
        d2["id"].as_str().unwrap(),
        "completed dispatch must not block new creation"
    );
}

#[test]
fn dedup_core_returns_reused_flag() {
    let db = test_db();
    seed_card(&db, "card-flag", "ready");

    let (id1, _, reused1) = create_dispatch_record_test(
        &db,
        "card-flag",
        "agent-1",
        "implementation",
        "First",
        &json!({}),
        DispatchCreateOptions::default(),
    )
    .unwrap();
    assert!(!reused1, "first creation must not be reused");

    let (id2, _, reused2) = create_dispatch_record_test(
        &db,
        "card-flag",
        "agent-1",
        "implementation",
        "Second",
        &json!({}),
        DispatchCreateOptions::default(),
    )
    .unwrap();
    assert!(reused2, "duplicate must be flagged as reused");
    assert_eq!(id1, id2);

    let conn = db.separate_conn().unwrap();
    assert_eq!(
        count_notify_outbox(&conn, &id1),
        1,
        "reused dispatch must not create a second notify outbox row"
    );
}

#[test]
fn non_review_dispatch_uses_card_repo_mapping_instead_of_default_repo() {
    let default_repo = init_test_repo();
    let default_repo_dir = default_repo.path().to_str().unwrap();
    let default_wt_dir = default_repo.path().join("wt-wrong-414");
    let default_wt_path = default_wt_dir.to_str().unwrap();
    run_git(
        default_repo_dir,
        &["worktree", "add", default_wt_path, "-b", "wt/wrong-414"],
    );
    std::fs::write(default_wt_dir.join("wrong.txt"), "wrong repo\n").unwrap();
    run_git(default_wt_path, &["add", "wrong.txt"]);
    run_git(default_wt_path, &["commit", "-m", "fix: wrong repo (#414)"]);

    let mapped_repo = init_test_repo();
    let mapped_repo_dir = mapped_repo.path().to_str().unwrap();
    let mapped_wt_dir = mapped_repo.path().join("wt-right-414");
    let mapped_wt_path = mapped_wt_dir.to_str().unwrap();
    run_git(
        mapped_repo_dir,
        &["worktree", "add", mapped_wt_path, "-b", "wt/right-414"],
    );
    std::fs::write(mapped_wt_dir.join("right.txt"), "right repo\n").unwrap();
    run_git(mapped_wt_path, &["add", "right.txt"]);
    run_git(mapped_wt_path, &["commit", "-m", "fix: mapped repo (#414)"]);

    let config_dir = write_repo_mapping_config(&[("owner/repo-b", mapped_repo_dir)]);
    let config_path = config_dir.path().join("agentdesk.yaml");
    let _env =
        DispatchEnvOverride::new(Some(default_repo_dir), Some(config_path.to_str().unwrap()));

    let db = test_db();
    let engine = test_engine(&db);
    seed_card(&db, "card-mapped", "ready");
    set_card_issue_number(&db, "card-mapped", 414);
    set_card_repo_id(&db, "card-mapped", "owner/repo-b");

    let dispatch = create_dispatch(
        &db,
        &engine,
        "card-mapped",
        "agent-1",
        "implementation",
        "Impl task",
        &json!({}),
    )
    .unwrap();

    let ctx = &dispatch["context"];
    let actual_wt_path = ctx["worktree_path"].as_str().unwrap();
    let canonical_actual = std::fs::canonicalize(actual_wt_path)
        .unwrap()
        .to_string_lossy()
        .into_owned();
    let canonical_expected = std::fs::canonicalize(mapped_wt_path)
        .unwrap()
        .to_string_lossy()
        .into_owned();
    let canonical_default = std::fs::canonicalize(default_wt_path)
        .unwrap()
        .to_string_lossy()
        .into_owned();

    assert_eq!(canonical_actual, canonical_expected);
    assert_eq!(ctx["worktree_branch"], "wt/right-414");
    assert_ne!(canonical_actual, canonical_default);
}

#[test]
fn create_dispatch_rejects_missing_card_repo_mapping() {
    let default_repo = init_test_repo();
    let default_repo_dir = default_repo.path().to_str().unwrap();
    let _env = DispatchEnvOverride::new(Some(default_repo_dir), None);

    let db = test_db();
    seed_card(&db, "card-missing-mapping", "ready");
    set_card_issue_number(&db, "card-missing-mapping", 515);
    set_card_repo_id(&db, "card-missing-mapping", "owner/missing");

    let err = create_dispatch_record_test(
        &db,
        "card-missing-mapping",
        "agent-1",
        "implementation",
        "Should fail",
        &json!({}),
        DispatchCreateOptions::default(),
    )
    .expect_err("dispatch should fail when repo mapping is missing");

    assert!(
        err.to_string()
            .contains("No local repo mapping for 'owner/missing'"),
        "unexpected error: {err:#}"
    );

    let conn = db.separate_conn().unwrap();
    let dispatch_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM task_dispatches WHERE kanban_card_id = 'card-missing-mapping'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        dispatch_count, 0,
        "missing repo mapping must fail before INSERT"
    );
}

#[test]
fn create_dispatch_uses_explicit_worktree_context_without_repo_mapping() {
    let (repo, _repo_override) = setup_test_repo();
    let repo_dir = repo.path().to_str().unwrap();
    run_git(repo_dir, &["checkout", "-b", "wt/explicit-515"]);

    let db = test_db();
    let engine = test_engine(&db);
    seed_card(&db, "card-explicit-worktree", "review");
    set_card_issue_number(&db, "card-explicit-worktree", 515);
    set_card_repo_id(&db, "card-explicit-worktree", "owner/missing");

    let dispatch = create_dispatch(
        &db,
        &engine,
        "card-explicit-worktree",
        "agent-1",
        "create-pr",
        "Create PR",
        &json!({
            "worktree_path": repo_dir,
            "worktree_branch": "wt/explicit-515",
            "branch": "wt/explicit-515",
        }),
    )
    .expect("explicit worktree context should bypass repo mapping lookup");

    let ctx = &dispatch["context"];
    assert_eq!(ctx["worktree_path"], repo_dir);
    assert_eq!(ctx["worktree_branch"], "wt/explicit-515");
    assert_eq!(ctx["branch"], "wt/explicit-515");
}

#[test]
fn create_dispatch_rejects_target_repo_from_card_description() {
    let default_repo = init_test_repo();
    let default_repo_dir = default_repo.path().to_str().unwrap();
    let _env = DispatchEnvOverride::new(Some(default_repo_dir), None);

    let external_repo = init_test_repo();
    let external_repo_dir = external_repo.path().to_str().unwrap();
    let external_wt_dir = external_repo.path().join("wt-target-627");
    let external_wt_path = external_wt_dir.to_str().unwrap();
    run_git(
        external_repo_dir,
        &["worktree", "add", external_wt_path, "-b", "wt/target-627"],
    );
    git_commit(external_wt_path, "fix: dispatch target repo (#627)");

    let db = test_db();
    let engine = test_engine(&db);
    seed_card(&db, "card-dispatch-target-repo", "ready");
    set_card_issue_number(&db, "card-dispatch-target-repo", 627);
    set_card_repo_id(&db, "card-dispatch-target-repo", "owner/missing");
    set_card_description(
        &db,
        "card-dispatch-target-repo",
        &format!("external repo path: {}", external_repo_dir),
    );

    let err = create_dispatch(
        &db,
        &engine,
        "card-dispatch-target-repo",
        "agent-1",
        "implementation",
        "Implement external repo task",
        &json!({}),
    )
    .expect_err("description target_repo must not bypass missing repo mapping");
    assert!(
        err.to_string()
            .contains("No local repo mapping for 'owner/missing'"),
        "unexpected error: {err:#}"
    );
}

#[test]
fn non_review_dispatch_injects_worktree_context() {
    // When resolve_card_worktree returns None (no issue), the context
    // should pass through unchanged (no worktree_path/worktree_branch).
    let db = test_db();
    seed_card(&db, "card-ctx", "ready");
    let engine = test_engine(&db);

    let dispatch = create_dispatch(
        &db,
        &engine,
        "card-ctx",
        "agent-1",
        "implementation",
        "Impl task",
        &json!({"custom_key": "custom_value"}),
    )
    .unwrap();

    // context is returned as parsed JSON by query_dispatch_row
    let ctx = &dispatch["context"];
    assert_eq!(ctx["custom_key"], "custom_value");
    // No issue number → no worktree injection
    assert!(
        ctx.get("worktree_path").is_none(),
        "no worktree_path without issue"
    );
    assert!(
        ctx.get("worktree_branch").is_none(),
        "no worktree_branch without issue"
    );
}

#[test]
fn create_dispatch_core_review_path_still_fails_closed_on_unrecoverable_external_target_repo() {
    let db = test_db();
    seed_card(&db, "card-review-762-a-core", "review");
    set_card_issue_number(&db, "card-review-762-a-core", 762);

    // Card's canonical repo — carries a LIVE worktree for the same
    // issue. A silent redirect would point the reviewer here.
    let (card_repo, _repo_override) = setup_test_repo();
    let card_repo_dir = card_repo.path().to_str().unwrap();
    set_card_repo_id(&db, "card-review-762-a-core", card_repo_dir);
    let card_live_wt = card_repo.path().join("wt-762-a-core-live");
    let card_live_wt_path = card_live_wt.to_str().unwrap();
    run_git(
        card_repo_dir,
        &[
            "worktree",
            "add",
            "-b",
            "wt/762-a-core-live",
            card_live_wt_path,
        ],
    );
    let _ = git_commit(card_live_wt_path, "feat: unrelated live card work (#762)");

    // External repo where the historical work ran — then deleted to
    // simulate the unrecoverable case.
    let external_repo = tempfile::tempdir().unwrap();
    let external_repo_dir = external_repo.path().to_str().unwrap();
    run_git(external_repo_dir, &["init", "-b", "main"]);
    run_git(
        external_repo_dir,
        &["config", "user.email", "test@test.com"],
    );
    run_git(external_repo_dir, &["config", "user.name", "Test"]);
    run_git(
        external_repo_dir,
        &["commit", "--allow-empty", "-m", "initial"],
    );
    let reviewed_commit = git_commit(
        external_repo_dir,
        "fix: external unrecoverable from core path (#762)",
    );

    let conn = db.separate_conn().unwrap();
    conn.execute(
        "INSERT INTO task_dispatches (
            id, kanban_card_id, to_agent_id, dispatch_type, status, title, context, result, created_at, updated_at
         ) VALUES (
            'dispatch-review-762-a-core', 'card-review-762-a-core', 'agent-1', 'implementation', 'completed',
            'Done', ?1, ?2, datetime('now'), datetime('now')
         )",
        sqlite_test::params![
            serde_json::json!({ "target_repo": external_repo_dir }).to_string(),
            serde_json::json!({
                "completed_worktree_path":
                    external_repo.path().join("wt-762-a-core-deleted"),
                "completed_branch": "wt/762-a-core-deleted",
                "completed_commit": reviewed_commit.clone(),
            })
            .to_string(),
        ],
    )
    .unwrap();
    drop(conn);

    std::fs::remove_dir_all(external_repo_dir).unwrap();

    // Invoke the real production path. The caller passes NO target_repo
    // override — `dispatch_create` will inject `card.repo_id`
    // (`card_repo_dir`) before calling `build_review_context`.
    let (dispatch_id, _, _) = create_dispatch_record_test(
        &db,
        "card-review-762-a-core",
        "agent-1",
        "review",
        "Review dispatch for 762-a",
        &json!({}),
        DispatchCreateOptions::default(),
    )
    .unwrap();

    let conn = db.separate_conn().unwrap();
    let context_str: String = conn
        .query_row(
            "SELECT context FROM task_dispatches WHERE id = ?1",
            [&dispatch_id],
            |row| row.get(0),
        )
        .unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&context_str).unwrap();

    assert_eq!(
        parsed["review_target_reject_reason"], "external_target_repo_unrecoverable",
        "full dispatch_create → build_review_context path must fail closed even though upstream injects card.repo_id as target_repo; got context: {parsed:#?}"
    );
    assert!(
        parsed.get("worktree_path").is_none(),
        "must not silently redirect to card's live worktree"
    );
    assert!(
        parsed.get("reviewed_commit").is_none(),
        "must not emit a reviewed_commit from card scope after rejection"
    );
    assert_eq!(
        parsed["target_repo"], external_repo_dir,
        "historical external target_repo must be preserved (not replaced with card.repo_id)"
    );
}
