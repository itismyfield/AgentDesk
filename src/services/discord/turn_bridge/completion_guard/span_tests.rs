use super::*;
use std::io::{self, Write};
use std::sync::Mutex;

#[derive(Clone)]
struct LogWriter(Arc<Mutex<Vec<u8>>>);

impl Write for LogWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[test]
fn concurrent_completions_keep_dispatch_spans_poll_scoped() {
    const CHILD: &str = "ADK_4221_COMPLETION_SPAN_CHILD";
    const TEST: &str = "services::discord::turn_bridge::completion_guard::span_tests::concurrent_completions_keep_dispatch_spans_poll_scoped";
    if std::env::var_os(CHILD).is_none() {
        // internal_api::init is process-global; the fixture never replaces a
        // sibling test's API context or contacts an operational API/database.
        let root = tempfile::tempdir().unwrap();
        let result = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", TEST, "--nocapture"])
            .env(CHILD, "1")
            .env("AGENTDESK_ROOT_DIR", root.path())
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr)
        );
        assert!(String::from_utf8_lossy(&result.stdout).contains("1 passed"));
        return;
    }
    let logs = Arc::new(Mutex::new(Vec::new()));
    let writer = LogWriter(logs.clone());
    let subscriber = tracing_subscriber::fmt()
        .without_time()
        .with_ansi(false)
        .with_target(false)
        .with_max_level(tracing::Level::INFO)
        .with_writer(move || writer.clone())
        .finish();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    tracing::subscriber::with_default(subscriber, || runtime.block_on(exercise_completions()));
    let logs = String::from_utf8(logs.lock().unwrap().clone()).unwrap();
    let outside: Vec<_> = logs
        .lines()
        .filter(|line| line.contains("outside completion"))
        .collect();
    assert_eq!(outside.len(), 4, "{logs}");
    for line in outside {
        assert!(
            !line.contains("trace_context"),
            "span leaked across await: {line}"
        );
    }
    let completions: Vec<_> = logs
        .lines()
        .filter(|line| line.contains("explicitly completed dispatch via API"))
        .collect();
    assert_eq!(completions.len(), 2, "{logs}");
    for (id, other) in [("dispatch-A", "dispatch-B"), ("dispatch-B", "dispatch-A")] {
        let event = completions
            .iter()
            .find(|line| line.contains(id))
            .expect("own completion event");
        assert!(!event.contains(other), "foreign dispatch identity: {event}");
        assert!(
            event.contains(&format!("card-{id}")),
            "missing snapshot card: {event}"
        );
        for operation in [
            "complete_work_dispatch_on_turn_end",
            "complete_work_dispatch_snapshot",
        ] {
            assert!(event.contains(operation), "missing operation span: {event}");
        }
    }
}

async fn exercise_completions() {
    use axum::{
        Json, Router,
        extract::{Path, State},
        http::Method,
        routing::any,
    };
    use tokio::sync::Barrier;
    let barriers = Arc::new((Barrier::new(2), Barrier::new(2)));
    let app = Router::new()
        .route(
            "/api/dispatches/{id}",
            any(
                |Path(id): Path<String>,
                 State(barriers): State<Arc<(Barrier, Barrier)>>,
                 method: Method| async move {
                    let (barrier, body) = if method == Method::GET {
                        (
                            &barriers.0,
                            serde_json::json!({"dispatch": {
                                "dispatch_type": "implementation", "status": "dispatched",
                                "kanban_card_id": format!("card-{id}")
                            }}),
                        )
                    } else {
                        assert_eq!(method, Method::PATCH);
                        (&barriers.1, serde_json::json!({}))
                    };
                    barrier.wait().await;
                    tracing::info!("outside completion");
                    Json(body)
                },
            ),
        )
        .with_state(barriers);
    let listener = tokio::net::TcpListener::bind((crate::config::loopback().as_str(), 0))
        .await
        .unwrap();
    crate::services::discord::internal_api::init(listener.local_addr().unwrap().port(), None);
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let shared = crate::services::discord::make_shared_data_for_tests();
    assert!(shared.pg_pool.is_none() && shared.policy.engine.is_none());
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        tokio::join!(
            complete_work_dispatch_on_turn_end(
                &shared,
                Some("dispatch-A"),
                None,
                Some("OUTCOME: noop")
            ),
            complete_work_dispatch_on_turn_end(
                &shared,
                Some("dispatch-B"),
                None,
                Some("OUTCOME: noop")
            ),
        );
    })
    .await
    .expect("both real completions must reach GET and PATCH");
    server.abort();
}
