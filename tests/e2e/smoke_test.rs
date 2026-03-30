//! E2E Smoke Test — Server lifecycle + API CRUD verification
//!
//! Tests the full HTTP lifecycle: health → agents → kanban → dispatches → settings → cleanup.
//! Spawns the real agentdesk binary on a random port with an isolated temp database.

use std::process::{Child, Command, Stdio};
use std::time::Duration;

/// A running AgentDesk server process for testing.
struct TestServer {
    child: Child,
    port: u16,
    _temp_dir: tempfile::TempDir,
}

impl TestServer {
    /// Start an isolated AgentDesk server on a random available port.
    fn start() -> Self {
        let port = get_free_port();
        let temp_dir = tempfile::tempdir().expect("failed to create temp dir");
        let data_dir = temp_dir.path().join("data");
        std::fs::create_dir_all(&data_dir).expect("failed to create data dir");

        // Resolve the policies directory relative to the project root
        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        let policies_dir = std::path::Path::new(manifest_dir).join("policies");

        // Write a minimal config file
        let config_path = temp_dir.path().join("agentdesk.yaml");
        let config_content = format!(
            r#"server:
  port: {port}
  host: "127.0.0.1"
discord: {{}}
agents: []
github:
  repos: []
  sync_interval_minutes: 0
policies:
  dir: "{policies}"
  hot_reload: false
data:
  dir: "{data}"
  db_name: "test.sqlite"
"#,
            port = port,
            policies = policies_dir.display(),
            data = data_dir.display(),
        );
        std::fs::write(&config_path, &config_content).expect("failed to write test config");

        // Spawn the server binary
        let binary = env!("CARGO_BIN_EXE_agentdesk");
        let child = Command::new(binary)
            .env("AGENTDESK_CONFIG", &config_path)
            .env("AGENTDESK_ROOT_DIR", temp_dir.path())
            .env("RUST_LOG", "agentdesk=warn")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("failed to start agentdesk binary");

        TestServer {
            child,
            port,
            _temp_dir: temp_dir,
        }
    }

    fn base_url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    fn api_url(&self, path: &str) -> String {
        format!("{}/api{}", self.base_url(), path)
    }

    /// Poll health endpoint until the server is ready (max 10 seconds).
    async fn wait_ready(&self) {
        let client = reqwest::Client::new();
        let url = self.api_url("/health");

        for _ in 0..100 {
            match client.get(&url).send().await {
                Ok(resp) if resp.status().is_success() => return,
                _ => tokio::time::sleep(Duration::from_millis(100)).await,
            }
        }
        panic!(
            "Server did not become ready within 10 seconds on port {}",
            self.port
        );
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn get_free_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("failed to bind for port");
    listener.local_addr().unwrap().port()
}

// ── Smoke Test ──────────────────────────────────────────────────

#[tokio::test]
async fn smoke_test_full_lifecycle() {
    let server = TestServer::start();
    server.wait_ready().await;
    let client = reqwest::Client::new();

    // ── 1. Health Check ─────────────────────────────────────────
    {
        let resp = client.get(server.api_url("/health")).send().await.unwrap();
        assert_eq!(resp.status(), 200, "health check should return 200");
        let body: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(body["ok"], true);
        assert_eq!(body["db"], true);
    }

    // ── 2. Agent CRUD ───────────────────────────────────────────

    // 2a. Empty agent list
    {
        let resp = client.get(server.api_url("/agents")).send().await.unwrap();
        assert_eq!(resp.status(), 200);
        let body: serde_json::Value = resp.json().await.unwrap();
        assert!(
            body["agents"].as_array().unwrap().is_empty(),
            "initial agents should be empty"
        );
    }

    // 2b. Create agent
    let agent_id = "test-agent-1";
    {
        let resp = client
            .post(server.api_url("/agents"))
            .json(&serde_json::json!({
                "id": agent_id,
                "name": "Test Agent",
                "provider": "claude",
            }))
            .send()
            .await
            .unwrap();
        assert!(
            resp.status().is_success(),
            "create agent should succeed: {}",
            resp.status()
        );
    }

    // 2c. List agents — should have 1
    {
        let resp = client.get(server.api_url("/agents")).send().await.unwrap();
        assert_eq!(resp.status(), 200);
        let body: serde_json::Value = resp.json().await.unwrap();
        let agents = body["agents"].as_array().unwrap();
        assert_eq!(agents.len(), 1, "should have exactly 1 agent");
        assert_eq!(agents[0]["id"], agent_id);
        assert_eq!(agents[0]["name"], "Test Agent");
    }

    // 2d. Get single agent
    {
        let resp = client
            .get(server.api_url(&format!("/agents/{agent_id}")))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let body: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(body["agent"]["id"], agent_id);
    }

    // 2e. Update agent
    {
        let resp = client
            .patch(server.api_url(&format!("/agents/{agent_id}")))
            .json(&serde_json::json!({
                "name": "Updated Agent",
            }))
            .send()
            .await
            .unwrap();
        assert!(resp.status().is_success(), "update agent should succeed");

        // Verify update
        let resp = client
            .get(server.api_url(&format!("/agents/{agent_id}")))
            .send()
            .await
            .unwrap();
        let body: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(body["agent"]["name"], "Updated Agent");
    }

    // ── 3. Kanban Card CRUD ─────────────────────────────────────

    // 3a. Empty card list
    {
        let resp = client
            .get(server.api_url("/kanban-cards"))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let body: serde_json::Value = resp.json().await.unwrap();
        assert!(
            body["cards"].as_array().unwrap().is_empty(),
            "initial cards should be empty"
        );
    }

    // 3b. Create card
    let card_id;
    {
        let resp = client
            .post(server.api_url("/kanban-cards"))
            .json(&serde_json::json!({
                "title": "Test Card: Implement Feature X",
                "priority": "high",
            }))
            .send()
            .await
            .unwrap();
        assert!(
            resp.status().is_success(),
            "create card should succeed: {}",
            resp.status()
        );
        let body: serde_json::Value = resp.json().await.unwrap();
        card_id = body["card"]["id"]
            .as_str()
            .expect("card id should be a string")
            .to_string();
        assert!(!card_id.is_empty(), "card id should not be empty");
    }

    // 3c. List cards — should have 1
    {
        let resp = client
            .get(server.api_url("/kanban-cards"))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let body: serde_json::Value = resp.json().await.unwrap();
        let cards = body["cards"].as_array().unwrap();
        assert_eq!(cards.len(), 1, "should have exactly 1 card");
        assert_eq!(cards[0]["title"], "Test Card: Implement Feature X");
        assert_eq!(cards[0]["priority"], "high");
    }

    // 3d. Get single card
    {
        let resp = client
            .get(server.api_url(&format!("/kanban-cards/{card_id}")))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let body: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(body["card"]["id"], card_id.as_str());
    }

    // 3e. Update card — assign agent
    {
        let resp = client
            .patch(server.api_url(&format!("/kanban-cards/{card_id}")))
            .json(&serde_json::json!({
                "assigned_agent_id": agent_id,
                "status": "ready",
            }))
            .send()
            .await
            .unwrap();
        assert!(resp.status().is_success(), "update card should succeed");
    }

    // ── 4. Dispatch Lifecycle ───────────────────────────────────

    // 4a. Create dispatch
    let dispatch_id;
    {
        let resp = client
            .post(server.api_url("/dispatches"))
            .json(&serde_json::json!({
                "kanban_card_id": card_id,
                "to_agent_id": agent_id,
                "title": "Implement Feature X",
                "dispatch_type": "implementation",
            }))
            .send()
            .await
            .unwrap();
        assert!(
            resp.status().is_success(),
            "create dispatch should succeed: {}",
            resp.status()
        );
        let body: serde_json::Value = resp.json().await.unwrap();
        dispatch_id = body["dispatch"]["id"]
            .as_str()
            .expect("dispatch id should be a string")
            .to_string();
        assert!(!dispatch_id.is_empty(), "dispatch id should not be empty");
    }

    // 4b. List dispatches
    {
        let resp = client
            .get(server.api_url("/dispatches"))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let body: serde_json::Value = resp.json().await.unwrap();
        let dispatches = body["dispatches"].as_array().unwrap();
        assert!(!dispatches.is_empty(), "should have at least 1 dispatch");
    }

    // 4c. Get single dispatch
    {
        let resp = client
            .get(server.api_url(&format!("/dispatches/{dispatch_id}")))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let body: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(body["dispatch"]["id"], dispatch_id.as_str());
        assert_eq!(body["dispatch"]["status"], "pending");
    }

    // 4d. Update dispatch status
    {
        let resp = client
            .patch(server.api_url(&format!("/dispatches/{dispatch_id}")))
            .json(&serde_json::json!({
                "status": "completed",
                "result": { "summary": "Feature X implemented successfully" },
            }))
            .send()
            .await
            .unwrap();
        assert!(resp.status().is_success(), "update dispatch should succeed");

        // Verify status change
        let resp = client
            .get(server.api_url(&format!("/dispatches/{dispatch_id}")))
            .send()
            .await
            .unwrap();
        let body: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(body["dispatch"]["status"], "completed");
    }

    // ── 5. Settings ─────────────────────────────────────────────

    // 5a. Get settings (should return empty or default)
    {
        let resp = client
            .get(server.api_url("/settings"))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
    }

    // 5b. Put settings
    {
        let resp = client
            .put(server.api_url("/settings"))
            .json(&serde_json::json!({
                "theme": "dark",
                "language": "ko",
            }))
            .send()
            .await
            .unwrap();
        assert!(resp.status().is_success(), "put settings should succeed");

        // Verify
        let resp = client
            .get(server.api_url("/settings"))
            .send()
            .await
            .unwrap();
        let body: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(body["theme"], "dark");
        assert_eq!(body["language"], "ko");
    }

    // ── 6. Stats ────────────────────────────────────────────────
    {
        let resp = client.get(server.api_url("/stats")).send().await.unwrap();
        assert_eq!(resp.status(), 200);
    }

    // ── 7. Error Cases ──────────────────────────────────────────

    // 7a. Get non-existent agent — returns 200 with error field (API convention)
    {
        let resp = client
            .get(server.api_url("/agents/nonexistent-id"))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let body: serde_json::Value = resp.json().await.unwrap();
        assert!(
            body.get("error").is_some(),
            "non-existent agent should have error field"
        );
    }

    // 7b. Get non-existent card — returns 404
    {
        let resp = client
            .get(server.api_url("/kanban-cards/nonexistent-id"))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 404, "non-existent card should return 404");
    }

    // ── 8. Cleanup ──────────────────────────────────────────────
    // Note: card has FK references from dispatches, so we can only delete
    // resources without dependents in this test.

    // Delete agent (card still references it via assigned_agent_id, which
    // is nullable, so this should succeed depending on FK setup)
    {
        let resp = client
            .delete(server.api_url(&format!("/agents/{agent_id}")))
            .send()
            .await
            .unwrap();
        let status = resp.status();
        let body: serde_json::Value = resp.json().await.unwrap_or(serde_json::json!({}));
        // Agent deletion may fail if FK constraints reference it — that's OK
        // The important thing is the API handles it gracefully (no panic/crash)
        if status.is_success() {
            // Verify cleanup
            let resp = client.get(server.api_url("/agents")).send().await.unwrap();
            let body: serde_json::Value = resp.json().await.unwrap();
            assert!(
                body["agents"].as_array().unwrap().is_empty(),
                "agents should be empty after cleanup"
            );
        } else {
            // FK constraint prevented deletion — verify server is still healthy
            assert_eq!(status.as_u16(), 500, "FK error should be 500: {body}");
            let resp = client.get(server.api_url("/health")).send().await.unwrap();
            assert_eq!(
                resp.status(),
                200,
                "server should still be healthy after FK error"
            );
        }
    }
}
