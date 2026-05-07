use super::build_review_context_sqlite_test as build_review_context;
use super::resolve_card_worktree_sqlite_test as resolve_card_worktree;
use super::*;
use crate::dispatch::test_support::*;
use crate::services::git::GitCommand;
use serde_json::json;

#[test]
fn dispatch_type_force_new_session_defaults_split_by_dispatch_type() {
    assert_eq!(
        dispatch_type_force_new_session_default(Some("implementation")),
        Some(true)
    );
    assert_eq!(
        dispatch_type_force_new_session_default(Some("review")),
        Some(true)
    );
    assert_eq!(
        dispatch_type_force_new_session_default(Some("rework")),
        Some(true)
    );
    assert_eq!(
        dispatch_type_force_new_session_default(Some("review-decision")),
        Some(false)
    );
    assert_eq!(
        dispatch_type_force_new_session_default(Some("consultation")),
        None
    );
    assert_eq!(dispatch_type_force_new_session_default(None), None);
}

#[test]
fn dispatch_type_session_strategy_defaults_split_by_dispatch_type() {
    assert_eq!(
        dispatch_type_session_strategy_default(Some("implementation")),
        Some(DispatchSessionStrategy {
            reset_provider_state: true,
            recreate_tmux: false,
        })
    );
    assert_eq!(
        dispatch_type_session_strategy_default(Some("review")),
        Some(DispatchSessionStrategy {
            reset_provider_state: true,
            recreate_tmux: false,
        })
    );
    assert_eq!(
        dispatch_type_session_strategy_default(Some("rework")),
        Some(DispatchSessionStrategy {
            reset_provider_state: true,
            recreate_tmux: false,
        })
    );
    assert_eq!(
        dispatch_type_session_strategy_default(Some("review-decision")),
        Some(DispatchSessionStrategy::default())
    );
    assert_eq!(
        dispatch_type_session_strategy_default(Some("consultation")),
        None
    );
    assert_eq!(dispatch_type_session_strategy_default(None), None);
}

#[test]
fn dispatch_type_thread_routing_includes_phase_gate() {
    assert!(dispatch_type_uses_thread_routing(Some("implementation")));
    assert!(dispatch_type_uses_thread_routing(Some("review")));
    assert!(dispatch_type_uses_thread_routing(Some("rework")));
    assert!(dispatch_type_uses_thread_routing(Some("phase-gate")));
    assert!(dispatch_type_uses_thread_routing(Some("review-decision")));
    assert!(dispatch_type_uses_thread_routing(None));
}

#[test]
fn resolve_card_worktree_returns_none_without_issue_number() {
    let db = test_db();
    seed_card(&db, "card-no-issue", "ready");
    // Card has no github_issue_number → should return None
    let result = resolve_card_worktree(&db, "card-no-issue", None).unwrap();
    assert!(
        result.is_none(),
        "card without issue number should return None"
    );
}

#[test]
fn resolve_card_worktree_ignores_target_repo_from_card_description() {
    let default_repo = init_test_repo();
    let default_repo_dir = default_repo.path().to_str().unwrap();
    let _env = DispatchEnvOverride::new(Some(default_repo_dir), None);

    let external_repo = init_test_repo();
    let external_repo_dir = external_repo.path().to_str().unwrap();
    let external_wt_dir = external_repo.path().join("wt-external-627");
    let external_wt_path = external_wt_dir.to_str().unwrap();
    run_git(
        external_repo_dir,
        &["worktree", "add", external_wt_path, "-b", "wt/external-627"],
    );
    git_commit(external_wt_path, "fix: external target repo (#627)");

    let db = test_db();
    seed_card(&db, "card-desc-target-repo", "ready");
    set_card_issue_number(&db, "card-desc-target-repo", 627);
    set_card_repo_id(&db, "card-desc-target-repo", "owner/missing");
    set_card_description(
        &db,
        "card-desc-target-repo",
        &format!("target_repo: {}", external_repo_dir),
    );

    let err = resolve_card_worktree(&db, "card-desc-target-repo", None)
        .expect_err("description target_repo must not bypass missing repo mapping");
    assert!(
        err.to_string()
            .contains("No local repo mapping for 'owner/missing'"),
        "unexpected error: {err:#}"
    );
}

#[test]
fn review_context_reuses_latest_completed_work_dispatch_target() {
    let db = test_db();
    seed_card(&db, "card-review-target", "review");

    // #682: Use a dedicated test repo instead of resolve_repo_dir() to
    // avoid picking up another test's leaked RepoDirOverride (a tempdir
    // that may have been dropped, which would fail the new exact-HEAD
    // check in refresh_review_target_worktree). The test is exercising
    // the "recorded worktree still exists with matching HEAD" reuse path.
    let (repo, _repo_override) = setup_test_repo();
    let repo_dir = repo.path().to_str().unwrap().to_string();
    let completed_commit = crate::services::platform::git_head_commit(&repo_dir).unwrap();
    let completed_branch = crate::services::platform::shell::git_branch_name(&repo_dir);

    let conn = db.separate_conn().unwrap();
    conn.execute(
        "INSERT INTO task_dispatches (
            id, kanban_card_id, to_agent_id, dispatch_type, status, title, context, result, created_at, updated_at
         ) VALUES (
            'dispatch-review-target', 'card-review-target', 'agent-1', 'implementation', 'completed',
            'Done', ?1, ?2, datetime('now'), datetime('now')
         )",
        sqlite_test::params![
            serde_json::json!({}).to_string(),
            serde_json::json!({
                "completed_worktree_path": repo_dir.clone(),
                "completed_branch": completed_branch.clone(),
                "completed_commit": completed_commit.clone(),
            })
            .to_string(),
        ],
    )
    .unwrap();
    drop(conn);

    let context = build_review_context(
        &db,
        "card-review-target",
        "agent-1",
        &json!({}),
        ReviewTargetTrust::Untrusted,
        TargetRepoSource::CardScopeDefault,
    )
    .unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&context).unwrap();

    assert_eq!(parsed["reviewed_commit"], completed_commit);
    assert_eq!(parsed["worktree_path"], repo_dir);
    if let Some(branch) = completed_branch {
        assert_eq!(parsed["branch"], branch);
    }
}

#[test]
fn review_context_refreshes_deleted_completed_worktree_to_active_issue_worktree() {
    let db = test_db();
    seed_card(&db, "card-review-stale-worktree", "review");
    set_card_issue_number(&db, "card-review-stale-worktree", 682);

    let (repo, _repo_override) = setup_test_repo();
    let repo_dir = repo.path().to_str().unwrap();
    let stale_wt_dir = repo.path().join("wt-682-stale");
    let stale_wt_path = stale_wt_dir.to_str().unwrap();

    run_git(
        repo_dir,
        &["worktree", "add", "-b", "wt/682-stale", stale_wt_path],
    );
    let reviewed_commit = git_commit(stale_wt_path, "fix: stale review target (#682)");
    run_git(repo_dir, &["worktree", "remove", "--force", stale_wt_path]);
    run_git(repo_dir, &["branch", "-D", "wt/682-stale"]);

    let live_wt_dir = repo.path().join("wt-682-live");
    let live_wt_path = live_wt_dir.to_str().unwrap();
    run_git(repo_dir, &["branch", "wt/682-live", &reviewed_commit]);
    run_git(repo_dir, &["worktree", "add", live_wt_path, "wt/682-live"]);

    let conn = db.separate_conn().unwrap();
    conn.execute(
        "INSERT INTO task_dispatches (
            id, kanban_card_id, to_agent_id, dispatch_type, status, title, context, result, created_at, updated_at
         ) VALUES (
            'dispatch-review-stale-worktree', 'card-review-stale-worktree', 'agent-1', 'implementation', 'completed',
            'Done', ?1, ?2, datetime('now'), datetime('now')
         )",
        sqlite_test::params![
            serde_json::json!({}).to_string(),
            serde_json::json!({
                "completed_worktree_path": stale_wt_path,
                "completed_branch": "wt/682-stale",
                "completed_commit": reviewed_commit.clone(),
            })
            .to_string(),
        ],
    )
    .unwrap();
    drop(conn);

    let context = build_review_context(
        &db,
        "card-review-stale-worktree",
        "agent-1",
        &json!({}),
        ReviewTargetTrust::Untrusted,
        TargetRepoSource::CardScopeDefault,
    )
    .unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&context).unwrap();
    let actual_worktree = std::fs::canonicalize(parsed["worktree_path"].as_str().unwrap())
        .unwrap()
        .to_string_lossy()
        .into_owned();
    let expected_worktree = std::fs::canonicalize(live_wt_path)
        .unwrap()
        .to_string_lossy()
        .into_owned();

    assert_eq!(parsed["reviewed_commit"], reviewed_commit);
    assert_eq!(actual_worktree, expected_worktree);
    assert_eq!(parsed["branch"], "wt/682-live");
}

#[test]
fn review_context_falls_back_to_repo_dir_when_completed_worktree_was_deleted() {
    let db = test_db();
    seed_card(&db, "card-review-stale-repo", "review");
    set_card_issue_number(&db, "card-review-stale-repo", 683);

    let (repo, _repo_override) = setup_test_repo();
    let repo_dir = repo.path().to_str().unwrap();
    let reviewed_commit = git_commit(repo_dir, "fix: repo fallback review target (#683)");
    let stale_wt_path = repo.path().join("wt-683-missing");

    let conn = db.separate_conn().unwrap();
    conn.execute(
        "INSERT INTO task_dispatches (
            id, kanban_card_id, to_agent_id, dispatch_type, status, title, context, result, created_at, updated_at
         ) VALUES (
            'dispatch-review-stale-repo', 'card-review-stale-repo', 'agent-1', 'implementation', 'completed',
            'Done', ?1, ?2, datetime('now'), datetime('now')
         )",
        sqlite_test::params![
            serde_json::json!({}).to_string(),
            serde_json::json!({
                "completed_worktree_path": stale_wt_path,
                "completed_branch": "wt/683-missing",
                "completed_commit": reviewed_commit.clone(),
            })
            .to_string(),
        ],
    )
    .unwrap();
    drop(conn);

    let context = build_review_context(
        &db,
        "card-review-stale-repo",
        "agent-1",
        &json!({}),
        ReviewTargetTrust::Untrusted,
        TargetRepoSource::CardScopeDefault,
    )
    .unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&context).unwrap();

    assert_eq!(parsed["reviewed_commit"], reviewed_commit);
    assert_eq!(parsed["worktree_path"], repo_dir);
    assert_eq!(parsed["branch"], "main");
}

/// #682: An issue-less card (no github_issue_number) whose completed-work
/// dispatch points at a worktree that has since been cleaned up must NOT
/// leak the stale path into the review dispatch context. The refresh path
/// should fall back to the card's repo_dir when the reviewed commit still
/// lives there — matching the behavior already covered for issue-bearing
/// cards (see review_context_falls_back_to_repo_dir_when_completed_worktree_was_deleted).
///
/// Regression guard for the kunkunGames port (commit bad35a191) which
/// bypassed refresh_review_target_worktree for issue-less cards and
/// returned the recorded (stale) target unchanged.

#[test]
fn review_context_refreshes_stale_worktree_for_issueless_card() {
    let db = test_db();
    seed_card(&db, "card-review-no-issue", "review");
    // Deliberately do NOT set_card_issue_number — this is the edge case.

    let (repo, _repo_override) = setup_test_repo();
    let repo_dir = repo.path().to_str().unwrap();
    let reviewed_commit = git_commit(repo_dir, "fix: issueless repo fallback (#682)");
    let stale_wt_path = repo.path().join("wt-682-deleted-no-issue");

    let conn = db.separate_conn().unwrap();
    conn.execute(
        "INSERT INTO task_dispatches (
            id, kanban_card_id, to_agent_id, dispatch_type, status, title, context, result, created_at, updated_at
         ) VALUES (
            'dispatch-review-no-issue', 'card-review-no-issue', 'agent-1', 'implementation', 'completed',
            'Done', ?1, ?2, datetime('now'), datetime('now')
         )",
        sqlite_test::params![
            serde_json::json!({}).to_string(),
            serde_json::json!({
                "completed_worktree_path": stale_wt_path,
                "completed_branch": "wt/682-deleted-no-issue",
                "completed_commit": reviewed_commit.clone(),
            })
            .to_string(),
        ],
    )
    .unwrap();
    drop(conn);

    let context = build_review_context(
        &db,
        "card-review-no-issue",
        "agent-1",
        &json!({}),
        ReviewTargetTrust::Untrusted,
        TargetRepoSource::CardScopeDefault,
    )
    .unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&context).unwrap();

    assert_eq!(parsed["reviewed_commit"], reviewed_commit);
    // Must NOT be the stale path — refresh should have dropped it in favor
    // of the repo_dir fallback (where the reviewed_commit lives).
    assert_ne!(
        parsed["worktree_path"].as_str(),
        Some(stale_wt_path.to_str().unwrap()),
        "issue-less card must not propagate stale worktree_path into review context"
    );
    assert_eq!(parsed["worktree_path"], repo_dir);
}

/// #682 (Codex review, [high]): An issue-less card whose completed-work
/// dispatch recorded a `target_repo` pointing at an external repo must
/// recover via that repo (not the card-scoped default) when its worktree
/// is cleaned up. Prior refresh logic consulted only card-scoped repo
/// resolution, so issue-less external-repo runs would lose their
/// reviewed_commit after stale-worktree cleanup.

#[test]
fn review_context_refreshes_stale_worktree_for_issueless_card_via_target_repo() {
    let db = test_db();
    seed_card(&db, "card-review-no-issue-tr", "review");

    // Two repos: the default (setup_test_repo) repo and a separate
    // "external" repo that holds the reviewed commit. We deliberately do
    // NOT commit the reviewed commit into the default repo so that the
    // card-scoped fallback can't find it — only the target_repo path can.
    let (_default_repo, _repo_override) = setup_test_repo();
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
    let reviewed_commit = git_commit(external_repo_dir, "fix: external repo review target (#682)");
    let stale_wt_path = external_repo.path().join("wt-682-external-deleted");

    let conn = db.separate_conn().unwrap();
    conn.execute(
        "INSERT INTO task_dispatches (
            id, kanban_card_id, to_agent_id, dispatch_type, status, title, context, result, created_at, updated_at
         ) VALUES (
            'dispatch-review-no-issue-tr', 'card-review-no-issue-tr', 'agent-1', 'implementation', 'completed',
            'Done', ?1, ?2, datetime('now'), datetime('now')
         )",
        sqlite_test::params![
            serde_json::json!({ "target_repo": external_repo_dir }).to_string(),
            serde_json::json!({
                "completed_worktree_path": stale_wt_path,
                "completed_branch": "wt/682-external-deleted",
                "completed_commit": reviewed_commit.clone(),
            })
            .to_string(),
        ],
    )
    .unwrap();
    drop(conn);

    let context = build_review_context(
        &db,
        "card-review-no-issue-tr",
        "agent-1",
        &json!({}),
        ReviewTargetTrust::Untrusted,
        TargetRepoSource::CardScopeDefault,
    )
    .unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&context).unwrap();

    assert_eq!(parsed["reviewed_commit"], reviewed_commit);
    // Must resolve via target_repo, not the card-scoped default.
    // Compare after canonicalization — macOS canonicalizes /var/folders
    // temp dirs to /private/var/folders.
    let actual_wt = parsed["worktree_path"].as_str().unwrap();
    let canonical_external = std::fs::canonicalize(external_repo_dir)
        .unwrap()
        .to_string_lossy()
        .into_owned();
    let canonical_actual = std::fs::canonicalize(actual_wt)
        .unwrap()
        .to_string_lossy()
        .into_owned();
    assert_eq!(canonical_actual, canonical_external);
}

/// #682 (Codex review, [medium]): If the recorded worktree_path still
/// exists as a directory but has been recycled for a different checkout
/// (so it no longer contains the reviewed_commit), refresh must drop it
/// and fall through to recovery. Prior code accepted any existing
/// directory without verifying the commit.

#[test]
fn review_context_drops_recycled_worktree_path_without_reviewed_commit() {
    let db = test_db();
    seed_card(&db, "card-review-recycled-wt", "review");
    set_card_issue_number(&db, "card-review-recycled-wt", 684);

    let (repo, _repo_override) = setup_test_repo();
    let repo_dir = repo.path().to_str().unwrap();

    // Build the "recycled" worktree path: it exists as a directory but
    // tracks an unrelated branch (no reviewed_commit reachable from it).
    let recycled_wt_dir = repo.path().join("wt-684-recycled");
    let recycled_wt_path = recycled_wt_dir.to_str().unwrap();
    run_git(
        repo_dir,
        &["worktree", "add", "-b", "wt/684-recycled", recycled_wt_path],
    );
    let _unrelated_commit = git_commit(recycled_wt_path, "feat: unrelated recycled tree work");

    // The reviewed commit for *our* card only lives on the main repo dir
    // (not in the recycled worktree's branch).
    let reviewed_commit = git_commit(
        repo_dir,
        "fix: reviewed commit not in recycled worktree (#684)",
    );

    let conn = db.separate_conn().unwrap();
    conn.execute(
        "INSERT INTO task_dispatches (
            id, kanban_card_id, to_agent_id, dispatch_type, status, title, context, result, created_at, updated_at
         ) VALUES (
            'dispatch-review-recycled', 'card-review-recycled-wt', 'agent-1', 'implementation', 'completed',
            'Done', ?1, ?2, datetime('now'), datetime('now')
         )",
        sqlite_test::params![
            serde_json::json!({}).to_string(),
            serde_json::json!({
                "completed_worktree_path": recycled_wt_path,
                "completed_branch": "wt/684-obsolete",
                "completed_commit": reviewed_commit.clone(),
            })
            .to_string(),
        ],
    )
    .unwrap();
    drop(conn);

    let context = build_review_context(
        &db,
        "card-review-recycled-wt",
        "agent-1",
        &json!({}),
        ReviewTargetTrust::Untrusted,
        TargetRepoSource::CardScopeDefault,
    )
    .unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&context).unwrap();

    assert_eq!(parsed["reviewed_commit"], reviewed_commit);
    // Must NOT accept the recycled worktree_path — it exists but does
    // not contain the reviewed_commit.
    assert_ne!(
        parsed["worktree_path"].as_str(),
        Some(recycled_wt_path),
        "recycled worktree path (exists but missing reviewed_commit) must be dropped"
    );
    // Falls back to repo_dir where the reviewed_commit actually lives.
    assert_eq!(parsed["worktree_path"], repo_dir);
}

/// #682 (Codex round 2+3, [high]): An issue-bearing card whose recorded
/// target_repo differs from the card's canonical repo must recover its
/// worktree via target_repo, not card-scoped repo resolution. This test
/// specifically exercises the resolve_card_worktree path (not the
/// repo_dir fallback) by creating a LIVE issue worktree in the external
/// repo with reviewed_commit as HEAD. If resolve_card_worktree failed
/// to honor target_repo, recovery would fall through to the repo_dir
/// branch and the worktree-path + HEAD assertions would catch it.

#[test]
fn review_context_refreshes_issue_bearing_external_target_repo_stale_worktree() {
    let db = test_db();
    seed_card(&db, "card-review-external-tr", "review");
    set_card_issue_number(&db, "card-review-external-tr", 685);

    let (_card_default_repo, _repo_override) = setup_test_repo();
    // Separate external repo — the completion actually ran here.
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

    // Live issue worktree in the external repo whose branch name
    // encodes the issue (685) so find_worktree_for_issue picks it up.
    let live_wt_dir = external_repo.path().join("wt-685-live");
    let live_wt_path = live_wt_dir.to_str().unwrap();
    run_git(
        external_repo_dir,
        &["worktree", "add", "-b", "wt/685-live", live_wt_path],
    );
    let reviewed_commit = git_commit(
        live_wt_path,
        "fix: external issue target_repo refresh (#685)",
    );

    // Stale (deleted) worktree that the completion dispatch originally
    // ran on — must NOT be returned.
    let stale_wt_path = external_repo.path().join("wt-685-external-deleted");

    let conn = db.separate_conn().unwrap();
    conn.execute(
        "INSERT INTO task_dispatches (
            id, kanban_card_id, to_agent_id, dispatch_type, status, title, context, result, created_at, updated_at
         ) VALUES (
            'dispatch-review-external-tr', 'card-review-external-tr', 'agent-1', 'implementation', 'completed',
            'Done', ?1, ?2, datetime('now'), datetime('now')
         )",
        sqlite_test::params![
            serde_json::json!({ "target_repo": external_repo_dir }).to_string(),
            serde_json::json!({
                "completed_worktree_path": stale_wt_path,
                "completed_branch": "wt/685-external-deleted",
                "completed_commit": reviewed_commit.clone(),
            })
            .to_string(),
        ],
    )
    .unwrap();
    drop(conn);

    let context = build_review_context(
        &db,
        "card-review-external-tr",
        "agent-1",
        &json!({}),
        ReviewTargetTrust::Untrusted,
        TargetRepoSource::CardScopeDefault,
    )
    .unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&context).unwrap();

    assert_eq!(parsed["reviewed_commit"], reviewed_commit);
    let actual_wt = parsed["worktree_path"].as_str().unwrap();
    let canonical_live = std::fs::canonicalize(live_wt_path)
        .unwrap()
        .to_string_lossy()
        .into_owned();
    let canonical_actual = std::fs::canonicalize(actual_wt)
        .unwrap()
        .to_string_lossy()
        .into_owned();
    assert_eq!(
        canonical_actual, canonical_live,
        "issue-bearing external-repo review must resolve to the live issue worktree via target_repo (not the repo root fallback)"
    );
    // Verify the returned path actually has reviewed_commit as HEAD —
    // this is what makes the test bite even if target_repo injection
    // silently misrouted to repo_dir (repo_dir HEAD is just the
    // initial empty commit, not reviewed_commit).
    let head_output = GitCommand::new()
        .repo(actual_wt)
        .args(["rev-parse", "HEAD"])
        .run_output()
        .unwrap();
    let head = String::from_utf8_lossy(&head_output.stdout)
        .trim()
        .to_string();
    assert_eq!(
        head, reviewed_commit,
        "returned worktree must have reviewed_commit as HEAD"
    );
}

/// #682 (Codex round 2, [high]): A recorded worktree path that still
/// exists but whose HEAD has advanced past reviewed_commit (follow-up
/// work on the same branch) must NOT be reused as-is. The reviewer
/// would otherwise see the descendant filesystem state, not the
/// reviewed state. git_commit_exists and merge-base --is-ancestor both
/// accept this case — only exact HEAD match is safe.

#[test]
fn review_context_rejects_recorded_worktree_with_descendant_head() {
    let db = test_db();
    seed_card(&db, "card-review-descendant", "review");
    set_card_issue_number(&db, "card-review-descendant", 686);

    let (repo, _repo_override) = setup_test_repo();
    let repo_dir = repo.path().to_str().unwrap();

    let wt_dir = repo.path().join("wt-686-descendant");
    let wt_path = wt_dir.to_str().unwrap();
    run_git(
        repo_dir,
        &["worktree", "add", "-b", "wt/686-descendant", wt_path],
    );
    let reviewed_commit = git_commit(wt_path, "fix: reviewed commit on descendant wt (#686)");
    // HEAD advances past the reviewed commit — follow-up commit on the
    // same branch in the same worktree.
    let _descendant_commit = git_commit(wt_path, "chore: follow-up work beyond reviewed");

    let conn = db.separate_conn().unwrap();
    conn.execute(
        "INSERT INTO task_dispatches (
            id, kanban_card_id, to_agent_id, dispatch_type, status, title, context, result, created_at, updated_at
         ) VALUES (
            'dispatch-review-descendant', 'card-review-descendant', 'agent-1', 'implementation', 'completed',
            'Done', ?1, ?2, datetime('now'), datetime('now')
         )",
        sqlite_test::params![
            serde_json::json!({}).to_string(),
            serde_json::json!({
                "completed_worktree_path": wt_path,
                "completed_branch": "wt/686-descendant",
                "completed_commit": reviewed_commit.clone(),
            })
            .to_string(),
        ],
    )
    .unwrap();
    drop(conn);

    let context = build_review_context(
        &db,
        "card-review-descendant",
        "agent-1",
        &json!({}),
        ReviewTargetTrust::Untrusted,
        TargetRepoSource::CardScopeDefault,
    )
    .unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&context).unwrap();

    assert_eq!(parsed["reviewed_commit"], reviewed_commit);
    // Recorded path must NOT be reused — HEAD advanced past the reviewed
    // commit.
    assert_ne!(
        parsed["worktree_path"].as_str(),
        Some(wt_path),
        "recorded worktree with advanced HEAD must be rejected"
    );
    // #682 (Codex round 3, [high]): when a worktree_path IS emitted, it
    // must have HEAD==reviewed_commit. Otherwise the reviewer sees the
    // wrong filesystem state. Acceptable outcomes:
    //   (a) worktree_path is None (reviewer falls back to default repo)
    //   (b) worktree_path is a path with HEAD exactly at reviewed_commit
    // (c) worktree_path is the recorded wt_path — which is the failure
    //     this test guards against.
    if let Some(emitted) = parsed["worktree_path"].as_str() {
        let head_output = GitCommand::new()
            .repo(emitted)
            .args(["rev-parse", "HEAD"])
            .run_output()
            .unwrap();
        let head = String::from_utf8_lossy(&head_output.stdout)
            .trim()
            .to_string();
        assert_eq!(
            head, reviewed_commit,
            "if worktree_path is emitted after rejecting the recorded path, HEAD must be exactly reviewed_commit (got {} at {})",
            head, emitted
        );
    }
    _ = repo_dir; // silence unused warning when worktree_path is None
}

#[test]
fn review_context_includes_merge_base_for_branch_review() {
    let db = test_db();
    seed_card(&db, "card-review-merge-base", "review");

    let (repo, _repo_override) = setup_test_repo();
    let repo_dir = repo.path().to_str().unwrap();
    let fork_point = crate::services::platform::git_head_commit(repo_dir).unwrap();
    let wt_dir = repo.path().join("wt-542");
    let wt_path = wt_dir.to_str().unwrap();

    run_git(repo_dir, &["worktree", "add", wt_path, "-b", "wt/fix-542"]);
    let reviewed_commit = git_commit(wt_path, "fix: branch-only review target");
    let main_commit = git_commit(repo_dir, "chore: main advanced after fork");
    assert_ne!(fork_point, main_commit);

    let conn = db.separate_conn().unwrap();
    conn.execute(
        "INSERT INTO task_dispatches (
            id, kanban_card_id, to_agent_id, dispatch_type, status, title, context, result, created_at, updated_at
         ) VALUES (
            'dispatch-review-merge-base', 'card-review-merge-base', 'agent-1', 'implementation', 'completed',
            'Done', ?1, ?2, datetime('now'), datetime('now')
         )",
        sqlite_test::params![
            serde_json::json!({}).to_string(),
            serde_json::json!({
                "completed_worktree_path": wt_path,
                "completed_branch": "wt/fix-542",
                "completed_commit": reviewed_commit.clone(),
            })
            .to_string(),
        ],
    )
    .unwrap();
    drop(conn);

    let context = build_review_context(
        &db,
        "card-review-merge-base",
        "agent-1",
        &json!({}),
        ReviewTargetTrust::Untrusted,
        TargetRepoSource::CardScopeDefault,
    )
    .unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&context).unwrap();

    assert_eq!(parsed["reviewed_commit"], reviewed_commit);
    assert_eq!(parsed["branch"], "wt/fix-542");
    assert_eq!(parsed["merge_base"], fork_point);
}

#[test]
fn review_context_skips_missing_merge_base_for_unknown_branch() {
    let mut obj = serde_json::Map::new();
    let (repo, _repo_override) = setup_test_repo();
    let repo_dir = repo.path().to_str().unwrap();
    let reviewed_commit = crate::services::platform::git_head_commit(repo_dir).unwrap();
    obj.insert("worktree_path".to_string(), json!(repo_dir));
    obj.insert("branch".to_string(), json!("missing-branch"));
    obj.insert("reviewed_commit".to_string(), json!(reviewed_commit));

    inject_review_merge_base_context(&mut obj);

    assert!(
        !obj.contains_key("merge_base"),
        "missing git merge-base must leave merge_base absent"
    );
}

#[test]
fn review_context_accepts_latest_work_dispatch_commit_for_same_issue() {
    let db = test_db();
    seed_card(&db, "card-review-match", "review");
    set_card_issue_number(&db, "card-review-match", 305);

    let (repo, _repo_override) = setup_test_repo();
    let repo_dir = repo.path().to_str().unwrap();
    let matching_commit = git_commit(repo_dir, "fix: target commit (#305)");

    let conn = db.separate_conn().unwrap();
    conn.execute(
        "INSERT INTO task_dispatches (
            id, kanban_card_id, to_agent_id, dispatch_type, status, title, context, result, created_at, updated_at
         ) VALUES (
            'dispatch-review-match', 'card-review-match', 'agent-1', 'implementation', 'completed',
            'Done', ?1, ?2, datetime('now'), datetime('now')
         )",
        sqlite_test::params![
            serde_json::json!({}).to_string(),
            serde_json::json!({
                "completed_worktree_path": repo_dir,
                "completed_branch": "main",
                "completed_commit": matching_commit.clone(),
            })
            .to_string(),
        ],
    )
    .unwrap();
    drop(conn);

    let context = build_review_context(
        &db,
        "card-review-match",
        "agent-1",
        &json!({}),
        ReviewTargetTrust::Untrusted,
        TargetRepoSource::CardScopeDefault,
    )
    .unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&context).unwrap();

    assert_eq!(parsed["reviewed_commit"], matching_commit);
    assert_eq!(parsed["worktree_path"], repo_dir);
    assert_eq!(parsed["branch"], "main");
    assert!(
        parsed.get("merge_base").is_none(),
        "main branch reviews must not inject an empty merge-base diff"
    );
}

#[test]
fn review_context_rejects_latest_work_dispatch_commit_from_other_issue() {
    let db = test_db();
    seed_card(&db, "card-review-mismatch", "review");
    set_card_issue_number(&db, "card-review-mismatch", 305);

    let (repo, _repo_override) = setup_test_repo();
    let repo_dir = repo.path().to_str().unwrap();
    let expected_commit = git_commit(repo_dir, "fix: target commit (#305)");
    let poisoned_commit = git_commit(repo_dir, "chore: unrelated (#999)");

    let conn = db.separate_conn().unwrap();
    conn.execute(
        "INSERT INTO task_dispatches (
            id, kanban_card_id, to_agent_id, dispatch_type, status, title, context, result, created_at, updated_at
         ) VALUES (
            'dispatch-review-mismatch', 'card-review-mismatch', 'agent-1', 'implementation', 'completed',
            'Done', ?1, ?2, datetime('now'), datetime('now')
         )",
        sqlite_test::params![
            serde_json::json!({}).to_string(),
            serde_json::json!({
                "completed_worktree_path": repo_dir,
                "completed_branch": "main",
                "completed_commit": poisoned_commit.clone(),
            })
            .to_string(),
        ],
    )
    .unwrap();
    drop(conn);

    let context = build_review_context(
        &db,
        "card-review-mismatch",
        "agent-1",
        &json!({}),
        ReviewTargetTrust::Untrusted,
        TargetRepoSource::CardScopeDefault,
    )
    .unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&context).unwrap();

    assert_eq!(parsed["reviewed_commit"], expected_commit);
    assert_ne!(parsed["reviewed_commit"], poisoned_commit);
    assert_eq!(parsed["worktree_path"], repo_dir);
    assert_eq!(parsed["branch"], "main");
}

#[test]
fn review_context_skips_poisoned_non_main_worktree_when_latest_commit_does_not_match_issue() {
    let db = test_db();
    seed_card(&db, "card-review-worktree-fallback", "review");
    set_card_issue_number(&db, "card-review-worktree-fallback", 320);

    let (repo, _repo_override) = setup_test_repo();
    let repo_dir = repo.path().to_str().unwrap();
    let expected_commit = git_commit(repo_dir, "fix: target commit (#320)");
    let wt_dir = repo.path().join("wt-320");
    let wt_path = wt_dir.to_str().unwrap();

    run_git(
        repo_dir,
        &["worktree", "add", wt_path, "-b", "wt/320-phase6"],
    );
    let poisoned_commit = git_commit(wt_path, "chore: unrelated worktree drift (#999)");
    assert_ne!(
        expected_commit, poisoned_commit,
        "poisoned worktree head must differ from the issue commit fallback"
    );

    let conn = db.separate_conn().unwrap();
    conn.execute(
        "INSERT INTO task_dispatches (
            id, kanban_card_id, to_agent_id, dispatch_type, status, title, context, result, created_at, updated_at
         ) VALUES (
            'dispatch-review-worktree-fallback', 'card-review-worktree-fallback', 'agent-1', 'implementation', 'completed',
            'Done', ?1, ?2, datetime('now'), datetime('now')
         )",
        sqlite_test::params![
            serde_json::json!({}).to_string(),
            serde_json::json!({
                "completed_worktree_path": wt_path,
                "completed_branch": "wt/320-phase6",
                "completed_commit": poisoned_commit.clone(),
            })
            .to_string(),
        ],
    )
    .unwrap();
    drop(conn);

    let context = build_review_context(
        &db,
        "card-review-worktree-fallback",
        "agent-1",
        &json!({}),
        ReviewTargetTrust::Untrusted,
        TargetRepoSource::CardScopeDefault,
    )
    .unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&context).unwrap();

    assert_eq!(parsed["worktree_path"], repo_dir);
    assert_eq!(parsed["branch"], "main");
    assert_eq!(parsed["reviewed_commit"], expected_commit);
    assert_ne!(parsed["reviewed_commit"], poisoned_commit);
}

#[test]
fn review_context_rejects_repo_head_fallback_when_repo_root_is_dirty() {
    let db = test_db();
    seed_card(&db, "card-review-dirty-root", "review");

    let (repo, _repo_override) = setup_test_repo();
    let repo_dir = repo.path().to_str().unwrap();
    std::fs::write(repo.path().join("tracked.txt"), "baseline\n").unwrap();
    run_git(repo_dir, &["add", "tracked.txt"]);
    run_git(repo_dir, &["commit", "-m", "feat: add tracked file"]);
    std::fs::write(repo.path().join("tracked.txt"), "dirty\n").unwrap();

    let err = build_review_context(
        &db,
        "card-review-dirty-root",
        "agent-1",
        &json!({}),
        ReviewTargetTrust::Untrusted,
        TargetRepoSource::CardScopeDefault,
    )
    .expect_err("dirty repo root must block repo HEAD fallback");

    assert!(
        err.to_string()
            .contains("repo-root HEAD fallback is unsafe while tracked changes exist"),
        "unexpected error: {err:#}"
    );
}

#[test]
fn review_context_rejects_commitless_completed_work_when_repo_root_is_dirty() {
    let db = test_db();
    seed_card(&db, "card-review-dirty-completion", "review");

    let (repo, _repo_override) = setup_test_repo();
    let repo_dir = repo.path().to_str().unwrap();
    std::fs::write(repo.path().join("tracked.txt"), "baseline\n").unwrap();
    run_git(repo_dir, &["add", "tracked.txt"]);
    run_git(repo_dir, &["commit", "-m", "feat: add tracked file"]);
    std::fs::write(repo.path().join("tracked.txt"), "dirty\n").unwrap();

    let conn = db.separate_conn().unwrap();
    conn.execute(
        "INSERT INTO task_dispatches (
            id, kanban_card_id, to_agent_id, dispatch_type, status, title, context, result, created_at, updated_at
         ) VALUES (
            'dispatch-review-dirty-completion', 'card-review-dirty-completion', 'agent-1', 'implementation', 'completed',
            'Implemented without commit', ?1, ?2, datetime('now'), datetime('now')
         )",
        sqlite_test::params![
            serde_json::json!({}).to_string(),
            serde_json::json!({}).to_string(),
        ],
    )
    .unwrap();
    drop(conn);

    let err = build_review_context(
        &db,
        "card-review-dirty-completion",
        "agent-1",
        &json!({}),
        ReviewTargetTrust::Untrusted,
        TargetRepoSource::CardScopeDefault,
    )
    .expect_err("dirty repo root must block fallback after commitless completion");

    assert!(
        err.to_string()
            .contains("repo-root HEAD fallback is unsafe while tracked changes exist"),
        "unexpected error: {err:#}"
    );
}

#[test]
fn review_context_skips_repo_head_fallback_after_rejected_external_work_target() {
    let db = test_db();
    seed_card(&db, "card-review-external-reject", "review");
    set_card_issue_number(&db, "card-review-external-reject", 595);

    let (repo, _repo_override) = setup_test_repo();
    let repo_dir = repo.path().to_str().unwrap();
    let default_head = crate::services::platform::git_head_commit(repo_dir).unwrap();

    let external_repo = tempfile::tempdir().unwrap();
    let external_dir = external_repo.path().to_str().unwrap();
    run_git(external_dir, &["init", "-b", "main"]);
    run_git(external_dir, &["config", "user.email", "test@test.com"]);
    run_git(external_dir, &["config", "user.name", "Test"]);
    run_git(external_dir, &["commit", "--allow-empty", "-m", "initial"]);
    run_git(
        external_dir,
        &["checkout", "-b", "codex/595-agentdesk-aiinstructions"],
    );
    let external_commit = git_commit(
        external_dir,
        "fix: shrink aiInstructions in external repo (#595)",
    );

    let conn = db.separate_conn().unwrap();
    conn.execute(
        "INSERT INTO task_dispatches (
            id, kanban_card_id, to_agent_id, dispatch_type, status, title, context, result, created_at, updated_at
         ) VALUES (
            'dispatch-review-external-reject', 'card-review-external-reject', 'agent-1', 'implementation', 'completed',
            'Done', ?1, ?2, datetime('now'), datetime('now')
         )",
        sqlite_test::params![
            serde_json::json!({}).to_string(),
            serde_json::json!({
                "completed_worktree_path": external_dir,
                "completed_branch": "codex/595-agentdesk-aiinstructions",
                "completed_commit": external_commit.clone(),
            })
            .to_string(),
        ],
    )
    .unwrap();
    drop(conn);

    let context = build_review_context(
        &db,
        "card-review-external-reject",
        "agent-1",
        &json!({}),
        ReviewTargetTrust::Untrusted,
        TargetRepoSource::CardScopeDefault,
    )
    .unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&context).unwrap();

    assert!(
        parsed.get("reviewed_commit").is_none(),
        "rejected external work target must not fall back to repo HEAD"
    );
    assert!(
        parsed.get("branch").is_none(),
        "rejected external work target must not inject a fake branch"
    );
    assert!(
        parsed.get("worktree_path").is_none(),
        "rejected external work target must not inject the default repo path"
    );
    assert!(
        parsed.get("merge_base").is_none(),
        "rejected external work target must not emit a fake diff range"
    );
    assert_eq!(
        parsed["review_target_reject_reason"],
        "latest_work_target_issue_mismatch"
    );
    assert!(
        parsed["review_target_warning"]
            .as_str()
            .unwrap_or_default()
            .contains("브랜치 정보 없음"),
        "warning must tell reviewers that manual lookup is required"
    );
    assert_ne!(
        parsed["reviewed_commit"],
        json!(default_head),
        "default repo HEAD must not be injected after rejection"
    );
}

#[test]
fn review_context_accepts_external_work_target_when_target_repo_is_in_context() {
    let db = test_db();
    seed_card(&db, "card-review-external-accept", "review");
    set_card_issue_number(&db, "card-review-external-accept", 627);
    set_card_repo_id(&db, "card-review-external-accept", "owner/missing");

    let default_repo = init_test_repo();
    let default_repo_dir = default_repo.path().to_str().unwrap();
    let _env = DispatchEnvOverride::new(Some(default_repo_dir), None);

    let external_repo = init_test_repo();
    let external_dir = external_repo.path().to_str().unwrap();
    run_git(external_dir, &["checkout", "-b", "codex/627-target-repo"]);
    let external_commit = git_commit(external_dir, "fix: cross repo review target (#627)");
    let conn = db.separate_conn().unwrap();
    conn.execute(
        "INSERT INTO task_dispatches (
            id, kanban_card_id, to_agent_id, dispatch_type, status, title, context, result, created_at, updated_at
         ) VALUES (
            'dispatch-review-external-accept', 'card-review-external-accept', 'agent-1', 'implementation', 'completed',
            'Done', ?1, ?2, datetime('now'), datetime('now')
         )",
        sqlite_test::params![
            serde_json::json!({}).to_string(),
            serde_json::json!({
                "completed_worktree_path": external_dir,
                "completed_branch": "codex/627-target-repo",
                "completed_commit": external_commit.clone(),
            })
            .to_string(),
        ],
    )
    .unwrap();
    drop(conn);

    // #761 (Codex round-2): Trusted internal callers may pre-seed
    // `target_repo` to steer review at an external repo. Public API
    // callers cannot — the trust signal is an out-of-band enum on
    // `build_review_context`, NOT a JSON field on the context payload.
    // The API-sourced path (`POST /api/dispatches` →
    // `create_dispatch_core_internal` → `build_review_context` with
    // `ReviewTargetTrust::Untrusted`) always strips review-target fields
    // regardless of what the client sent. See
    // `dispatch_create_review_strips_untrusted_review_target_fields_from_context`
    // in `server/routes/routes_tests.rs` for the API-level negative case.
    let context = build_review_context(
        &db,
        "card-review-external-accept",
        "agent-1",
        &json!({ "target_repo": external_dir }),
        ReviewTargetTrust::Trusted,
        TargetRepoSource::CallerSupplied,
    )
    .unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&context).unwrap();
    let actual_worktree = std::fs::canonicalize(parsed["worktree_path"].as_str().unwrap())
        .unwrap()
        .to_string_lossy()
        .into_owned();
    let actual_target_repo = std::fs::canonicalize(parsed["target_repo"].as_str().unwrap())
        .unwrap()
        .to_string_lossy()
        .into_owned();
    let expected_external_dir = std::fs::canonicalize(external_dir)
        .unwrap()
        .to_string_lossy()
        .into_owned();

    assert_eq!(parsed["reviewed_commit"], external_commit);
    assert_eq!(parsed["branch"], "codex/627-target-repo");
    assert_eq!(actual_worktree, expected_external_dir);
    assert_eq!(actual_target_repo, expected_external_dir);
    // #761 (Codex round-2): Even though trust is now an out-of-band Rust
    // parameter, defensively confirm no legacy `_trusted_review_target`
    // JSON key slips through if some upstream caller ever attached one.
    assert!(
        parsed.get("_trusted_review_target").is_none(),
        "legacy trusted sentinel must not appear in the persisted dispatch context"
    );
}

/// #762 (A): If the historical work dispatch ran against an external
/// `target_repo` whose reviewed commit can no longer be recovered, the
/// review must NOT silently fall back to the card's canonical worktree.
/// Prior behavior consulted `resolve_card_worktree`/
/// `resolve_card_issue_commit_target` with `ctx_snapshot` (card-scoped),
/// which silently redirected the reviewer to unrelated code whenever the
/// card had its own live issue worktree. Fail closed instead.

#[test]
fn review_context_fails_closed_when_external_target_repo_is_unrecoverable() {
    let db = test_db();
    seed_card(&db, "card-review-762-external-fail", "review");
    set_card_issue_number(&db, "card-review-762-external-fail", 762);

    // Card's canonical repo: this is where the silent-redirect bug would
    // have sent the reviewer. It has a LIVE worktree for issue 762.
    let (card_repo, _repo_override) = setup_test_repo();
    let card_repo_dir = card_repo.path().to_str().unwrap();
    set_card_repo_id(&db, "card-review-762-external-fail", card_repo_dir);
    let card_live_wt_dir = card_repo.path().join("wt-762-card-live");
    let card_live_wt_path = card_live_wt_dir.to_str().unwrap();
    run_git(
        card_repo_dir,
        &[
            "worktree",
            "add",
            "-b",
            "wt/762-card-live",
            card_live_wt_path,
        ],
    );
    let _card_live_commit = git_commit(
        card_live_wt_path,
        "feat: unrelated ongoing work on card issue (#762)",
    );

    // External repo where the historical work ran. We create the
    // reviewed_commit here (subject references #762 so the validity
    // check passes) but then blow the whole directory away — this is
    // the "external repo unrecoverable" scenario.
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
        "fix: external unrecoverable commit (#762)",
    );

    let conn = db.separate_conn().unwrap();
    conn.execute(
        "INSERT INTO task_dispatches (
            id, kanban_card_id, to_agent_id, dispatch_type, status, title, context, result, created_at, updated_at
         ) VALUES (
            'dispatch-review-762-external-fail', 'card-review-762-external-fail', 'agent-1', 'implementation', 'completed',
            'Done', ?1, ?2, datetime('now'), datetime('now')
         )",
        sqlite_test::params![
            serde_json::json!({ "target_repo": external_repo_dir }).to_string(),
            serde_json::json!({
                "completed_worktree_path":
                    external_repo.path().join("wt-762-external-deleted"),
                "completed_branch": "wt/762-external-deleted",
                "completed_commit": reviewed_commit.clone(),
            })
            .to_string(),
        ],
    )
    .unwrap();
    drop(conn);

    // Make the external repo genuinely unrecoverable. After this, the
    // path exists but is not a git repo, so resolve_repo_dir_for_target
    // errors and refresh cannot locate reviewed_commit via target_repo
    // or via the card repo (card repo never had that commit).
    std::fs::remove_dir_all(external_repo_dir).unwrap();

    let context = build_review_context(
        &db,
        "card-review-762-external-fail",
        "agent-1",
        &json!({}),
        ReviewTargetTrust::Trusted,
        TargetRepoSource::CardScopeDefault,
    )
    .unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&context).unwrap();

    assert!(
        parsed.get("reviewed_commit").is_none(),
        "unrecoverable external target_repo must not emit a reviewed_commit from card scope"
    );
    assert!(
        parsed.get("worktree_path").is_none(),
        "unrecoverable external target_repo must not redirect to card's live issue worktree: got {:?}",
        parsed.get("worktree_path")
    );
    assert!(
        parsed.get("branch").is_none(),
        "unrecoverable external target_repo must not inject a card-scoped branch"
    );
    assert_eq!(
        parsed["review_target_reject_reason"],
        "external_target_repo_unrecoverable"
    );
    assert!(
        parsed["review_target_warning"]
            .as_str()
            .unwrap_or_default()
            .contains("target_repo"),
        "warning must mention target_repo so operators can investigate"
    );
    // The original external target_repo is preserved on the context so
    // downstream prompt builders can surface it to the reviewer even
    // when the commit itself cannot be located.
    assert_eq!(parsed["target_repo"], external_repo_dir);
}

/// #762 round-2 (A): when the dispatch-core path pre-injects the card's
/// `target_repo` into the context before calling `build_review_context`,
/// the fail-closed filter for unrecoverable external target_repos must
/// STILL engage. Previous behavior snapshotted `context["target_repo"]`
/// after the pre-injection and treated every dispatch as
/// caller-supplied — silently disabling the filter and letting
/// card-scoped fallbacks redirect the reviewer to unrelated code.

#[test]
fn create_dispatch_core_review_path_honors_caller_supplied_target_repo() {
    let db = test_db();
    seed_card(&db, "card-review-762-a-caller", "review");
    set_card_issue_number(&db, "card-review-762-a-caller", 627);
    set_card_repo_id(&db, "card-review-762-a-caller", "owner/missing");

    let default_repo = init_test_repo();
    let default_repo_dir = default_repo.path().to_str().unwrap();
    let _env = DispatchEnvOverride::new(Some(default_repo_dir), None);

    let external_repo = init_test_repo();
    let external_dir = external_repo.path().to_str().unwrap();
    run_git(
        external_dir,
        &["checkout", "-b", "codex/627-caller-supplied"],
    );
    let external_commit = git_commit(external_dir, "fix: caller-supplied target repo (#627)");
    let conn = db.separate_conn().unwrap();
    conn.execute(
        "INSERT INTO task_dispatches (
            id, kanban_card_id, to_agent_id, dispatch_type, status, title, context, result, created_at, updated_at
         ) VALUES (
            'dispatch-review-762-a-caller', 'card-review-762-a-caller', 'agent-1', 'implementation', 'completed',
            'Done', ?1, ?2, datetime('now'), datetime('now')
         )",
        sqlite_test::params![
            serde_json::json!({}).to_string(),
            serde_json::json!({
                "completed_worktree_path": external_dir,
                "completed_branch": "codex/627-caller-supplied",
                "completed_commit": external_commit.clone(),
            })
            .to_string(),
        ],
    )
    .unwrap();
    drop(conn);

    // Trusted internal invocation — simulates an in-process Rust caller
    // that legitimately pre-pins `target_repo`. Public API clients cannot
    // reach this path (see #761: dispatch_create_core_internal always
    // passes ReviewTargetTrust::Untrusted).
    let context_str = build_review_context(
        &db,
        "card-review-762-a-caller",
        "agent-1",
        &json!({ "target_repo": external_dir }),
        ReviewTargetTrust::Trusted,
        TargetRepoSource::CallerSupplied,
    )
    .unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&context_str).unwrap();

    assert_eq!(parsed["reviewed_commit"], external_commit);
    assert_eq!(parsed["branch"], "codex/627-caller-supplied");
    assert!(
        parsed.get("review_target_reject_reason").is_none(),
        "caller-supplied target_repo must not trigger the unrecoverable filter: {parsed:#?}"
    );
}

/// #762 round-2 (C): when the historical work dispatch recorded a
/// `target_repo` we cannot resolve AND the card has no resolvable
/// `repo_id`, `historical_target_repo_differs_from_card` must treat the
/// situation as divergent. Previous behavior returned `false` (not
/// divergent), which let `resolve_repo_dir_for_target(None)` redirect to
/// the default repo — silent external redirect.

#[test]
fn review_context_fails_closed_when_both_work_and_card_target_repos_are_unresolvable() {
    let db = test_db();
    seed_card(&db, "card-review-762-c-none-none", "review");
    set_card_issue_number(&db, "card-review-762-c-none-none", 762);
    // NOTE: intentionally DO NOT set_card_repo_id — card has no
    // resolvable repo_id, so `card_repo_id` side of the comparison is
    // `None`.

    // Set the default repo so card-scoped fallback would resolve into
    // an unrelated repo if the bug triggers.
    let default_repo = init_test_repo();
    let default_repo_dir = default_repo.path().to_str().unwrap();
    let _env = DispatchEnvOverride::new(Some(default_repo_dir), None);
    // Seed an unrelated commit in the default repo — if the silent
    // redirect happens, reviewed_commit would be this unrelated HEAD.
    let default_head = git_commit(default_repo_dir, "chore: unrelated default repo work");

    // Historical dispatch recorded a `target_repo` pointing at a
    // directory that does NOT resolve to any known repo (doesn't
    // exist). This makes `normalized_target_repo_path(work)` return
    // None, and card is None → (None, None).
    let bogus_external = "/tmp/agentdesk-762-nonexistent-external-xyz";
    let reviewed_commit = default_head.clone(); // any sha; won't be used

    let conn = db.separate_conn().unwrap();
    conn.execute(
        "INSERT INTO task_dispatches (
            id, kanban_card_id, to_agent_id, dispatch_type, status, title, context, result, created_at, updated_at
         ) VALUES (
            'dispatch-review-762-c-none-none', 'card-review-762-c-none-none', 'agent-1', 'implementation', 'completed',
            'Done', ?1, ?2, datetime('now'), datetime('now')
         )",
        sqlite_test::params![
            serde_json::json!({ "target_repo": bogus_external }).to_string(),
            serde_json::json!({
                "completed_worktree_path": format!("{bogus_external}/wt-gone"),
                "completed_branch": "wt/762-c-gone",
                "completed_commit": reviewed_commit.clone(),
            })
            .to_string(),
        ],
    )
    .unwrap();
    drop(conn);

    let context = build_review_context(
        &db,
        "card-review-762-c-none-none",
        "agent-1",
        &json!({}),
        ReviewTargetTrust::Trusted,
        TargetRepoSource::CardScopeDefault,
    )
    .unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&context).unwrap();

    assert_eq!(
        parsed["review_target_reject_reason"], "external_target_repo_unrecoverable",
        "when work_target_repo is unresolvable AND card has no resolvable repo_id, must fail closed instead of redirecting to default repo: {parsed:#?}"
    );
    assert!(
        parsed.get("reviewed_commit").is_none(),
        "must not redirect to default repo HEAD"
    );
    assert_ne!(
        parsed.get("reviewed_commit").and_then(|v| v.as_str()),
        Some(default_head.as_str()),
        "default repo HEAD must never be injected when both sides unresolvable"
    );
}

#[test]
fn review_context_allows_explicit_noop_latest_work_dispatch_when_review_mode_is_noop_verification()
{
    let db = test_db();
    seed_card(&db, "card-review-noop", "review");

    let conn = db.separate_conn().unwrap();
    conn.execute(
        "INSERT INTO task_dispatches (
            id, kanban_card_id, to_agent_id, dispatch_type, status, title, context, result, created_at, updated_at
         ) VALUES (
            'dispatch-review-noop', 'card-review-noop', 'agent-1', 'implementation', 'completed',
            'No changes needed', ?1, ?2, datetime('now'), datetime('now')
         )",
        sqlite_test::params![
            serde_json::json!({}).to_string(),
            serde_json::json!({
                "work_outcome": "noop",
                "completed_without_changes": true,
                "notes": "spec already satisfied",
            })
            .to_string(),
        ],
    )
    .unwrap();
    drop(conn);

    let context = build_review_context(
        &db,
        "card-review-noop",
        "agent-1",
        &json!({
            "review_mode": "noop_verification",
            "noop_reason": "spec already satisfied"
        }),
        ReviewTargetTrust::Untrusted,
        TargetRepoSource::CardScopeDefault,
    )
    .expect("explicit noop work should still create a noop_verification review dispatch");
    let parsed: serde_json::Value =
        serde_json::from_str(&context).expect("review context must parse");
    assert_eq!(parsed["review_mode"], "noop_verification");
    assert_eq!(parsed["noop_reason"], "spec already satisfied");
}

#[test]
fn review_context_recovers_issue_branch_from_reviewed_commit_membership() {
    let db = test_db();
    seed_card(&db, "card-review-contains-branch", "review");
    set_card_issue_number(&db, "card-review-contains-branch", 610);

    let (repo, _repo_override) = setup_test_repo();
    let repo_dir = repo.path().to_str().unwrap();
    run_git(repo_dir, &["checkout", "-b", "feat/610-review"]);
    let reviewed_commit = git_commit(repo_dir, "fix: recover branch from commit (#610)");
    let fork_point = run_git(repo_dir, &["rev-parse", "HEAD^"]);
    let fork_point = String::from_utf8_lossy(&fork_point.stdout)
        .trim()
        .to_string();
    run_git(repo_dir, &["checkout", "main"]);

    let context = build_review_context(
        &db,
        "card-review-contains-branch",
        "agent-1",
        &json!({}),
        ReviewTargetTrust::Untrusted,
        TargetRepoSource::CardScopeDefault,
    )
    .unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&context).unwrap();

    assert_eq!(parsed["reviewed_commit"], reviewed_commit);
    assert_eq!(parsed["worktree_path"], repo_dir);
    assert_eq!(parsed["branch"], "feat/610-review");
    assert_eq!(parsed["merge_base"], fork_point);
}

#[test]
fn review_context_includes_quality_checklist_and_verdict_guidance() {
    let db = test_db();
    seed_card(&db, "card-review-quality", "review");

    let (repo, _repo_override) = setup_test_repo();
    let repo_dir = repo.path().to_str().unwrap();
    let completed_commit = crate::services::platform::git_head_commit(repo_dir).unwrap();

    let conn = db.separate_conn().unwrap();
    conn.execute(
        "INSERT INTO task_dispatches (
            id, kanban_card_id, to_agent_id, dispatch_type, status, title, context, result, created_at, updated_at
         ) VALUES (
            'dispatch-review-quality', 'card-review-quality', 'agent-1', 'implementation', 'completed',
            'Done', ?1, ?2, datetime('now'), datetime('now')
         )",
        sqlite_test::params![
            serde_json::json!({}).to_string(),
            serde_json::json!({
                "completed_worktree_path": repo_dir,
                "completed_branch": "main",
                "completed_commit": completed_commit,
            })
            .to_string(),
        ],
    )
    .unwrap();
    drop(conn);

    let context = build_review_context(
        &db,
        "card-review-quality",
        "agent-1",
        &json!({}),
        ReviewTargetTrust::Untrusted,
        TargetRepoSource::CardScopeDefault,
    )
    .unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&context).unwrap();
    let checklist = parsed["review_quality_checklist"]
        .as_array()
        .expect("checklist array must exist");

    assert_eq!(
        parsed["review_quality_scope_reminder"],
        REVIEW_QUALITY_SCOPE_REMINDER
    );
    assert_eq!(
        parsed["review_verdict_guidance"],
        REVIEW_VERDICT_IMPROVE_GUIDANCE
    );
    assert_eq!(checklist.len(), REVIEW_QUALITY_CHECKLIST.len());
    assert!(checklist.iter().any(|item| {
        item.as_str()
            .unwrap_or_default()
            .contains("race condition / 동시성 이슈")
    }));
    assert!(checklist.iter().any(|item| {
        item.as_str()
            .unwrap_or_default()
            .contains("에러 핸들링 누락")
    }));
}
