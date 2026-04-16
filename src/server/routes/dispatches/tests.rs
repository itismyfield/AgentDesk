use super::discord_delivery::{DispatchTransport, ReviewFollowupKind};
use super::outbox::{
    DispatchFollowupConfig, extract_review_verdict, format_dispatch_message,
    handle_completed_dispatch_followups, handle_completed_dispatch_followups_with_config,
    handle_completed_dispatch_followups_with_config_and_transport, prefix_dispatch_message,
    use_counter_model_channel,
};
use crate::db::Db;
use crate::engine::PolicyEngine;
use crate::server::routes::AppState;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::{
    Json, Router,
    extract::Path,
    response::IntoResponse,
    routing::{get, post},
};
use std::sync::{Arc, Mutex};

fn test_db() -> Db {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    conn.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
    crate::db::schema::migrate(&conn).unwrap();
    crate::db::wrap_conn(conn)
}

fn test_engine(db: &Db) -> PolicyEngine {
    let config = crate::config::Config::default();
    PolicyEngine::new(&config, db.clone()).unwrap()
}

#[derive(Default)]
struct MockDispatchSummaryState {
    archived: bool,
    calls: Vec<String>,
    patch_payloads: Vec<serde_json::Value>,
    messages: Vec<String>,
}

#[derive(Default)]
struct MockDispatchTransportState {
    dispatch_calls: usize,
    review_followup_kinds: Vec<ReviewFollowupKind>,
    review_followup_messages: Vec<String>,
}

#[derive(Clone, Default)]
struct MockDispatchTransport {
    state: Arc<Mutex<MockDispatchTransportState>>,
    send_dispatch_error: Option<String>,
    review_followup_error: Option<String>,
}

impl MockDispatchTransport {
    fn failing_dispatch(error: &str) -> Self {
        Self {
            send_dispatch_error: Some(error.to_string()),
            ..Self::default()
        }
    }
}

impl DispatchTransport for MockDispatchTransport {
    fn send_dispatch(
        &self,
        _db: Db,
        _agent_id: String,
        _title: String,
        _card_id: String,
        _dispatch_id: String,
    ) -> impl std::future::Future<Output = Result<(), String>> + Send {
        let state = self.state.clone();
        let error = self.send_dispatch_error.clone();
        async move {
            state.lock().unwrap().dispatch_calls += 1;
            match error {
                Some(error) => Err(error),
                None => Ok(()),
            }
        }
    }

    fn send_review_followup(
        &self,
        _db: Db,
        _card_id: String,
        _channel_id_num: u64,
        message: String,
        kind: ReviewFollowupKind,
    ) -> impl std::future::Future<Output = Result<(), String>> + Send {
        let state = self.state.clone();
        let error = self.review_followup_error.clone();
        async move {
            let mut guard = state.lock().unwrap();
            guard.review_followup_kinds.push(kind);
            guard.review_followup_messages.push(message);
            match error {
                Some(error) => Err(error),
                None => Ok(()),
            }
        }
    }
}

struct SummaryRepoFixture {
    _dir: tempfile::TempDir,
    path: String,
    start_commit: String,
    end_commit: String,
}

fn run_git(dir: &std::path::Path, args: &[&str]) -> std::process::Output {
    std::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap_or_else(|err| panic!("git {:?} failed to start: {err}", args))
}

fn run_git_ok(dir: &std::path::Path, args: &[&str]) {
    let output = run_git(dir, args);
    assert!(
        output.status.success(),
        "git {:?} failed: stdout={}, stderr={}",
        args,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

fn git_stdout(dir: &std::path::Path, args: &[&str]) -> String {
    let output = run_git(dir, args);
    assert!(
        output.status.success(),
        "git {:?} failed: stdout={}, stderr={}",
        args,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

fn write_text_file(path: &std::path::Path, content: &str) {
    std::fs::write(path, content).unwrap_or_else(|err| {
        panic!("failed to write {}: {err}", path.display());
    });
}

fn setup_summary_repo_fixture() -> SummaryRepoFixture {
    let dir = tempfile::tempdir().unwrap();
    let repo_path = dir.path().to_path_buf();

    run_git_ok(&repo_path, &["init", "-b", "main"]);
    write_text_file(&repo_path.join("a.txt"), "alpha\nremove\n");
    run_git_ok(&repo_path, &["add", "a.txt"]);
    run_git_ok(
        &repo_path,
        &[
            "-c",
            "user.name=Dispatch Test",
            "-c",
            "user.email=dispatch@example.com",
            "commit",
            "-m",
            "base",
        ],
    );
    let start_commit = git_stdout(&repo_path, &["rev-parse", "HEAD"]);

    run_git_ok(&repo_path, &["checkout", "-b", "wt/514-summary"]);
    write_text_file(&repo_path.join("a.txt"), "alpha\nadd\n");
    write_text_file(&repo_path.join("b.txt"), "beta\n");
    run_git_ok(&repo_path, &["add", "a.txt", "b.txt"]);
    run_git_ok(
        &repo_path,
        &[
            "-c",
            "user.name=Dispatch Test",
            "-c",
            "user.email=dispatch@example.com",
            "commit",
            "-m",
            "feature",
        ],
    );
    let end_commit = git_stdout(&repo_path, &["rev-parse", "HEAD"]);

    SummaryRepoFixture {
        _dir: dir,
        path: repo_path.to_string_lossy().into_owned(),
        start_commit,
        end_commit,
    }
}

async fn spawn_dispatch_summary_mock_server(
    initial_archived: bool,
) -> (
    String,
    Arc<Mutex<MockDispatchSummaryState>>,
    tokio::task::JoinHandle<()>,
) {
    async fn get_channel(
        State(state): State<Arc<Mutex<MockDispatchSummaryState>>>,
        Path(thread_id): Path<String>,
    ) -> impl IntoResponse {
        let archived = {
            let mut state = state.lock().unwrap();
            state.calls.push(format!("GET /channels/{thread_id}"));
            state.archived
        };
        (
            StatusCode::OK,
            Json(serde_json::json!({
                "id": thread_id,
                "thread_metadata": {
                    "archived": archived,
                    "locked": false
                }
            })),
        )
    }

    async fn patch_channel(
        State(state): State<Arc<Mutex<MockDispatchSummaryState>>>,
        Path(thread_id): Path<String>,
        Json(body): Json<serde_json::Value>,
    ) -> impl IntoResponse {
        let mut state = state.lock().unwrap();
        state.calls.push(format!("PATCH /channels/{thread_id}"));
        state.patch_payloads.push(body.clone());
        if let Some(archived) = body.get("archived").and_then(|value| value.as_bool()) {
            state.archived = archived;
        }
        (StatusCode::OK, Json(serde_json::json!({"id": thread_id})))
    }

    async fn post_message(
        State(state): State<Arc<Mutex<MockDispatchSummaryState>>>,
        Path(thread_id): Path<String>,
        Json(body): Json<serde_json::Value>,
    ) -> impl IntoResponse {
        let mut state = state.lock().unwrap();
        state
            .calls
            .push(format!("POST /channels/{thread_id}/messages"));
        if let Some(content) = body.get("content").and_then(|value| value.as_str()) {
            state.messages.push(content.to_string());
        }
        (
            StatusCode::OK,
            Json(serde_json::json!({"id": format!("message-{thread_id}")})),
        )
    }

    let state = Arc::new(Mutex::new(MockDispatchSummaryState {
        archived: initial_archived,
        ..Default::default()
    }));
    let app = Router::new()
        .route(
            "/channels/{thread_id}",
            get(get_channel).patch(patch_channel),
        )
        .route("/channels/{thread_id}/messages", post(post_message))
        .with_state(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    (format!("http://{addr}"), state, handle)
}

#[test]
fn review_dispatch_uses_counter_model_channel() {
    assert!(use_counter_model_channel(Some("review")));
    assert!(use_counter_model_channel(Some("e2e-test")));
    // #256: consultation dispatches go to counter-model channel
    assert!(use_counter_model_channel(Some("consultation")));
    // review-decision goes to the original agent's primary channel,
    // not the counter-model channel, to reuse the implementation thread
    assert!(!use_counter_model_channel(Some("review-decision")));
    assert!(!use_counter_model_channel(Some("implementation")));
    assert!(!use_counter_model_channel(Some("rework")));
    assert!(!use_counter_model_channel(None));
}

#[test]
fn review_dispatch_message_includes_compact_metadata_and_issue_url() {
    let message = format_dispatch_message(
        "dispatch-1",
        "[Review R1] card-1",
        Some("https://github.com/itismyfield/AgentDesk/issues/19"),
        Some(19),
        Some("review"),
        None,
    );

    assert!(message.starts_with(
        "── review dispatch ──\nDISPATCH:dispatch-1 [🔍 리뷰] - #19 [Review R1] card-1"
    ));
    assert!(message.contains("<https://github.com/itismyfield/AgentDesk/issues/19>"));
    assert!(message.contains(
        "한 줄 지시: 코드 리뷰만 수행하고 상세 범위와 verdict 규칙은 시스템 프롬프트의 [Current Task]를 따르세요."
    ));
    assert!(message.contains("dispatch-1"));
    assert!(message.contains("`POST /api/review-verdict` (`dispatch_id=dispatch-1`)"));
    assert!(!message.contains("VERDICT: pass|improve|reject|rework"));
    assert!(message.chars().count() <= 500);
}

#[test]
fn review_dispatch_message_omits_branch_and_commit_details() {
    let message = format_dispatch_message(
        "dispatch-br",
        "[Review R1] card-1",
        Some("https://github.com/itismyfield/AgentDesk/issues/19"),
        Some(19),
        Some("review"),
        None,
    );

    assert!(message.contains("DISPATCH:dispatch-br [🔍 리뷰] - #19 [Review R1] card-1"));
    assert!(!message.contains("wt/feature-branch"));
    assert!(!message.contains("abc12345deadbeef"));
    assert!(!message.contains("main 브랜치가 아닙니다"));
}

#[test]
fn review_dispatch_message_omits_verbose_quality_checklist_and_guidance() {
    let message = format_dispatch_message(
        "dispatch-quality",
        "[Review R1] card-1",
        Some("https://github.com/itismyfield/AgentDesk/issues/19"),
        Some(19),
        Some("review"),
        Some(
            &serde_json::json!({
                "review_quality_scope_reminder": crate::dispatch::REVIEW_QUALITY_SCOPE_REMINDER,
                "review_verdict_guidance": crate::dispatch::REVIEW_VERDICT_IMPROVE_GUIDANCE,
                "review_quality_checklist": crate::dispatch::REVIEW_QUALITY_CHECKLIST,
            })
            .to_string(),
        ),
    );

    assert!(message.contains("DISPATCH:dispatch-quality [🔍 리뷰] - #19 [Review R1] card-1"));
    assert!(!message.contains(crate::dispatch::REVIEW_QUALITY_SCOPE_REMINDER));
    assert!(!message.contains("race condition / 동시성 이슈"));
    assert!(!message.contains("에러 핸들링 누락"));
    assert!(!message.contains("리소스 정리 누락"));
    assert!(!message.contains(crate::dispatch::REVIEW_VERDICT_IMPROVE_GUIDANCE));
}

#[test]
fn review_dispatch_message_omits_merge_base_diff_instructions() {
    let message = format_dispatch_message(
        "dispatch-merge-base",
        "[Review R1] card-1",
        Some("https://github.com/itismyfield/AgentDesk/issues/19"),
        Some(19),
        Some("review"),
        Some(r#"{"merge_base":"11223344556677889900aabbccddeeff00112233"}"#),
    );

    assert!(message.contains("DISPATCH:dispatch-merge-base [🔍 리뷰] - #19 [Review R1] card-1"));
    assert!(!message.contains("merge-base(main"));
    assert!(!message.contains("11223344556677889900aabbccddeeff00112233"));
    assert!(!message.contains("git diff"));
}

#[test]
fn review_dispatch_message_without_commit() {
    let message = format_dispatch_message(
        "dispatch-no-commit",
        "[Review R1] card-1",
        None,
        None,
        Some("review"),
        None,
    );

    assert!(message.contains("DISPATCH:dispatch-no-commit [🔍 리뷰] - [Review R1] card-1"));
    assert!(message.contains("dispatch-no-commit"));
    assert!(message.contains(
        "한 줄 지시: 코드 리뷰만 수행하고 상세 범위와 verdict 규칙은 시스템 프롬프트의 [Current Task]를 따르세요."
    ));
    assert!(!message.contains(r#""commit""#));
    assert!(!message.contains(r#""provider""#));
}

#[test]
fn review_dispatch_message_omits_noop_verification_details() {
    let message = format_dispatch_message(
        "dispatch-noop-review",
        "[Review R1] card-655",
        Some("https://github.com/itismyfield/AgentDesk/issues/655"),
        Some(655),
        Some("review"),
        Some(
            &serde_json::json!({
                "review_mode": "noop_verification",
                "noop_reason": "OUTCOME: noop\\nfeature already exists",
                "review_quality_scope_reminder": crate::dispatch::REVIEW_QUALITY_SCOPE_REMINDER,
                "review_verdict_guidance": crate::dispatch::REVIEW_VERDICT_IMPROVE_GUIDANCE,
                "review_quality_checklist": crate::dispatch::REVIEW_QUALITY_CHECKLIST,
            })
            .to_string(),
        ),
    );

    assert!(
        message.contains("DISPATCH:dispatch-noop-review [🔍 리뷰] - #655 [Review R1] card-655")
    );
    assert!(!message.contains("Review Mode"));
    assert!(!message.contains("noop_verification"));
    assert!(!message.contains("OUTCOME: noop"));
    assert!(!message.contains("git diff"));
}

#[test]
fn review_dispatch_message_omits_manual_lookup_warning_when_branch_is_missing() {
    let message = format_dispatch_message(
        "dispatch-manual-review-target",
        "[Review R1] card-1",
        None,
        None,
        Some("review"),
        Some(
            &serde_json::json!({
                "review_target_warning": "브랜치 정보 없음 — 직접 확인 필요. 최근 완료 작업 커밋이 현재 카드 이슈와 일치하지 않아 repo HEAD 폴백을 생략했습니다.",
                "review_target_reject_reason": "latest_work_target_issue_mismatch",
            })
            .to_string(),
        ),
    );

    assert!(message.contains(
        "한 줄 지시: 코드 리뷰만 수행하고 상세 범위와 verdict 규칙은 시스템 프롬프트의 [Current Task]를 따르세요."
    ));
    assert!(!message.contains("브랜치 정보 없음"));
    assert!(!message.contains("리뷰 대상 브랜치:"));
}

#[test]
fn implementation_dispatch_message_stays_compact() {
    let message = format_dispatch_message(
        "dispatch-2",
        "Implement feature",
        Some("https://github.com/itismyfield/AgentDesk/issues/24"),
        Some(24),
        Some("implementation"),
        None,
    );

    assert!(message.starts_with(
        "── implementation dispatch ──\nDISPATCH:dispatch-2 [📋 구현] - #24 Implement feature"
    ));
    assert!(message.contains("<https://github.com/itismyfield/AgentDesk/issues/24>"));
    assert!(message.contains(
        "한 줄 지시: 이 이슈를 구현하고 상세 요구사항과 완료 규칙은 시스템 프롬프트의 [Current Task]를 따르세요."
    ));
    assert!(!message.contains("`OUTCOME: noop`"));
    assert!(!message.contains("검토 전용"));
    assert!(!message.contains("review-verdict"));
    assert!(message.chars().count() <= 500);
}

#[test]
fn e2e_test_dispatch_message_stays_compact() {
    let message = format_dispatch_message(
        "dispatch-e2e",
        "Run regression",
        Some("https://github.com/itismyfield/AgentDesk/issues/340"),
        Some(340),
        Some("e2e-test"),
        None,
    );

    assert!(message.contains("[🧪 E2E 테스트]"));
    assert!(message.contains(
        "한 줄 지시: 검증만 수행하고 상세 기준과 완료 규칙은 시스템 프롬프트의 [Current Task]를 따르세요."
    ));
    assert!(!message.contains("/api/dispatches/"));
    assert!(!message.contains("review-verdict"));
    assert!(message.chars().count() <= 500);
}

#[test]
fn consultation_dispatch_message_stays_compact() {
    let message = format_dispatch_message(
        "dispatch-consult",
        "Need investigation",
        Some("https://github.com/itismyfield/AgentDesk/issues/256"),
        Some(256),
        Some("consultation"),
        None,
    );

    assert!(message.contains("[💬 상담]"));
    assert!(message.contains(
        "한 줄 지시: 필요한 조사/판단만 수행하고 상세 기준과 완료 규칙은 시스템 프롬프트의 [Current Task]를 따르세요."
    ));
    assert!(!message.contains("/api/dispatches/"));
    assert!(!message.contains("review-verdict"));
    assert!(message.chars().count() <= 500);
}

#[test]
fn review_decision_primary_message_includes_action_instructions() {
    let message = format_dispatch_message(
        "dispatch-rd",
        "[리뷰 검토] Test Card",
        Some("https://github.com/itismyfield/AgentDesk/issues/249"),
        Some(249),
        Some("review-decision"),
        Some(
            r#"{
                "verdict":"rework",
                "repo":"owner/repo",
                "issue_number":249,
                "pr_number":366,
                "reviewed_commit":"feedfacecafebeef",
                "decision_endpoint":"POST /api/review-decision"
            }"#,
        ),
    );

    assert!(message.contains("[⚖️ 리뷰 검토]"));
    assert!(message.contains(
        "한 줄 지시: GitHub 리뷰 피드백을 확인하고 accept/dispute/dismiss 중 하나를 제출하세요."
    ));
    assert!(message.contains("대상: repo=owner/repo, issue=#249, pr=#366, commit=feedfacecaf…"));
    assert!(message.contains("제출: `POST /api/review-decision`"));
    assert!(message.contains("<https://github.com/itismyfield/AgentDesk/issues/249>"));
    assert!(!message.contains("카운터모델 리뷰 결과"));
    assert!(!message.contains("review-verdict"));
}

#[test]
fn prefix_dispatch_message_merges_separator_and_body() {
    let message = prefix_dispatch_message("review-decision", "DISPATCH:d-1 - Example");
    assert_eq!(
        message,
        "── review-decision dispatch ──\nDISPATCH:d-1 - Example"
    );
}

#[test]
fn review_verdict_extraction_defaults_to_unknown() {
    // Missing verdict must NOT default to "pass" — that caused false review passes
    assert_eq!(extract_review_verdict(None), "unknown");
    assert_eq!(
        extract_review_verdict(Some(r#"{"auto_completed":true}"#)),
        "unknown"
    );
    assert_eq!(
        extract_review_verdict(Some(r#"{"decision":"dismiss"}"#)),
        "dismiss"
    );
    assert_eq!(
        extract_review_verdict(Some(r#"{"verdict":"improve"}"#)),
        "improve"
    );
    assert_eq!(
        extract_review_verdict(Some(r#"{"verdict":"pass"}"#)),
        "pass"
    );
}

#[tokio::test]
async fn completed_review_dispatch_with_explicit_verdict_creates_followup() {
    // When a review dispatch has an explicit verdict (e.g. "improve"),
    // Rust creates a review-decision dispatch for the original agent.
    let db = test_db();
    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO agents (
                id, name, provider, discord_channel_id, discord_channel_alt, discord_channel_cc, discord_channel_cdx
             ) VALUES ('agent-1', 'Agent 1', 'claude', '123', '456', '123', '456')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO kanban_cards (id, title, status, assigned_agent_id, latest_dispatch_id, created_at, updated_at)
             VALUES ('card-1', 'Needs follow-up', 'review', 'agent-1', 'dispatch-review', datetime('now'), datetime('now'))",
            [],
        )
        .unwrap();
        conn.execute(
            "UPDATE kanban_cards SET github_issue_number = 692, repo_id = ?1 WHERE id = 'card-1'",
            [std::env::current_dir().unwrap().display().to_string()],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO pr_tracking (
                card_id, repo_id, worktree_path, branch, pr_number, head_sha, state, created_at, updated_at
             ) VALUES (
                'card-1', ?1, ?1, 'wt/692-followup', 366, 'feedfacecafebeef', 'review',
                datetime('now'), datetime('now')
             )",
            [std::env::current_dir()
                .unwrap()
                .display()
                .to_string()],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO task_dispatches (id, kanban_card_id, to_agent_id, dispatch_type, status, title, result, context, created_at, updated_at)
             VALUES ('dispatch-review', 'card-1', 'agent-1', 'review', 'completed', '[Review R1] card-1', '{\"verdict\":\"improve\",\"notes\":\"missing null check\",\"items\":[\"handle null\"]}', '{\"from_provider\":\"codex\",\"target_provider\":\"claude\",\"reviewed_commit\":\"feedfacecafebeef\"}', datetime('now'), datetime('now'))",
            [],
        )
        .unwrap();
    }

    handle_completed_dispatch_followups(&db, "dispatch-review")
        .await
        .expect("review followup should succeed");

    let conn = db.lock().unwrap();
    let latest_dispatch_id: String = conn
        .query_row(
            "SELECT latest_dispatch_id FROM kanban_cards WHERE id = 'card-1'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_ne!(latest_dispatch_id, "dispatch-review");
    let (dispatch_type, dispatch_status, context): (String, String, Option<String>) = conn
        .query_row(
            "SELECT dispatch_type, status, context FROM task_dispatches WHERE id = ?1",
            [&latest_dispatch_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(dispatch_type, "review-decision");
    assert_eq!(dispatch_status, "pending");
    let context = context.expect("review-decision should persist provider routing context");
    assert!(context.contains("\"from_provider\":\"codex\""));
    assert!(context.contains("\"reviewed_commit\":\"feedfacecafebeef\""));
    assert!(context.contains("\"issue_number\":692"));
    assert!(context.contains("\"pr_number\":366"));
    assert!(context.contains("\"decision_endpoint\":\"POST /api/review-decision\""));
    assert!(!context.contains("\"notes\""));
    assert!(!context.contains("\"items\""));
}

#[tokio::test]
async fn unknown_review_verdict_followup_includes_target_and_submission_hints() {
    let db = test_db();
    let transport = MockDispatchTransport::default();
    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO agents (id, name, discord_channel_id, discord_channel_alt) VALUES ('agent-1', 'Agent 1', '123', '456')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO kanban_cards (
                id, title, status, assigned_agent_id, github_issue_url, latest_dispatch_id, created_at, updated_at
            ) VALUES (
                'card-unknown', 'Unknown verdict', 'review', 'agent-1',
                'https://github.com/itismyfield/AgentDesk/issues/692', 'dispatch-review',
                datetime('now'), datetime('now')
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "UPDATE kanban_cards SET github_issue_number = 692 WHERE id = 'card-unknown'",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO task_dispatches (
                id, kanban_card_id, to_agent_id, dispatch_type, status, title, context, created_at, updated_at
            ) VALUES (
                'dispatch-review', 'card-unknown', 'agent-1', 'review', 'completed',
                '[Review R1] card-unknown',
                '{\"repo\":\"owner/repo\",\"issue_number\":692,\"pr_number\":366,\"reviewed_commit\":\"feedfacecafebeef\",\"verdict_endpoint\":\"POST /api/review-verdict\"}',
                datetime('now'), datetime('now')
            )",
            [],
        )
        .unwrap();
    }

    super::discord_delivery::send_review_result_to_primary_with_transport(
        &db,
        "card-unknown",
        "dispatch-review",
        "unknown",
        &transport,
    )
    .await
    .expect("unknown verdict followup should be sent");

    let transport_state = transport.state.lock().unwrap();
    assert_eq!(
        transport_state.review_followup_kinds,
        vec![ReviewFollowupKind::Unknown]
    );
    let message = transport_state
        .review_followup_messages
        .first()
        .expect("followup message should be captured");
    assert!(message.contains("대상: repo=owner/repo, issue=#692, pr=#366, commit=feedfacecaf…"));
    assert!(message.contains("누락된 verdict 제출 경로 참고: 제출: `POST /api/review-verdict` (`dispatch_id=dispatch-review`)"));
    assert!(message.contains("https://github.com/itismyfield/AgentDesk/issues/692"));
}

#[test]
fn review_decision_routing_falls_back_to_latest_completed_review_provider() {
    let db = test_db();
    let conn = db.lock().unwrap();
    conn.execute(
        "INSERT INTO agents (
            id, name, provider, discord_channel_id, discord_channel_alt, discord_channel_cc, discord_channel_cdx
         ) VALUES ('agent-1', 'Agent 1', 'claude', '123', '456', '123', '456')",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO kanban_cards (id, title, status, assigned_agent_id, latest_dispatch_id, created_at, updated_at)
         VALUES ('card-route', 'Route test', 'review', 'agent-1', 'dispatch-rd', datetime('now'), datetime('now'))",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO task_dispatches (id, kanban_card_id, to_agent_id, dispatch_type, status, title, context, created_at, updated_at)
         VALUES ('dispatch-review', 'card-route', 'agent-1', 'review', 'completed', '[Review R1] card-route', '{\"from_provider\":\"codex\",\"target_provider\":\"claude\"}', datetime('now', '-1 minute'), datetime('now', '-1 minute'))",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO task_dispatches (id, kanban_card_id, to_agent_id, dispatch_type, status, title, created_at, updated_at)
         VALUES ('dispatch-rd', 'card-route', 'agent-1', 'review-decision', 'pending', '[리뷰 검토] card-route', datetime('now'), datetime('now'))",
        [],
    )
    .unwrap();

    let channel = super::discord_delivery::resolve_dispatch_delivery_channel_on_conn(
        &conn,
        "agent-1",
        "card-route",
        Some("review-decision"),
        None,
    )
    .unwrap();
    assert_eq!(
        channel.as_deref(),
        Some("456"),
        "review-decision should route back to the implementation provider channel"
    );
}

#[tokio::test]
async fn auto_completed_review_dispatch_skips_rust_followup() {
    // When a review dispatch is auto-completed without a verdict,
    // Rust should NOT create a followup (policy engine handles it).
    let db = test_db();
    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO agents (id, name, discord_channel_id, discord_channel_alt) VALUES ('agent-1', 'Agent 1', '123', '456')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO kanban_cards (id, title, status, assigned_agent_id, latest_dispatch_id, created_at, updated_at)
             VALUES ('card-1', 'Auto test', 'review', 'agent-1', 'dispatch-auto', datetime('now'), datetime('now'))",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO task_dispatches (id, kanban_card_id, to_agent_id, dispatch_type, status, title, result, created_at, updated_at)
             VALUES ('dispatch-auto', 'card-1', 'agent-1', 'review', 'completed', '[Review R1] card-1', '{\"auto_completed\":true,\"completion_source\":\"session_idle\"}', datetime('now'), datetime('now'))",
            [],
        )
        .unwrap();
    }

    handle_completed_dispatch_followups(&db, "dispatch-auto")
        .await
        .expect("auto-completed review followup should succeed");

    let conn = db.lock().unwrap();
    let latest_dispatch_id: String = conn
        .query_row(
            "SELECT latest_dispatch_id FROM kanban_cards WHERE id = 'card-1'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    // latest_dispatch_id should remain unchanged — auto-complete with "unknown" verdict skips Rust followup
    assert_eq!(latest_dispatch_id, "dispatch-auto");
}

/// After an implementation dispatch completes, if hooks created a review dispatch
/// (latest_dispatch_id changed), handle_completed_dispatch_followups should detect it
/// and attempt to send it to Discord. This test verifies the detection logic without
/// actually hitting Discord (send_dispatch_to_discord will no-op without a bot token).
#[tokio::test]
async fn impl_dispatch_followup_detects_new_review_dispatch() {
    let db = test_db();
    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO agents (id, name, discord_channel_id, discord_channel_alt) VALUES ('agent-1', 'Agent 1', '123', '456')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO kanban_cards (id, title, status, assigned_agent_id, latest_dispatch_id, created_at, updated_at)
             VALUES ('card-1', 'Impl card', 'review', 'agent-1', 'dispatch-review', datetime('now'), datetime('now'))",
            [],
        )
        .unwrap();
        // The completed implementation dispatch
        conn.execute(
            "INSERT INTO task_dispatches (id, kanban_card_id, to_agent_id, dispatch_type, status, title, result, created_at, updated_at)
             VALUES ('dispatch-impl', 'card-1', 'agent-1', 'implementation', 'completed', 'Impl card', '{\"auto_completed\":true}', datetime('now'), datetime('now'))",
            [],
        )
        .unwrap();
        // The review dispatch created by hooks after implementation completion
        conn.execute(
            "INSERT INTO task_dispatches (id, kanban_card_id, to_agent_id, dispatch_type, status, title, context, created_at, updated_at)
             VALUES ('dispatch-review', 'card-1', 'agent-1', 'review', 'pending', '[Review R1] card-1', '{}', datetime('now'), datetime('now'))",
            [],
        )
        .unwrap();
    }

    // handle_completed_dispatch_followups should detect that latest_dispatch_id
    // ('dispatch-review') differs from the completed dispatch ('dispatch-impl')
    // and attempt send_dispatch_to_discord (which no-ops without bot token).
    // The key assertion: no panic, no error, and the review dispatch stays pending.
    handle_completed_dispatch_followups(&db, "dispatch-impl")
        .await
        .expect("implementation followup should succeed");

    let conn = db.lock().unwrap();
    // latest_dispatch_id should still point to the review dispatch
    let latest: String = conn
        .query_row(
            "SELECT latest_dispatch_id FROM kanban_cards WHERE id = 'card-1'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(latest, "dispatch-review");

    // Review dispatch should remain pending (not modified by followup handler)
    let review_status: String = conn
        .query_row(
            "SELECT status FROM task_dispatches WHERE id = 'dispatch-review'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(review_status, "pending");
}

#[tokio::test]
async fn active_thread_id_preserved_when_card_not_done() {
    // When an implementation dispatch completes but card is in "review" (not done),
    // the thread should NOT be archived — it may be reused for rework/review-decision.
    let db = test_db();
    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO agents (id, name, discord_channel_id) VALUES ('agent-1', 'Agent 1', '123')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO kanban_cards (id, title, status, assigned_agent_id, latest_dispatch_id, active_thread_id)
             VALUES ('card-1', 'In Review', 'review', 'agent-1', 'dispatch-impl', '999888777')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO task_dispatches (id, kanban_card_id, to_agent_id, dispatch_type, status, title, thread_id, created_at, updated_at)
             VALUES ('dispatch-impl', 'card-1', 'agent-1', 'implementation', 'completed', 'card-1', '999888777', datetime('now'), datetime('now'))",
            [],
        )
        .unwrap();
    }

    handle_completed_dispatch_followups(&db, "dispatch-impl")
        .await
        .expect("thread reuse followup should succeed");

    // active_thread_id should still be set (NOT cleared) because card is not done
    let conn = db.lock().unwrap();
    let active_thread: Option<String> = conn
        .query_row(
            "SELECT active_thread_id FROM kanban_cards WHERE id = 'card-1'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(active_thread, Some("999888777".to_string()));
}

#[tokio::test]
async fn active_thread_id_cleared_when_card_done() {
    // When a card reaches "done", active_thread_id should be cleared.
    // (Thread archiving requires Discord API call, but we verify the DB cleanup.)
    let db = test_db();
    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO agents (id, name, discord_channel_id) VALUES ('agent-1', 'Agent 1', '123')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO kanban_cards (id, title, status, assigned_agent_id, latest_dispatch_id, active_thread_id)
             VALUES ('card-1', 'Done Card', 'done', 'agent-1', 'dispatch-final', '999888777')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO task_dispatches (id, kanban_card_id, to_agent_id, dispatch_type, status, title, thread_id, created_at, updated_at)
             VALUES ('dispatch-final', 'card-1', 'agent-1', 'implementation', 'completed', 'card-1', '999888777', datetime('now'), datetime('now'))",
            [],
        )
        .unwrap();
    }

    handle_completed_dispatch_followups(&db, "dispatch-final")
        .await
        .expect("done-card followup should succeed");

    // active_thread_id should be cleared when card is done
    let conn = db.lock().unwrap();
    let active_thread: Option<String> = conn
        .query_row(
            "SELECT active_thread_id FROM kanban_cards WHERE id = 'card-1'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(active_thread.is_none());
}

#[tokio::test]
async fn completed_work_dispatch_posts_summary_before_archiving_thread() {
    let fixture = setup_summary_repo_fixture();
    let (base_url, state, server_handle) = spawn_dispatch_summary_mock_server(true).await;
    let db = test_db();
    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO agents (id, name, discord_channel_id) VALUES ('agent-1', 'Agent 1', '123')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO kanban_cards (
                id, title, status, assigned_agent_id, latest_dispatch_id, active_thread_id, created_at, updated_at
            ) VALUES (
                'card-summary', 'Summary Card', 'done', 'agent-1', 'dispatch-summary', 'thread-summary',
                datetime('now'), datetime('now')
            )",
            [],
        )
        .unwrap();
        let context = serde_json::json!({
            "reviewed_commit": fixture.start_commit,
            "worktree_path": fixture.path,
            "branch": "wt/514-summary"
        });
        let result = serde_json::json!({
            "completed_worktree_path": fixture.path,
            "completed_branch": "wt/514-summary",
            "completed_commit": fixture.end_commit
        });
        conn.execute(
            "INSERT INTO task_dispatches (
                id, kanban_card_id, to_agent_id, dispatch_type, status, title, context, result,
                thread_id, created_at, updated_at, completed_at
            ) VALUES (
                'dispatch-summary', 'card-summary', 'agent-1', 'implementation', 'completed', 'Summary Card',
                ?1, ?2, 'thread-summary', '2026-04-13 12:00:00', '2026-04-13 12:15:00', '2026-04-13 12:15:00'
            )",
            rusqlite::params![context.to_string(), result.to_string()],
        )
        .unwrap();
    }

    let config = DispatchFollowupConfig {
        discord_api_base: base_url,
        notify_bot_token: Some("notify-token".to_string()),
        announce_bot_token: Some("announce-token".to_string()),
    };
    handle_completed_dispatch_followups_with_config(&db, "dispatch-summary", &config)
        .await
        .expect("summary followup should succeed");

    server_handle.abort();
    let state = state.lock().unwrap();
    assert_eq!(
        state.calls,
        vec![
            "GET /channels/thread-summary",
            "PATCH /channels/thread-summary",
            "POST /channels/thread-summary/messages",
            "PATCH /channels/thread-summary",
        ]
    );
    assert_eq!(state.messages.len(), 1, "summary message should be posted");
    assert_eq!(
        state.messages[0],
        "🔔 완료 요약: 2개 파일, +2/-1, 머지 대기\n소요 시간 15분"
    );
    assert_eq!(
        state.patch_payloads,
        vec![
            serde_json::json!({"archived": false}),
            serde_json::json!({"archived": true}),
        ]
    );

    let conn = db.lock().unwrap();
    let active_thread: Option<String> = conn
        .query_row(
            "SELECT active_thread_id FROM kanban_cards WHERE id = 'card-summary'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(
        active_thread.is_none(),
        "done-card followup should clear active_thread_id after posting summary"
    );
}

/// When an explicit review verdict (improve/rework/reject) completes,
/// send_review_result_to_primary creates the review-decision dispatch
/// and sets review_followup_handled=true, preventing duplicate resend
/// via the generic latest_dispatch_id check.
#[tokio::test]
async fn review_followup_skips_generic_resend_for_explicit_verdict() {
    let db = test_db();
    let transport = MockDispatchTransport::default();
    let config = DispatchFollowupConfig {
        discord_api_base: "http://127.0.0.1:9".to_string(),
        notify_bot_token: None,
        announce_bot_token: None,
    };
    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO agents (id, name, discord_channel_id, discord_channel_alt) VALUES ('agent-1', 'Agent 1', '123', '456')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO kanban_cards (id, title, status, assigned_agent_id, latest_dispatch_id, created_at, updated_at)
             VALUES ('card-1', 'Review test', 'review', 'agent-1', 'dispatch-review', datetime('now'), datetime('now'))",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO task_dispatches (id, kanban_card_id, to_agent_id, dispatch_type, status, title, result, created_at, updated_at)
             VALUES ('dispatch-review', 'card-1', 'agent-1', 'review', 'completed', '[Review R1] card-1', '{\"verdict\":\"rework\"}', datetime('now'), datetime('now'))",
            [],
        )
        .unwrap();
    }

    handle_completed_dispatch_followups_with_config_and_transport(
        &db,
        "dispatch-review",
        &config,
        &transport,
    )
    .await
    .expect("explicit review verdict followup should succeed");

    let conn = db.lock().unwrap();
    // A review-decision dispatch should have been created
    let latest_dispatch_id: String = conn
        .query_row(
            "SELECT latest_dispatch_id FROM kanban_cards WHERE id = 'card-1'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_ne!(latest_dispatch_id, "dispatch-review");

    // Count total dispatches — should be exactly 2 (original review + one review-decision).
    // Before this fix, the generic latest_dispatch_id check would call send_dispatch_to_discord
    // again, potentially creating duplicate notifications.
    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM task_dispatches WHERE kanban_card_id = 'card-1'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        count, 2,
        "should have exactly 2 dispatches (review + review-decision), not more"
    );

    let (dt, ds): (String, String) = conn
        .query_row(
            "SELECT dispatch_type, status FROM task_dispatches WHERE id = ?1",
            [&latest_dispatch_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(dt, "review-decision");
    assert_eq!(ds, "pending");

    let transport_state = transport.state.lock().unwrap();
    assert_eq!(
        transport_state.dispatch_calls, 0,
        "explicit verdict followup must not re-send the original dispatch"
    );
    assert!(
        transport_state.review_followup_kinds.is_empty(),
        "explicit verdict followup should enqueue review-decision instead of posting a message"
    );
}

/// When the agent's discord_channel_id points to a non-existent channel,
/// send_dispatch_to_discord must NOT write the notified marker.
/// This ensures that Discord send failures leave the dispatch recoverable
/// by timeouts.js [I-0].
#[tokio::test]
async fn no_notified_marker_when_discord_send_fails() {
    let db = test_db();
    let transport = MockDispatchTransport::failing_dispatch("mock dispatch transport failure");
    {
        let conn = db.lock().unwrap();
        // Use a bogus numeric channel ID that will fail at Discord API
        conn.execute(
            "INSERT INTO agents (id, name, discord_channel_id) VALUES ('agent-1', 'Agent 1', '1')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO kanban_cards (id, title, status, assigned_agent_id, latest_dispatch_id, created_at, updated_at)
             VALUES ('card-1', 'Test card', 'requested', 'agent-1', 'dispatch-1', datetime('now'), datetime('now'))",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO task_dispatches (id, kanban_card_id, to_agent_id, dispatch_type, status, title, created_at, updated_at)
             VALUES ('dispatch-1', 'card-1', 'agent-1', 'implementation', 'pending', 'Test card', datetime('now'), datetime('now'))",
            [],
        )
        .unwrap();
    }

    // Channel ID "1" is a valid u64 but not a real Discord channel.
    // Thread creation and fallback will both fail with Discord API errors.
    // No notified marker should be written.
    let send_result = super::discord_delivery::send_dispatch_to_discord_with_transport(
        &db,
        "agent-1",
        "Test card",
        "card-1",
        "dispatch-1",
        &transport,
    )
    .await;
    assert!(
        send_result.is_err(),
        "mock transport failure should fail delivery"
    );

    let conn = db.lock().unwrap();
    let marker_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM kv_meta WHERE key = 'dispatch_notified:dispatch-1'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        marker_count, 0,
        "notified marker must not be written when Discord send fails"
    );

    // thread_id should also NOT be saved (rollback on failure)
    let thread_id: Option<String> = conn
        .query_row(
            "SELECT thread_id FROM task_dispatches WHERE id = 'dispatch-1'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(
        thread_id.is_none(),
        "thread_id must not be saved when thread message POST fails"
    );

    let reserving_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM kv_meta WHERE key = 'dispatch_reserving:dispatch-1'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        reserving_count, 0,
        "dispatch_reserving marker must be released when delivery fails"
    );

    let transport_state = transport.state.lock().unwrap();
    assert_eq!(transport_state.dispatch_calls, 1);
}

/// send_review_result_to_primary must not create a review-decision dispatch
/// for done cards — the central create_dispatch_core done guard blocks it.
#[tokio::test]
async fn review_followup_does_not_create_dispatch_for_done_card() {
    let db = test_db();
    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO agents (id, name, discord_channel_id, discord_channel_alt) VALUES ('agent-1', 'Agent 1', '123', '456')",
            [],
        ).unwrap();
        conn.execute(
            "INSERT INTO kanban_cards (id, title, status, assigned_agent_id, latest_dispatch_id, created_at, updated_at)
             VALUES ('card-done', 'Done Card', 'done', 'agent-1', 'dispatch-review', datetime('now'), datetime('now'))",
            [],
        ).unwrap();
        conn.execute(
            "INSERT INTO task_dispatches (id, kanban_card_id, to_agent_id, dispatch_type, status, title, result, created_at, updated_at)
             VALUES ('dispatch-review', 'card-done', 'agent-1', 'review', 'completed', '[Review R1]', '{\"verdict\":\"rework\"}', datetime('now'), datetime('now'))",
            [],
        ).unwrap();
    }

    // This triggers send_review_result_to_primary for a done card.
    // The done-card guard should reject creating a review-decision dispatch,
    // but the original dispatch/card state must remain unchanged.
    let result = handle_completed_dispatch_followups(&db, "dispatch-review").await;
    let error = result.expect_err("done-card review followup must fail closed");
    assert!(
        error.contains("Cannot create review-decision dispatch for terminal card"),
        "unexpected followup error: {error}"
    );

    let conn = db.lock().unwrap();
    // latest_dispatch_id should NOT have changed (no new dispatch created)
    let latest: String = conn
        .query_row(
            "SELECT latest_dispatch_id FROM kanban_cards WHERE id = 'card-done'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        latest, "dispatch-review",
        "done card latest_dispatch_id must not be overwritten"
    );

    // Only the original dispatch should exist — no review-decision was created
    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM task_dispatches WHERE kanban_card_id = 'card-done'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        count, 1,
        "no review-decision dispatch should be created for done card"
    );
}

#[tokio::test]
async fn pending_dispatch_lookup_ignores_legacy_auto_queue_run_unified_thread_id() {
    let db = test_db();
    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO agents (id, name, discord_channel_id) VALUES ('agent-1', 'Agent 1', '111222333')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO kanban_cards (
                id, title, status, assigned_agent_id, latest_dispatch_id, created_at, updated_at
            ) VALUES (
                'card-legacy-run', 'Legacy run thread', 'review', 'agent-1',
                'dispatch-review', datetime('now'), datetime('now')
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO task_dispatches (
                id, kanban_card_id, dispatch_type, status, title, created_at, updated_at
            ) VALUES (
                'dispatch-review', 'card-legacy-run', 'review', 'pending',
                '[Review R1] card-legacy-run', datetime('now'), datetime('now')
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_runs (
                id, repo, agent_id, status, unified_thread, unified_thread_id
            ) VALUES (
                'run-legacy', 'test/repo', 'agent-1', 'active', 1,
                '{\"111222333\":\"999888777\"}'
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (
                id, run_id, kanban_card_id, agent_id, dispatch_id, status
            ) VALUES (
                'entry-legacy', 'run-legacy', 'card-legacy-run', 'agent-1',
                'dispatch-review', 'dispatched'
            )",
            [],
        )
        .unwrap();
    }

    let state = AppState::test_state(db.clone(), test_engine(&db));
    let (status, body) = super::get_pending_dispatch_for_thread(
        State(state),
        Query(std::collections::HashMap::from([(
            "thread_id".to_string(),
            "999888777".to_string(),
        )])),
    )
    .await;

    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body.0["error"], "no pending dispatch for thread");
}

#[tokio::test]
async fn pending_dispatch_lookup_finds_review_thread_dispatch() {
    let db = test_db();
    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO kanban_cards (id, title, status, active_thread_id, created_at, updated_at)
             VALUES ('card-review', 'Review card', 'review', '999888777', datetime('now'), datetime('now'))",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO task_dispatches (id, kanban_card_id, dispatch_type, status, title, created_at, updated_at)
             VALUES ('dispatch-review', 'card-review', 'review', 'pending', '[Review R1] card-review', datetime('now'), datetime('now'))",
            [],
        )
        .unwrap();
    }

    let state = AppState::test_state(db.clone(), test_engine(&db));
    let (status, body) = super::get_pending_dispatch_for_thread(
        State(state),
        Query(std::collections::HashMap::from([(
            "thread_id".to_string(),
            "999888777".to_string(),
        )])),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.0["dispatch_id"], "dispatch-review");
}

#[tokio::test]
async fn pending_dispatch_lookup_finds_review_decision_thread_dispatch() {
    let db = test_db();
    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO kanban_cards (id, title, status, channel_thread_map, active_thread_id, created_at, updated_at)
             VALUES ('card-decision', 'Decision card', 'review', '{\"123456789\":\"999888777\"}', '999888777', datetime('now'), datetime('now'))",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO task_dispatches (id, kanban_card_id, dispatch_type, status, title, created_at, updated_at)
             VALUES ('dispatch-review', 'card-decision', 'review', 'completed', '[Review R1] card-decision', datetime('now', '-1 minute'), datetime('now', '-1 minute'))",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO task_dispatches (id, kanban_card_id, dispatch_type, status, title, created_at, updated_at)
             VALUES ('dispatch-review-decision', 'card-decision', 'review-decision', 'pending', '[리뷰 검토] card-decision', datetime('now'), datetime('now'))",
            [],
        )
        .unwrap();
    }

    let state = AppState::test_state(db.clone(), test_engine(&db));
    let (status, body) = super::get_pending_dispatch_for_thread(
        State(state),
        Query(std::collections::HashMap::from([(
            "thread_id".to_string(),
            "999888777".to_string(),
        )])),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.0["dispatch_id"], "dispatch-review-decision");
}
