use axum::body::Bytes;
use axum::http::{Method, StatusCode, Uri};
use axum::response::IntoResponse;
use poise::serenity_prelude::{ChannelId, HttpBuilder};
use std::sync::{Arc, Mutex};

use crate::services::discord::gateway::{DiscordGateway, TurnGateway};
use crate::services::discord::tui_prompt_relay::bridge_gateway::terminal_notice_test_gateway;
use crate::services::discord::turn_end_summary::tests::row;
use crate::services::discord::turn_end_summary::{LiveSummary, post_live};
use crate::services::provider::ProviderKind;
use crate::services::turn_summary_line::Outcome;

#[derive(Clone, Copy, Debug)]
enum Reply {
    RateLimited,
    MalformedLimit,
    GlobalLimit,
    ServerError,
    InvalidJson,
    Redirect,
}

fn message(channel: u64) -> serde_json::Value {
    serde_json::json!({
        "id":"900001", "channel_id":channel.to_string(), "content":"-# ✅ 완료",
        "author":{"id":"1","username":"bot","discriminator":"0001","avatar":null},
        "timestamp":"2026-10-20T00:00:00+00:00","edited_timestamp":null,
        "tts":false,"mention_everyone":false,"mentions":[],"mention_roles":[],
        "attachments":[],"embeds":[],"pinned":false,"type":0
    })
}

struct Recorder {
    http: Arc<poise::serenity_prelude::Http>,
    calls: Calls,
    server: tokio::task::AbortHandle,
}

type Calls = Arc<Mutex<Vec<(Method, String, serde_json::Value)>>>;

impl Recorder {
    async fn start(channel: u64, reply: Reply) -> Self {
        let calls = Calls::default();
        let recorded = calls.clone();
        let app = axum::Router::new().fallback(axum::routing::any(
            move |method: Method, uri: Uri, body: Bytes| {
                let recorded = recorded.clone();
                async move {
                    let first = {
                        let mut calls = recorded.lock().unwrap();
                        calls.push((
                            method,
                            uri.path().into(),
                            serde_json::from_slice(&body).unwrap(),
                        ));
                        calls.len() == 1
                    };
                    // A retry receives a valid message so its extra POST reaches the oracle.
                    if !first {
                        return axum::Json(message(channel)).into_response();
                    }
                    let mut response = match reply {
                        Reply::RateLimited => (
                            StatusCode::TOO_MANY_REQUESTS,
                            axum::Json(serde_json::json!({"code":0,"message":"rate limited"})),
                        )
                            .into_response(),
                        Reply::ServerError => (
                            StatusCode::INTERNAL_SERVER_ERROR,
                            axum::Json(serde_json::json!({"code":0,"message":"uncertain result"})),
                        )
                            .into_response(),
                        Reply::InvalidJson => (StatusCode::OK, "invalid JSON").into_response(),
                        Reply::Redirect => StatusCode::TEMPORARY_REDIRECT.into_response(),
                        Reply::MalformedLimit | Reply::GlobalLimit => {
                            axum::Json(message(channel)).into_response()
                        }
                    };
                    let headers = response.headers_mut();
                    match reply {
                        Reply::RateLimited => {
                            headers.insert("retry-after", "0".parse().unwrap());
                        }
                        Reply::MalformedLimit => {
                            headers.insert("x-ratelimit-limit", "invalid".parse().unwrap());
                        }
                        Reply::GlobalLimit => {
                            headers.insert("x-ratelimit-global", "true".parse().unwrap());
                            headers.insert("retry-after", "0".parse().unwrap());
                        }
                        Reply::Redirect => {
                            headers.insert("location", uri.path().parse().unwrap());
                        }
                        Reply::ServerError | Reply::InvalidJson => {}
                    }
                    response
                }
            },
        ));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let http = Arc::new(
            HttpBuilder::new("terminal-notice-test-token")
                .proxy(format!("http://{}", listener.local_addr().unwrap()))
                .ratelimiter_disabled(true)
                .build(),
        );
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Self {
            http,
            calls,
            server: server.abort_handle(),
        }
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        self.server.abort();
    }
}

#[tokio::test(flavor = "current_thread")]
async fn turn_end_summary_spends_one_physical_post_and_consumes_every_response() {
    let _isolation = Isolation::new();
    let _boot = crate::services::tui_o::cutover::test_override::force_channels(&[]);
    let proxy_env = || {
        let keys = ["HTTPS_PROXY", "https_proxy", "HTTP_PROXY", "http_proxy"];
        let keys = keys
            .into_iter()
            .chain(["ALL_PROXY", "all_proxy", "NO_PROXY", "no_proxy"]);
        keys.map(|key| (key, std::env::var_os(key)))
            .collect::<Vec<_>>()
    };
    let before_trap = proxy_env();
    let trap = EgressTrap::start().await;
    // Premise: the notice client routes a non-loopback host through the trap. `.invalid` never
    // resolves, so even a client that ignored the trap could not reach a server.
    let unroutable = HttpBuilder::new("terminal-notice-test-token")
        .proxy("http://egress-probe.invalid")
        .ratelimiter_disabled(true)
        .build();
    let probe_shared = crate::services::discord::make_shared_data_for_tests();
    let probe = crate::services::discord::outbound::confirmation::send_terminal_notice_http_once(
        &unroutable,
        &probe_shared,
        ChannelId::new(586_799),
        "-# egress probe",
    );
    let probe = tokio::time::timeout(std::time::Duration::from_secs(3), probe).await;
    assert!(probe.unwrap().is_err());
    let seen = trap.take();
    assert!(
        seen.len() == 1 && seen[0].contains("egress-probe.invalid"),
        "the trap must stand in for the network: {seen:?}"
    );
    for (index, (direct, reply)) in [false, true]
        .into_iter()
        .flat_map(|direct| {
            [
                Reply::RateLimited,
                Reply::MalformedLimit,
                Reply::GlobalLimit,
                Reply::ServerError,
                Reply::InvalidJson,
                Reply::Redirect,
            ]
            .into_iter()
            .map(move |reply| (direct, reply))
        })
        .enumerate()
    {
        let channel = 586_700 + index as u64;
        let recorder = Recorder::start(channel, reply).await;
        let shared = crate::services::discord::make_shared_data_for_tests();
        let gateway: Arc<dyn TurnGateway> = if direct {
            terminal_notice_test_gateway(recorder.http.clone(), shared)
        } else {
            Arc::new(DiscordGateway::new(
                recorder.http.clone(),
                shared,
                ProviderKind::Claude,
                None,
            ))
        };
        let summary = LiveSummary::bridge(
            ChannelId::new(channel),
            &row(channel, 1, "strict-terminal-notice"),
            0,
            Outcome::Completed,
            None,
        );
        tokio::time::timeout(
            std::time::Duration::from_secs(3),
            post_live(gateway.as_ref(), &ProviderKind::Claude, summary.clone()),
        )
        .await
        .unwrap();
        assert_eq!(
            trap.take(),
            Vec::<String>::new(),
            "the terminal POST must stay on the loopback recorder for {reply:?} direct={direct}"
        );
        let calls = recorder.calls.lock().unwrap().clone();
        assert_eq!(
            calls.len(),
            1,
            "one physical terminal POST for {reply:?} direct={direct}: {calls:?}"
        );
        assert_eq!(calls[0].0, Method::POST);
        assert_eq!(calls[0].1, format!("/api/v10/channels/{channel}/messages"));
        assert_eq!(calls[0].2["content"], "-# ✅ 완료");
        assert!(calls[0].2.get("nonce").is_none());
        tokio::time::timeout(
            std::time::Duration::from_secs(3),
            post_live(gateway.as_ref(), &ProviderKind::Claude, summary),
        )
        .await
        .unwrap();
        assert_eq!(
            recorder.calls.lock().unwrap().len(),
            1,
            "duplicate producer cannot re-post terminal summary after {reply:?} direct={direct}"
        );
        assert_eq!(
            trap.take(),
            Vec::<String>::new(),
            "no request may leave the loopback recorder for {reply:?} direct={direct}"
        );
    }
    drop(trap);
    // Names only: a proxy value can carry credentials.
    let changed: Vec<&str> = (proxy_env().into_iter().zip(&before_trap))
        .filter(|(now, was)| now != *was)
        .map(|((key, _), _)| key)
        .collect();
    assert!(
        changed.is_empty(),
        "the trap must give these proxy variables back their earlier values: {changed:?}"
    );
}

/// `turn_end_summary::tests::Isolation`, taken here so this file owns the environment lock
/// that its egress trap writes the proxy variables under.
struct Isolation {
    _guards: [crate::config::TestEnvVarGuard; 2],
    _live: crate::config_live_reload::TestLiveGuard,
    _root: tempfile::TempDir,
}

impl Isolation {
    fn new() -> Self {
        let live = crate::config_live_reload::test_live_guard(
            crate::config::test_env_lock::acquire_shared_test_env_lock(),
        );
        let root = tempfile::tempdir().unwrap();
        let config = root.path().join("config.yaml");
        std::fs::write(&config, "server: {}\n").unwrap();
        let set = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock;
        let guards = [
            set("AGENTDESK_ROOT_DIR", root.path()),
            set("AGENTDESK_CONFIG", &config),
        ];
        crate::config_live_reload::install(crate::config::Config::default());
        Self {
            _guards: guards,
            _live: live,
            _root: root,
        }
    }
}

/// The proxy every environment variable names while the test runs: a request that leaves the
/// loopback recorder is caught here and closed instead of reaching the network.
struct EgressTrap {
    seen: Arc<Mutex<Vec<String>>>,
    server: tokio::task::AbortHandle,
    _env: Vec<crate::config::TestEnvVarGuard>,
}

impl EgressTrap {
    /// The caller holds the shared environment lock.
    async fn start() -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let seen = Arc::<Mutex<Vec<String>>>::default();
        let recorded = seen.clone();
        let server = tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let mut head = [0u8; 512];
                let read = tokio::io::AsyncReadExt::read(&mut stream, &mut head).await;
                let line = String::from_utf8_lossy(&head[..read.unwrap_or(0)]);
                recorded
                    .lock()
                    .unwrap()
                    .push(line.lines().next().unwrap_or_default().to_string());
            }
        });
        let set = crate::config::TestEnvVarGuard::set_value_after_shared_test_env_lock;
        let proxies = ["HTTPS_PROXY", "https_proxy", "HTTP_PROXY", "http_proxy"];
        let mut env: Vec<_> = (proxies.into_iter())
            .chain(["ALL_PROXY", "all_proxy"])
            .map(|key| set(key, url.as_ref()))
            .collect();
        for key in ["NO_PROXY", "no_proxy"] {
            env.push(set(key, "127.0.0.1,localhost,::1".as_ref()));
        }
        Self {
            seen,
            server: server.abort_handle(),
            _env: env,
        }
    }

    fn take(&self) -> Vec<String> {
        std::mem::take(&mut *self.seen.lock().unwrap())
    }
}

impl Drop for EgressTrap {
    fn drop(&mut self) {
        self.server.abort();
        // Newest first: on Windows `HTTPS_PROXY` and `https_proxy` are one variable, and only
        // the oldest guard holds the value from before the trap.
        while let Some(guard) = self._env.pop() {
            drop(guard);
        }
    }
}

mod handle_send_contract_tests {
    //! #3038 Phase A characterization tests — pin the `/api/discord/send`
    //! body-parsing 400 responses and the legacy `channel_id` fallback /
    //! `channel:` prefixing behavior of `handle_send` before the health.rs
    //! directory decomposition. All expectations capture current behavior
    //! as-is (no seam; empty `HealthRegistry`).

    use crate::services::discord::health::HealthRegistry;
    use crate::services::message_outbox::source_registry::SendCallerClass;

    use crate::services::discord::outbound::send_api::{handle_send, handle_send_with_caller};

    struct TestRuntimeRoot {
        previous_root: Option<std::ffi::OsString>,
        _temp: tempfile::TempDir,
        _lock: std::sync::MutexGuard<'static, ()>,
    }

    impl TestRuntimeRoot {
        fn new() -> Self {
            let lock = crate::config::shared_test_env_lock()
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            let previous_root = std::env::var_os("AGENTDESK_ROOT_DIR");
            let temp = tempfile::tempdir().expect("temp runtime root"); // agentdesk-audit: allow-unwrap — test setup in #[cfg(test)] mod
            unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", temp.path()) };
            Self {
                previous_root,
                _temp: temp,
                _lock: lock,
            }
        }
    }

    impl Drop for TestRuntimeRoot {
        fn drop(&mut self) {
            match &self.previous_root {
                Some(value) => unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", value) },
                None => unsafe { std::env::remove_var("AGENTDESK_ROOT_DIR") },
            }
        }
    }

    async fn assert_source_gate_body(
        caller_class: SendCallerClass,
        source: &str,
        expected_status: &str,
        expected_error: &str,
    ) {
        let _runtime_root = TestRuntimeRoot::new();
        let registry = HealthRegistry::new();
        let body = serde_json::json!({
            "target": "channel:999999999999999999",
            "content": "hi",
            "source": source,
        })
        .to_string();

        let (status, body) = handle_send_with_caller(&registry, None, &body, caller_class).await;
        assert_eq!(status, expected_status);
        let body: serde_json::Value = serde_json::from_str(&body).unwrap(); // agentdesk-audit: allow-unwrap — test response JSON should parse
        assert_eq!(body["ok"], false);
        assert_eq!(body["error"], expected_error);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn invalid_json_body_is_a_400_with_pinned_body() {
        let registry = HealthRegistry::new();
        let (status, body) = handle_send(&registry, None, "not json").await;
        assert_eq!(status, "400 Bad Request");
        assert_eq!(body, r#"{"ok":false,"error":"invalid JSON"}"#);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn missing_content_is_a_400_with_pinned_body() {
        let _runtime_root = TestRuntimeRoot::new();
        let registry = HealthRegistry::new();
        let (status, body) = handle_send(&registry, None, r#"{"target":"channel:123"}"#).await;
        assert_eq!(status, "400 Bad Request");
        assert_eq!(body, r#"{"ok":false,"error":"content is required"}"#);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn missing_target_is_a_400_invalid_target() {
        let _runtime_root = TestRuntimeRoot::new();
        let registry = HealthRegistry::new();
        let (status, body) = handle_send(&registry, None, r#"{"content":"hi"}"#).await;
        assert_eq!(status, "400 Bad Request");
        let body: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(body["ok"], false);
        assert_eq!(
            body["error"],
            crate::services::discord::outbound::send_target::SEND_TARGET_CONTRACT
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn channel_id_fallback_reaches_target_resolution_like_explicit_target() {
        let _runtime_root = TestRuntimeRoot::new();
        let registry = HealthRegistry::new();
        // Legacy `channel_id` (numeric, no `channel:` prefix) must be accepted
        // via the fallback + prefixing path and produce the exact same
        // downstream response as the explicit `target` spelling.
        let (fallback_status, fallback_body) = handle_send(
            &registry,
            None,
            r#"{"channel_id":"999999999999999999","content":"hi","source":"system"}"#,
        )
        .await;
        let (explicit_status, explicit_body) = handle_send(
            &registry,
            None,
            r#"{"target":"channel:999999999999999999","content":"hi","source":"system"}"#,
        )
        .await;
        assert_eq!(fallback_status, explicit_status);
        assert_eq!(fallback_body, explicit_body);
        // Both spellings clear target parsing and reach the authorization
        // ladder: on an empty registry the unmapped channel is rejected by
        // the role-map gate, not by target parsing.
        assert_eq!(fallback_status, "403 Forbidden");
        assert_eq!(
            fallback_body,
            r#"{"ok":false,"error":"channel not in role-map"}"#
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn cli_caller_can_use_cli_sources_but_not_system() {
        assert_source_gate_body(
            SendCallerClass::Cli,
            "agentdesk-cli",
            "403 Forbidden",
            "channel not in role-map",
        )
        .await;
        assert_source_gate_body(
            SendCallerClass::Cli,
            "operator",
            "403 Forbidden",
            "channel not in role-map",
        )
        .await;
        assert_source_gate_body(
            SendCallerClass::Cli,
            "system",
            "403 Forbidden",
            "source not allowed for this caller",
        )
        .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn dashboard_unknown_and_loopback_use_caller_specific_source_gate() {
        assert_source_gate_body(
            SendCallerClass::Dashboard,
            "dashboard",
            "403 Forbidden",
            "channel not in role-map",
        )
        .await;
        assert_source_gate_body(
            SendCallerClass::Dashboard,
            "headless_turn",
            "403 Forbidden",
            "source not allowed for this caller",
        )
        .await;
        assert_source_gate_body(
            SendCallerClass::Unknown,
            "slo_alerter",
            "403 Forbidden",
            "source not allowed for this caller",
        )
        .await;
        assert_source_gate_body(
            SendCallerClass::LoopbackInternal,
            "headless_turn",
            "403 Forbidden",
            "channel not in role-map",
        )
        .await;
    }
}

mod manual_delivery_api_contract_tests {
    use crate::services::discord::health::HealthRegistry;
    use crate::services::discord::outbound::send_api::handle_send;

    #[tokio::test]
    async fn api_send_rejects_user_supplied_message_outbox_delivery_id_namespace() {
        let registry = HealthRegistry::new();

        let body = serde_json::json!({
            "target": "channel:9000",
            "content": "forged internal delivery",
            "source": "headless_turn",
            "bot": "notify",
            "correlation_id": "message_outbox:headless_turn:terminal:forged",
            "semantic_event_id": "message_outbox:41:deliver"
        })
        .to_string();
        let (status, body) = handle_send(&registry, None, &body).await;
        let response: serde_json::Value = serde_json::from_str(&body).unwrap();

        assert_eq!(status, "400 Bad Request");
        assert_eq!(response["ok"], false);
        assert_eq!(
            response["error"],
            "delivery_id correlation namespace is reserved"
        );
    }
}

mod send_gate_host_tests {
    use crate::services::discord::outbound::send_gate;
    use crate::services::provider::ProviderKind;
    #[tokio::test]
    async fn provider_notice_uses_channel_owner_tool_card_credentials() {
        use crate::services::discord::{health::HealthRegistry, make_shared_data_for_tests};
        use std::sync::Arc;
        let registry = HealthRegistry::new();
        let first = make_shared_data_for_tests();
        let owner = make_shared_data_for_tests();
        first.settings.write().await.allowed_channel_ids = vec![41];
        owner.settings.write().await.allowed_channel_ids = vec![42];
        first
            .http
            .cached_bot_token
            .set("first-token".into())
            .unwrap();
        owner
            .http
            .cached_bot_token
            .set("owner-token".into())
            .unwrap();
        registry.register("claude".into(), Arc::clone(&first)).await;
        registry.register("claude".into(), Arc::clone(&owner)).await;
        registry
            .register_http(
                "claude".into(),
                first.serenity_http_or_token_fallback().unwrap(),
            )
            .await;
        let options = send_gate::ManualOutboundOptions {
            trusted_internal_outbox: true,
            channel_owner_provider: Some(ProviderKind::Claude),
            ..Default::default()
        };
        let http = send_gate::resolve_manual_bot_http(
            &registry,
            "claude",
            poise::serenity_prelude::ChannelId::new(42),
            &options,
        )
        .await
        .unwrap();
        assert!(http.token().ends_with("owner-token"));
        first.settings.write().await.allowed_channel_ids.push(42);
        assert_eq!(
            send_gate::resolve_manual_bot_http(
                &registry,
                "claude",
                poise::serenity_prelude::ChannelId::new(42),
                &options
            )
            .await
            .unwrap_err()
            .0,
            "503 Service Unavailable"
        );
        assert!(
            send_gate::resolve_manual_bot_http(
                &HealthRegistry::new(),
                "claude",
                poise::serenity_prelude::ChannelId::new(42),
                &options
            )
            .await
            .is_err()
        );
    }
}
