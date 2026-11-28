use std::path::Path;
use std::sync::{Arc, Mutex};

use axum::extract::Path as RoutePath;
use axum::http::StatusCode;
use axum::routing::patch;
use axum::{Json, Router};
use serde_json::{Value, json};

use crate::config::TestEnvVarGuard;

pub(super) struct ArchiveFixture {
    patches: Arc<Mutex<Vec<u64>>>,
    server: tokio::task::JoinHandle<()>,
    _base: TestEnvVarGuard,
}

impl ArchiveFixture {
    pub(super) async fn install(root: &Path) -> Self {
        let token = crate::runtime_layout::credential_token_path(
            root,
            crate::domain::bot_role::UtilityBotRole::Announce.alias(),
        );
        std::fs::create_dir_all(token.parent().unwrap()).unwrap();
        std::fs::write(token, "fixture-announce-token").unwrap();

        let patches = Arc::new(Mutex::new(Vec::new()));
        let received = patches.clone();
        let app = Router::new().route(
            "/channels/{thread}",
            patch(
                move |RoutePath(thread): RoutePath<u64>, Json(body): Json<Value>| {
                    let received = received.clone();
                    async move {
                        if body != json!({"archived": true}) {
                            return StatusCode::BAD_REQUEST;
                        }
                        received.lock().unwrap().push(thread);
                        StatusCode::NO_CONTENT
                    }
                },
            ),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let base = TestEnvVarGuard::set_value_after_shared_test_env_lock(
            "AGENTDESK_DISCORD_API_BASE_URL",
            base.as_ref(),
        );
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Self {
            patches,
            server,
            _base: base,
        }
    }

    pub(super) fn take_patches(&self) -> Vec<u64> {
        std::mem::take(&mut *self.patches.lock().unwrap())
    }
}

impl Drop for ArchiveFixture {
    fn drop(&mut self) {
        self.server.abort();
    }
}
