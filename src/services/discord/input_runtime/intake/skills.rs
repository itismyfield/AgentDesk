//! Original skill provenance and protected admission before Legacy command effects.

use crate::services::discord::commands::build_provider_skill_prompt;
use crate::services::discord::{Context, Data, Error, SharedData};
use crate::services::provider::ProviderKind;
use poise::serenity_prelude as serenity;

fn slash_skill_draft(
    interaction: &serenity::CommandInteraction,
    provider: &ProviderKind,
    invoked_as: &str,
    skill: &str,
    args: &str,
    registered: bool,
    execution_channel: u64,
) -> Result<crate::services::discord::input_runtime::intake::SourceDraft, String> {
    use crate::services::discord::input_runtime::intake::{Origin, SourceDraft};
    if skill_is_control(skill) {
        return Err("skill control aliases require their control handler".into());
    }
    let suffix = if args.is_empty() {
        String::new()
    } else {
        format!(" {args}")
    };
    let prompt = if registered {
        build_provider_skill_prompt(provider, skill, args)?
    } else {
        format!("/{skill}{suffix}")
    };
    let interaction_id = interaction.id.get();
    #[cfg(test)]
    let interaction_id = if crate::services::discord::input_runtime::supervisor::mutant(
        "c2b_slash_confirmation_source",
    ) {
        interaction_id + 1
    } else {
        interaction_id
    };
    Ok(SourceDraft {
        origin: Origin::SlashInteraction {
            interaction_id,
            author_id: interaction.user.id.get(),
            original_channel_id: interaction.channel_id.get(),
            execution_channel_id: execution_channel,
        },
        author_is_bot: interaction.user.bot,
        original_text: format!("{invoked_as} {skill}{suffix}"),
        prompt,
        reply_context: None,
        attachments: Vec::new(),
        voice_announcement: None,
    })
}

pub(in crate::services::discord) async fn hold_slash_skill(
    ctx: Context<'_>,
    invoked_as: &str,
    skill: &str,
    args: &str,
) -> Result<bool, Error> {
    use crate::services::discord::input_runtime::{self as runtime, intake};
    use intake::admit::{SourceKind, begin_external, prepare, submit_slash};
    #[cfg(test)]
    if runtime::supervisor::mutant("c2b_skill_early_guard_removed") {
        return Ok(false);
    }
    if !runtime::fence::any_protected() {
        return Ok(false);
    }
    let poise::Context::Application(app) = ctx else {
        return Err("protected skill requires its original interaction".into());
    };
    let (shared, provider) = (&ctx.data().shared, &ctx.data().provider);
    let registered = shared
        .skills_cache
        .read()
        .await
        .iter()
        .any(|(name, _)| name == skill);
    let raw = prepare(
        SourceKind::SlashSkill,
        slash_skill_draft(
            app.interaction,
            provider,
            invoked_as,
            skill,
            args,
            false,
            ctx.channel_id().get(),
        )?,
        None,
    )?;
    let facts =
        runtime::runtime::ports::source_policy_facts(shared, &ctx.serenity_context().http, &raw)
            .await;
    if matches!(facts.execution.protection, intake::Protection::Unknown) {
        return Err("protected skill is held while its execution target is unknown".into());
    }
    let source = prepare(
        SourceKind::SlashSkill,
        slash_skill_draft(
            app.interaction,
            provider,
            invoked_as,
            skill,
            args,
            false,
            facts.execution.channel_id,
        )?,
        None,
    )?;
    let facts =
        runtime::runtime::ports::source_policy_facts(shared, &ctx.serenity_context().http, &source)
            .await;
    match intake::classify(&facts, &source) {
        intake::Classification::Legacy => return Ok(false),
        intake::Classification::Target => {}
        disposition => return Err(format!("protected skill input is held: {disposition:?}").into()),
    }
    if skill.trim().is_empty() {
        return Err("protected skill requires a nonempty skill name".into());
    }
    let source = prepare(
        SourceKind::SlashSkill,
        slash_skill_draft(
            app.interaction,
            provider,
            invoked_as,
            skill,
            args,
            registered,
            source.identity().execution_channel_id,
        )?,
        None,
    )?;
    if intake::classify(&facts, &source) != intake::Classification::Target {
        return Err("protected skill source changed after policy approval".into());
    }
    let client = begin_external(
        runtime::runtime::registry(),
        provider,
        source.identity().execution_channel_id,
    )
    .map_err(|reason| format!("protected skill input is held: {reason:?}"))?
    .ok_or("protected skill has no execution client")?;
    let receipt = submit_slash(&client, &source)
        .await
        .map_err(|reason| format!("protected skill input is held: {reason:?}"))?;
    if !matches!(
        receipt,
        runtime::supervisor::external::ExternalReceipt::Received { .. }
    ) {
        return Err(format!("protected skill input was not received: {receipt:?}").into());
    }
    // The original interaction is durable before a response can acknowledge custody.
    ctx.say("입력을 원장에 접수했습니다.").await?;
    Ok(true)
}

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
    use crate::services::discord::{Data, input_runtime::intake};
    use axum::{
        body::Body,
        extract::State,
        http::{Request, StatusCode},
        response::Response,
        routing::post,
    };
    use serde_json::{Value, json};
    use std::any::Any;
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    };

    fn interaction(
        channel: u64,
        id: u64,
        command: &str,
        skill: &str,
    ) -> serenity::CommandInteraction {
        let mut user = serenity::User::default();
        user.id = serenity::UserId::new(6_325_211);
        user.name = "original-skill-author".into();
        serde_json::from_value(json!({
            "id":id.to_string(), "application_id":"6325212",
            "data":{"id":"6325213","name":command,"type":1,"resolved":{},
                "options":[{"name":"skill","type":3,"value":skill},{"name":"args","type":3,"value":"exact args"}]},
            "guild_id":null,"channel":null,"channel_id":channel.to_string(),"member":null,
            "user":user,"token":"original-skill-fixture","version":1,"app_permissions":null,
            "locale":"en-US","guild_locale":null,"entitlements":[],"context":null,"attachment_size_limit":8388608
        })).unwrap()
    }

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
        callbacks: Arc<Mutex<Vec<Value>>>,
        base: String,
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
                callbacks,
                base,
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
        }
    }
    async fn invoke(
        ctx: &serenity::Context,
        data: &Data,
        interaction: &serenity::CommandInteraction,
    ) -> (bool, bool) {
        let command = if interaction.data.name == "cc" {
            crate::services::discord::commands::cmd_cc()
        } else {
            crate::services::discord::commands::cmd_skill()
        };
        let options = poise::FrameworkOptions::<Data, Error>::default();
        let (manager, _) =
            serenity::gateway::ShardManager::new(serenity::gateway::ShardManagerOptions {
                data: ctx.data.clone(),
                event_handlers: vec![],
                raw_event_handlers: vec![],
                framework: Arc::new(std::sync::OnceLock::new()),
                shard_index: 0,
                shard_init: 0,
                shard_total: 1,
                voice_manager: None,
                ws_url: Arc::new(tokio::sync::Mutex::new("ws://127.0.0.1:1".into())),
                cache: ctx.cache.clone(),
                http: ctx.http.clone(),
                intents: serenity::GatewayIntents::empty(),
                presence: None,
            });
        let sent = AtomicBool::new(false);
        let invocation_data: tokio::sync::Mutex<Box<dyn Any + Send + Sync>> =
            tokio::sync::Mutex::new(Box::new(()));
        let args = interaction.data.options();
        let app = poise::ApplicationContext {
            serenity_context: ctx,
            interaction,
            interaction_type: poise::CommandInteractionType::Command,
            args: &args,
            has_sent_initial_response: &sent,
            framework: poise::FrameworkContext {
                bot_id: ctx.cache.current_user().id,
                options: &options,
                user_data: data,
                shard_manager: &manager,
            },
            parent_commands: &[],
            command: &command,
            data,
            invocation_data: &invocation_data,
            __non_exhaustive: (),
        };
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(8),
            command.slash_action.unwrap()(app),
        )
        .await
        .unwrap();
        (
            matches!(result, Err(poise::FrameworkError::Command { .. })),
            sent.load(Ordering::SeqCst),
        )
    }

    #[test]
    fn c2b_slash_registered_and_unknown_aliases_preserve_entire_original_source() {
        use intake::admit::{SourceKind, prepare};
        use intake::{Origin, SourceDraft};
        for command in ["skill", "cc"] {
            for registered in [false, true] {
                let original = interaction(6_325_220, 6_325_221, command, "local-skill");
                let before = serde_json::to_value(&original).unwrap();
                let invoked = format!("/{command}");
                let actual = prepare(
                    SourceKind::SlashSkill,
                    slash_skill_draft(
                        &original,
                        &ProviderKind::Claude,
                        &invoked,
                        "local-skill",
                        "exact args",
                        registered,
                        6_325_222,
                    )
                    .unwrap(),
                    None,
                )
                .unwrap();
                let expected = prepare(
                    SourceKind::SlashSkill,
                    SourceDraft {
                        origin: Origin::SlashInteraction {
                            interaction_id: 6_325_221,
                            author_id: 6_325_211,
                            original_channel_id: 6_325_220,
                            execution_channel_id: 6_325_222,
                        },
                        author_is_bot: false,
                        original_text: format!("/{command} local-skill exact args"),
                        prompt: if registered {
                            build_provider_skill_prompt(
                                &ProviderKind::Claude,
                                "local-skill",
                                "exact args",
                            )
                            .unwrap()
                        } else {
                            "/local-skill exact args".into()
                        },
                        reply_context: None,
                        attachments: vec![],
                        voice_announcement: None,
                    },
                    None,
                )
                .unwrap();
                assert_eq!(actual.input(), expected.input());
                assert_eq!(actual.identity(), expected.identity());
                assert_eq!(actual.key(), expected.key());
                assert_eq!(serde_json::to_value(&original).unwrap(), before);
            }
        }
        for control in [
            "clear", "stop", "pwd", "health", "status", "inflight", "help",
        ] {
            assert!(
                slash_skill_draft(
                    &interaction(6_325_220, 6_325_221, "skill", control),
                    &ProviderKind::Claude,
                    "/skill",
                    control,
                    "",
                    true,
                    6_325_222
                )
                .is_err()
            );
        }
    }

    #[test]
    fn c2b_actual_slash_skill_and_cc_do_not_ack_without_durable_receipt() {
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
                let channel = 6_325_223;
                let transport = Transport::new(channel).await;
                let _api = crate::config::TestEnvVarGuard::set_value_after_shared_test_env_lock(
                    "AGENTDESK_DISCORD_API_BASE_URL",
                    std::ffi::OsStr::new(&format!("{}/api/v10", transport.base)),
                );
                let data = data().await;
                let _gate = crate::services::discord::input_runtime::fence::Gate::protect(
                    ProviderKind::Claude,
                    channel,
                )
                .unwrap();
                let before = crate::services::discord::input_runtime::runtime::registry()
                    .population_for_test();
                for command in ["skill", "cc"] {
                    for registered in [false, true] {
                        *data.shared.skills_cache.write().await = if registered {
                            vec![("local-skill".into(), "fixture".into())]
                        } else {
                            vec![]
                        };
                        let original = interaction(channel, 6_325_224, command, "local-skill");
                        let snapshot = serde_json::to_value(&original).unwrap();
                        let (rejected, acknowledged) =
                            invoke(&transport.ctx, &data, &original).await;
                        assert!(
                            rejected,
                            "protected invocation must keep missing policy/client custody explicit"
                        );
                        assert!(
                            !acknowledged,
                            "no durable receipt exists to justify an interaction ACK"
                        );
                        assert!(
                            transport.callbacks.lock().unwrap().is_empty(),
                            "the real command action sent an early ACK"
                        );
                        assert!(
                            transport.mock.written_texts().is_empty(),
                            "the real command action emitted confirmation or provider effects"
                        );
                        assert_eq!(serde_json::to_value(&original).unwrap(), snapshot);
                    }
                }
                assert!(transport.mock.unhandled.lock().unwrap().is_empty());
                assert!(
                    data.shared.core.lock().await.sessions.is_empty(),
                    "protected skill must not auto-restore session state"
                );
                assert_eq!(
                    crate::services::discord::input_runtime::runtime::registry()
                        .population_for_test(),
                    before
                );
            });
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
                            .starts_with("ledger admission retains responsibility for rescan"),
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
