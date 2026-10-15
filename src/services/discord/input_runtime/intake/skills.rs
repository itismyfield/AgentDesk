//! Text skill provenance and protected admission before Legacy command effects.

use crate::services::discord::commands::build_provider_skill_prompt;
use crate::services::discord::{Data, Error, SharedData};
use poise::serenity_prelude as serenity;

fn skill_is_control(skill: &str) -> bool {
    matches!(
        skill,
        "clear" | "stop" | "pwd" | "health" | "status" | "inflight" | "help"
    )
}

pub(in crate::services::discord) async fn protected_text_skill_source(
    shared: &std::sync::Arc<SharedData>,
    raw: &str,
) -> Option<Result<String, Error>> {
    let raw = raw.trim_start();
    let raw = if raw.starts_with("<@")
        && crate::services::discord::catch_up::handled_command::is_text_command(raw)
    {
        raw.split_once('>')?.1.trim_start()
    } else {
        raw
    };
    let mut parts = raw.splitn(3, char::is_whitespace);
    let command = parts.next()?;
    if !matches!(command, "!skill" | "!cc") {
        return None;
    }
    let skill = parts.next().unwrap_or("");
    if skill.is_empty() || skill_is_control(skill) {
        return Some(Err(
            "skill control aliases require their control handler".into()
        ));
    }
    let args = raw
        .strip_prefix(command)?
        .trim()
        .strip_prefix(skill)?
        .trim();
    let registered = shared
        .skills_cache
        .read()
        .await
        .iter()
        .any(|(name, _)| name == skill);
    Some(if registered {
        build_provider_skill_prompt(&shared.provider, skill, args).map_err(Into::into)
    } else {
        Err(format!(
            "Unknown skill: `{skill}`. Use `!skill` to see available skills. `!cc` still works."
        )
        .into())
    })
}

pub(in crate::services::discord) async fn hold_text_skill(
    ctx: &serenity::Context,
    message: &serenity::Message,
    data: &Data,
    skill: &str,
    args: &str,
) -> Result<bool, Error> {
    use crate::services::discord::input_runtime::{fence, intake::admit::SourceKind};
    #[cfg(test)]
    if crate::services::discord::input_runtime::supervisor::mutant("c2b_skill_early_guard_removed")
    {
        return Ok(false);
    }
    if !fence::any_protected() {
        return Ok(false);
    }
    let prompt = protected_text_skill_source(&data.shared, &message.content)
        .await
        .unwrap_or_else(|| {
            build_provider_skill_prompt(&data.provider, skill, args).map_err(Into::into)
        });
    let kind = if prompt.is_ok() {
        SourceKind::TextSkill
    } else {
        SourceKind::LiveMessage
    };
    if !crate::services::discord::router::message_handler::IntakeDeps::hold_live(
        &data.shared,
        &ctx.http,
        &data.provider,
        message,
        kind,
        Some(
            prompt
                .as_ref()
                .cloned()
                .unwrap_or_else(|_| message.content.clone()),
        ),
    )
    .await?
    {
        return Ok(false);
    }
    if let Err(message_text) = prompt {
        message.reply(&ctx.http, message_text.to_string()).await?;
    }
    Ok(true)
}

#[cfg(all(test, unix))]
mod c2b_skill_tests {
    use super::*;
    use crate::services::discord::tui_prompt_relay::relay_e2e::discord_mock;
    use crate::services::provider::ProviderKind;
    use axum::{
        body::Body,
        extract::State,
        http::{Request, StatusCode},
        response::Response,
        routing::post,
    };
    use serde_json::Value;
    use std::sync::{Arc, Mutex};

    #[derive(Clone)]
    struct TransportState {
        backend: String,
        callbacks: Arc<Mutex<Vec<Value>>>,
        client: reqwest::Client,
    }
    async fn callback(
        State(state): State<TransportState>,
        axum::Json(body): axum::Json<Value>,
    ) -> StatusCode {
        state.callbacks.lock().unwrap().push(body);
        StatusCode::NO_CONTENT
    }
    async fn relay(State(state): State<TransportState>, request: Request<Body>) -> Response {
        let (parts, body) = request.into_parts();
        let body = axum::body::to_bytes(body, 1024 * 1024).await.unwrap();
        let response = state
            .client
            .request(
                parts.method,
                format!("{}{}", state.backend, parts.uri.path_and_query().unwrap()),
            )
            .headers(parts.headers)
            .body(body)
            .send()
            .await
            .unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        let mut result = Response::new(Body::from(response.bytes().await.unwrap()));
        *result.status_mut() = status;
        *result.headers_mut() = headers;
        result
    }
    struct Transport {
        ctx: serenity::Context,
        mock: discord_mock::DiscordMockState,
        backend: tokio::task::JoinHandle<()>,
        frontend: tokio::task::JoinHandle<()>,
    }
    impl Drop for Transport {
        fn drop(&mut self) {
            self.frontend.abort();
            self.backend.abort();
        }
    }
    impl Transport {
        async fn new(channel: u64) -> Self {
            let mock = discord_mock::DiscordMockState::new();
            mock.allow_channel(channel);
            let (backend_url, gateway, backend) = discord_mock::start(mock.clone()).await;
            let callbacks = Arc::new(Mutex::new(Vec::new()));
            let state = TransportState {
                backend: backend_url,
                callbacks: callbacks.clone(),
                client: reqwest::Client::builder().no_proxy().build().unwrap(),
            };
            let app = axum::Router::new()
                .route(
                    "/api/v10/interactions/{id}/{token}/callback",
                    post(callback),
                )
                .fallback(relay)
                .with_state(state);
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let base = format!("http://{}", listener.local_addr().unwrap());
            let frontend = tokio::spawn(async move {
                axum::serve(listener, app).await.unwrap();
            });
            let ctx = discord_mock::serenity_context(base.clone(), gateway).await;
            Self {
                ctx,
                mock,
                backend,
                frontend,
            }
        }
    }
    async fn data() -> Data {
        let shared = crate::services::discord::make_shared_data_for_tests();
        shared.settings.write().await.owner_user_id = Some(6_325_211);
        let voice_config = crate::voice::VoiceConfig::default();
        Data {
            shared,
            token: "test-token".into(),
            provider: ProviderKind::Claude,
            voice_receiver: crate::voice::VoiceReceiver::from_voice_config(&voice_config),
            voice_config,
            ingress_shadow: None,
        }
    }
    #[test]
    fn c2b_actual_text_skill_aliases_hold_original_before_confirmation_and_keep_controls() {
        let _lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let root = tempfile::tempdir().unwrap();
                let config = root.path().join("isolated-config.yml");
                std::fs::write(&config, "server: {}\ndata: {dir: data}\n").unwrap();
                let _root = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
                    "AGENTDESK_ROOT_DIR",
                    root.path(),
                );
                let _config = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
                    "AGENTDESK_CONFIG",
                    &config,
                );
                let channel = 6_325_225;
                let transport = Transport::new(channel).await;
                let data = data().await;
                *data.shared.skills_cache.write().await =
                    vec![("local-skill".into(), "fixture".into())];
                let _gate = crate::services::discord::input_runtime::fence::Gate::protect(
                    ProviderKind::Claude,
                    channel,
                )
                .unwrap();
                for command in ["!skill", "!cc"] {
                    let mut original = discord_mock::user_message(
                        6_325_226,
                        &format!("{command} local-skill exact args"),
                    );
                    original.channel_id = serenity::ChannelId::new(channel);
                    original.author.id = serenity::UserId::new(6_325_211);
                    let before = serde_json::to_value(&original).unwrap();
                    let refusal =
                        crate::services::discord::commands::handle_text_command_with_uploads(
                            &transport.ctx,
                            &original,
                            &data,
                            original.channel_id,
                            &original.content,
                            &[],
                            &mut None,
                        )
                        .await
                        .expect_err("protected text skill has no client to assume custody");
                    assert!(
                        refusal
                            .to_string()
                            .starts_with("original pending unavailable:"),
                        "the actual dispatcher must preserve the pending refusal: {refusal}"
                    );
                    assert_eq!(serde_json::to_value(&original).unwrap(), before);
                    assert!(
                        transport.mock.written_texts().is_empty(),
                        "protected registered text skill emitted legacy confirmation"
                    );
                }
                assert!(data.shared.core.lock().await.sessions.is_empty());
                for command in ["!skill", "!cc"] {
                    let mut original =
                        discord_mock::user_message(6_325_227, &format!("{command} clear"));
                    original.channel_id = serenity::ChannelId::new(channel);
                    original.author.id = serenity::UserId::new(6_325_211);
                    assert!(
                        crate::services::discord::commands::handle_text_command_with_uploads(
                            &transport.ctx,
                            &original,
                            &data,
                            original.channel_id,
                            &original.content,
                            &[],
                            &mut None
                        )
                        .await
                        .unwrap()
                    );
                }
                assert!(
                    transport
                        .mock
                        .written_texts()
                        .iter()
                        .any(|text| text == "Use `!clear` instead.")
                );
                assert!(transport.mock.unhandled.lock().unwrap().is_empty());
            });
    }
}
