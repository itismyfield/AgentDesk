use super::*;

#[test]
fn checked_selector_http_ack_and_errors_are_not_best_effort() {
    const CHILD: &str = "ADK_6577_SELECTOR_CHECKED_CHILD";
    const TEST: &str = "services::discord::adk_session::selector::selector_checked_tests::checked_selector_http_ack_and_errors_are_not_best_effort";
    if std::env::var_os(CHILD).is_none() {
        // Isolate the process-global API context from sibling tests and operational services.
        let root = tempfile::tempdir().unwrap();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", TEST, "--nocapture"])
            .env(CHILD, "1")
            .env("AGENTDESK_ROOT_DIR", root.path())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"));
        return;
    }
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            use axum::{Json, Router, extract::State, http::StatusCode, routing::post};
            use std::collections::VecDeque;
            use std::sync::{Arc, Mutex};
            type Reply = (StatusCode, &'static str);
            type Fixture = Arc<Mutex<(VecDeque<Reply>, Vec<serde_json::Value>)>>;
            async fn respond(
                State(state): State<Fixture>,
                Json(body): Json<serde_json::Value>,
            ) -> Reply {
                let mut fixture = state.lock().unwrap();
                fixture.1.push(body);
                fixture.0.pop_front().unwrap()
            }
            let state: Fixture = Arc::new(Mutex::new((VecDeque::new(), Vec::new())));
            let app = Router::new()
                .route("/api/dispatched-sessions/clear-session-id", post(respond))
                .route("/api/dispatched-sessions/webhook", post(respond))
                .with_state(state.clone());
            let listener = tokio::net::TcpListener::bind((crate::config::loopback().as_str(), 0))
                .await
                .unwrap();
            super::super::super::internal_api::init(listener.local_addr().unwrap().port(), None);
            let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
            let queue = |replies: Vec<Reply>| state.lock().unwrap().0.extend(replies);
            queue(vec![
                (StatusCode::OK, "{\"cleared\":1}"),
                (StatusCode::OK, "{\"cleared\":0}"),
            ]);
            clear_provider_session_id_checked("claude/hash/host:pane")
                .await
                .unwrap();
            assert_eq!(
                state.lock().unwrap().1[0]["session_key"],
                "claude/hash/host:pane"
            );
            assert_eq!(state.lock().unwrap().1[1]["session_key"], "host:pane");
            for reply in [
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "{\"error\":\"db failed\"}",
                ),
                (StatusCode::OK, "invalid json"),
                (StatusCode::OK, "{\"cleared\":false}"),
                (StatusCode::OK, "{}"),
            ] {
                queue(vec![reply]);
                assert!(
                    clear_provider_session_id_checked("host:pane")
                        .await
                        .is_err()
                );
            }
            queue(vec![
                (StatusCode::OK, "{\"cleared\":1}"),
                (StatusCode::INTERNAL_SERVER_ERROR, "{}"),
            ]);
            assert!(
                clear_provider_session_id_checked("claude/hash/host:pane")
                    .await
                    .is_err()
            );
            for reply in [
                (StatusCode::INTERNAL_SERVER_ERROR, "{}"),
                (StatusCode::OK, "{\"ok\":false}"),
                (StatusCode::OK, "invalid json"),
                (StatusCode::OK, "{}"),
            ] {
                queue(vec![reply]);
                assert!(
                    save_provider_session_id_checked(
                        "host:pane",
                        "selector-Y",
                        Some("raw-Y"),
                        &ProviderKind::Claude,
                        serenity::ChannelId::new(6577)
                    )
                    .await
                    .is_err()
                );
            }
            queue(vec![(StatusCode::OK, "{\"ok\":true}")]);
            save_provider_session_id_checked(
                "host:pane",
                "selector-Y",
                Some("raw-Y"),
                &ProviderKind::Claude,
                serenity::ChannelId::new(6577),
            )
            .await
            .unwrap();
            let last = state.lock().unwrap().1.last().unwrap().clone();
            assert_eq!(last["claude_session_id"], "selector-Y");
            assert_eq!(last["session_id"], "raw-Y");
            assert_eq!(last["channel_id"], "6577");
            assert_eq!(last["provider"], "claude");
            server.abort();
            let _ = server.await;
            assert!(
                save_provider_session_id_checked(
                    "host:pane",
                    "Y",
                    None,
                    &ProviderKind::Claude,
                    serenity::ChannelId::new(6577)
                )
                .await
                .is_err()
            );
        });
}
