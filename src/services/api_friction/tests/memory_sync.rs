use super::super::*;
use super::helpers::{TestPostgresDb, restore_env};
use crate::services::discord::settings::{MemoryBackendKind, ResolvedMemorySettings};
use serde_json::json;
use std::fs;

#[derive(Clone)]
struct MockHttpResponse {
    status_line: &'static str,
    headers: Vec<(&'static str, &'static str)>,
    body: String,
}

async fn spawn_response_sequence_server(
    responses: Vec<MockHttpResponse>,
) -> (
    String,
    tokio::sync::oneshot::Receiver<Vec<String>>,
    tokio::task::JoinHandle<()>,
) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (requests_tx, requests_rx) = tokio::sync::oneshot::channel();
    let handle = tokio::spawn(async move {
        let mut requests = Vec::new();
        for response in responses {
            let Ok((mut stream, _)) = listener.accept().await else {
                break;
            };
            let mut buf = [0u8; 32768];
            let n = stream.read(&mut buf).await.unwrap_or(0);
            requests.push(String::from_utf8_lossy(&buf[..n]).to_string());

            let mut raw_response = format!(
                "HTTP/1.1 {}\r\nContent-Length: {}\r\nContent-Type: application/json\r\nConnection: close\r\n",
                response.status_line,
                response.body.len()
            );
            for (header, value) in response.headers {
                raw_response.push_str(&format!("{header}: {value}\r\n"));
            }
            raw_response.push_str("\r\n");
            raw_response.push_str(&response.body);

            let _ = stream.write_all(raw_response.as_bytes()).await;
            let _ = stream.shutdown().await;
        }
        let _ = requests_tx.send(requests);
    });

    (format!("http://{}", addr), requests_rx, handle)
}

#[tokio::test]
async fn record_api_friction_reports_syncs_to_memento() {
    let initialize_response = serde_json::to_string(&json!({
        "jsonrpc": "2.0",
        "id": 1,
        "result": {
            "protocolVersion": "2025-11-25"
        }
    }))
    .unwrap();
    let remember_response = serde_json::to_string(&json!({
        "jsonrpc": "2.0",
        "id": 2,
        "result": {
            "content": [
                {
                    "type": "text",
                    "text": serde_json::to_string(&json!({"usage": {"input_tokens": 8, "output_tokens": 3}})).unwrap()
                }
            ],
            "isError": false
        }
    }))
    .unwrap();
    let (base_url, requests_rx, handle) = spawn_response_sequence_server(vec![
        MockHttpResponse {
            status_line: "200 OK",
            headers: vec![("MCP-Session-Id", "session-1")],
            body: initialize_response,
        },
        MockHttpResponse {
            status_line: "200 OK",
            headers: vec![("MCP-Session-Id", "session-1")],
            body: remember_response,
        },
    ])
    .await;

    let _guard = crate::services::discord::runtime_store::lock_test_env();
    let temp = tempfile::tempdir().unwrap();
    let previous_root = std::env::var_os("AGENTDESK_ROOT_DIR");
    let previous_key = std::env::var_os("MEMENTO_TEST_KEY");
    let config_dir = temp.path().join("config");
    fs::create_dir_all(&config_dir).unwrap();
    fs::write(
        config_dir.join("agentdesk.yaml"),
        format!(
            "server:\n  port: 8791\nmemory:\n  backend: memento\n  mcp:\n    endpoint: {base_url}\n    access_key_env: MEMENTO_TEST_KEY\n"
        ),
    )
    .unwrap();
    unsafe {
        std::env::set_var("AGENTDESK_ROOT_DIR", temp.path());
        std::env::set_var("MEMENTO_TEST_KEY", "memento-key");
    }

    let pg_db = TestPostgresDb::create().await;
    let pg_pool = pg_db.connect_and_migrate().await;
    let db = crate::db::test_db();
    let result = record_api_friction_reports(
        Some(&db),
        Some(&pg_pool),
        &ResolvedMemorySettings {
            backend: MemoryBackendKind::Memento,
            ..ResolvedMemorySettings::default()
        },
        ApiFrictionRecordContext {
            channel_id: 1,
            session_key: Some("host:session"),
            dispatch_id: None,
            provider: "codex",
        },
        &[ApiFrictionReport {
            endpoint: "/api/docs/kanban".to_string(),
            friction_type: "docs-bypass".to_string(),
            summary: "category guessing".to_string(),
            workaround: Some("sqlite3".to_string()),
            suggested_fix: Some("document a single endpoint".to_string()),
            docs_category: Some("kanban".to_string()),
            keywords: vec!["/api/docs/kanban".to_string(), "sqlite3".to_string()],
        }],
    )
    .await
    .unwrap();

    handle.abort();
    restore_env("AGENTDESK_ROOT_DIR", previous_root);
    restore_env("MEMENTO_TEST_KEY", previous_key);

    assert_eq!(result.stored_event_count, 1);
    assert_eq!(result.memory_stored_count, 1);
    assert_eq!(result.token_usage.input_tokens, 8);
    assert_eq!(result.token_usage.output_tokens, 3);

    let requests = requests_rx.await.unwrap();
    assert!(requests[1].contains("\"name\":\"remember\""));
    assert!(requests[1].contains("\"topic\":\"api-friction\""));
    assert!(requests[1].contains("\"type\":\"error\""));

    let memory_status: String =
        sqlx::query_scalar("SELECT memory_status FROM api_friction_events LIMIT 1")
            .fetch_one(&pg_pool)
            .await
            .unwrap();
    assert_eq!(memory_status, "stored");
    pg_pool.close().await;
    pg_db.drop().await;
}
