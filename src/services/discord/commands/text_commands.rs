use poise::serenity_prelude as serenity;
use poise::serenity_prelude::{CreateAttachment, CreateMessage};
use std::sync::Arc;

use super::super::router::{TurnKind, handle_text_message};
use super::super::*;
use super::build_provider_skill_prompt;
use crate::services::provider::CancelToken;

enum TextStopLookup {
    NoActiveTurn,
    AlreadyStopping,
    Stop(Arc<CancelToken>),
}

async fn cancel_text_stop_token_mailbox(
    shared: &Arc<SharedData>,
    channel_id: serenity::ChannelId,
) -> TextStopLookup {
    let result = mailbox_cancel_active_turn(shared, channel_id).await;
    match result.token {
        Some(_) if result.already_stopping => TextStopLookup::AlreadyStopping,
        Some(token) => TextStopLookup::Stop(token),
        None => TextStopLookup::NoActiveTurn,
    }
}

async fn fetch_escalation_settings_via_api()
-> Result<crate::server::routes::escalation::EscalationSettingsResponse, String> {
    let body = crate::services::discord::internal_api::get_escalation_settings().await?;
    serde_json::from_value(body).map_err(|err| err.to_string())
}

async fn save_escalation_settings_via_api(
    settings: &crate::server::routes::escalation::EscalationSettings,
) -> Result<crate::server::routes::escalation::EscalationSettingsResponse, String> {
    let body =
        crate::services::discord::internal_api::put_escalation_settings(settings.clone()).await?;
    serde_json::from_value(body).map_err(|err| err.to_string())
}

fn parse_discord_user_id(raw: &str) -> Option<u64> {
    raw.trim()
        .trim_start_matches("<@")
        .trim_end_matches('>')
        .trim_start_matches('!')
        .parse::<u64>()
        .ok()
}

fn format_escalation_settings_summary(
    settings: &crate::server::routes::escalation::EscalationSettings,
) -> String {
    let mode = match settings.mode {
        crate::config::EscalationMode::Pm => "pm",
        crate::config::EscalationMode::User => "user",
        crate::config::EscalationMode::Scheduled => "scheduled",
    };
    let owner = settings
        .owner_user_id
        .map(|id| id.to_string())
        .unwrap_or_else(|| "(none)".to_string());
    let pm_channel = settings
        .pm_channel_id
        .clone()
        .unwrap_or_else(|| "(none)".to_string());
    format!(
        "mode: `{}`\nowner_user_id: `{}`\npm_channel_id: `{}`\nschedule: `{}` / `{}`",
        mode, owner, pm_channel, settings.schedule.pm_hours, settings.schedule.timezone
    )
}

pub(in crate::services::discord) async fn handle_text_command(
    ctx: &serenity::Context,
    msg: &serenity::Message,
    data: &Data,
    channel_id: serenity::ChannelId,
    text: &str,
) -> Result<bool, Error> {
    let parts: Vec<&str> = text.splitn(3, char::is_whitespace).collect();
    let cmd = parts[0];
    let arg1 = parts.get(1).unwrap_or(&"");
    let arg2 = parts.get(2).unwrap_or(&"");

    // Issue #1005: Before any command-specific handling, classify the command
    // by risk tier and apply the owner guard. This runs BEFORE the allow_all
    // branch inside individual arms so that high-risk commands are never
    // unlocked by `allow_all_users=true`.
    let risk = super::command_risk(cmd, arg1);
    if risk.is_high_risk() {
        let is_owner = check_owner(msg.author.id, &data.shared).await;
        let high_risk_enabled = super::high_risk_enabled_via_env();
        let decision = super::evaluate_policy(risk, is_owner, high_risk_enabled);
        if let Some(reply) = decision.denial_message(cmd) {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::warn!(
                "  [{ts}] ⛔ CommandPolicy denied {} for {} (id:{}) — risk={:?}",
                cmd,
                msg.author.name,
                msg.author.id.get(),
                risk,
            );
            let _ = msg.reply(&ctx.http, reply).await;
            return Ok(true);
        }
    }

    match cmd {
        "!start" => {
            let path_str = if arg1.is_empty() { "." } else { arg1 };

            let effective_path = if path_str == "." || path_str.is_empty() {
                let Some(workspace_dir) = runtime_store::workspace_root() else {
                    let _ = msg
                        .reply(&ctx.http, "Error: cannot determine workspace root.")
                        .await;
                    return Ok(true);
                };

                use rand::Rng;
                let random_name: String = rand::thread_rng()
                    .sample_iter(&rand::distributions::Alphanumeric)
                    .take(8)
                    .map(char::from)
                    .collect();
                let ch_name = resolve_channel_category(ctx, channel_id)
                    .await
                    .0
                    .unwrap_or_else(|| format!("ch-{}", channel_id));
                let dir = workspace_dir.join(format!("{}-{}", ch_name, random_name));
                std::fs::create_dir_all(&dir).ok();
                dir.to_string_lossy().to_string()
            } else if path_str.starts_with('~') {
                dirs::home_dir()
                    .map(|h| path_str.replacen('~', &h.to_string_lossy(), 1))
                    .unwrap_or_else(|| path_str.to_string())
            } else {
                path_str.to_string()
            };

            if !std::path::Path::new(&effective_path).exists() {
                let _ = msg
                    .reply(
                        &ctx.http,
                        format!("Error: path `{}` does not exist.", effective_path),
                    )
                    .await;
                return Ok(true);
            }

            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] ◀ [{}] !start path={}",
                msg.author.name,
                effective_path
            );

            let (ch_name, cat_name) = resolve_channel_category(ctx, channel_id).await;
            {
                let mut d = data.shared.core.lock().await;
                let session = d
                    .sessions
                    .entry(channel_id)
                    .or_insert_with(|| DiscordSession {
                        session_id: None,
                        memento_context_loaded: false,
                        memento_reflected: false,
                        current_path: None,
                        history: Vec::new(),
                        pending_uploads: Vec::new(),
                        cleared: false,
                        channel_name: None,
                        category_name: None,
                        remote_profile_name: None,
                        channel_id: Some(channel_id.get()),
                        last_active: tokio::time::Instant::now(),
                        worktree: None,
                        born_generation: runtime_store::load_generation(),
                        assistant_turns: 0,
                    });
                session.current_path = Some(effective_path.clone());
                session.channel_name = ch_name;
                session.category_name = cat_name;
                session.last_active = tokio::time::Instant::now();
            }

            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!("  [{ts}] ▶ Session started: {}", effective_path);
            let _ = msg
                .reply(
                    &ctx.http,
                    format!("Session started at `{}`.", effective_path),
                )
                .await;
            return Ok(true);
        }

        "!meeting" => {
            let action = if arg1.is_empty() { "start" } else { arg1 };
            let agenda = if arg2.is_empty() { arg1 } else { arg2 };

            match action {
                "start" => {
                    let agenda_text = if agenda.is_empty() || *agenda == "start" {
                        let _ = msg
                            .reply(
                                &ctx.http,
                                "사용법: `!meeting start <안건>` 또는 `!meeting <안건>`",
                            )
                            .await;
                        return Ok(true);
                    } else {
                        agenda
                    };

                    let ts = chrono::Local::now().format("%H:%M:%S");
                    tracing::info!(
                        "  [{ts}] ◀ [{}] !meeting start {}",
                        msg.author.name,
                        agenda_text
                    );

                    let http = ctx.http.clone();
                    let shared = data.shared.clone();
                    let provider = data.provider.clone();
                    let reviewer = provider.counterpart();
                    let agenda_owned = agenda_text.to_string();

                    let _ = msg
                        .reply(
                            &ctx.http,
                            format!(
                                "📋 회의를 시작할게. 진행 모델: {} / 교차검증: {}",
                                provider.display_name(),
                                provider.counterpart().display_name()
                            ),
                        )
                        .await;

                    tokio::spawn(async move {
                        match meeting::start_meeting(
                            &*http,
                            channel_id,
                            &agenda_owned,
                            provider,
                            reviewer,
                            &shared,
                        )
                        .await
                        {
                            Ok(Some(id)) => {
                                let ts = chrono::Local::now().format("%H:%M:%S");
                                tracing::info!("  [{ts}] ✅ Meeting completed: {id}");
                            }
                            Ok(None) => {}
                            Err(e) => {
                                let ts = chrono::Local::now().format("%H:%M:%S");
                                tracing::info!("  [{ts}] ❌ Meeting error: {e}");
                            }
                        }
                    });
                    return Ok(true);
                }
                "stop" => {
                    let _ = meeting::cancel_meeting(&ctx.http, channel_id, &data.shared).await;
                    return Ok(true);
                }
                "status" => {
                    let _ = meeting::meeting_status(&ctx.http, channel_id, &data.shared).await;
                    return Ok(true);
                }
                _ => {
                    let full_agenda = text.trim_start_matches("!meeting").trim();
                    if full_agenda.is_empty() {
                        let _ = msg.reply(&ctx.http, "사용법: `!meeting <안건>`").await;
                        return Ok(true);
                    }
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    tracing::info!("  [{ts}] ◀ [{}] !meeting {}", msg.author.name, full_agenda);

                    let http = ctx.http.clone();
                    let shared = data.shared.clone();
                    let provider = data.provider.clone();
                    let reviewer = provider.counterpart();
                    let agenda_owned = full_agenda.to_string();

                    let _ = msg
                        .reply(
                            &ctx.http,
                            format!(
                                "📋 회의를 시작할게. 진행 모델: {} / 교차검증: {}",
                                provider.display_name(),
                                provider.counterpart().display_name()
                            ),
                        )
                        .await;

                    tokio::spawn(async move {
                        match meeting::start_meeting(
                            &*http,
                            channel_id,
                            &agenda_owned,
                            provider,
                            reviewer,
                            &shared,
                        )
                        .await
                        {
                            Ok(Some(id)) => {
                                let ts = chrono::Local::now().format("%H:%M:%S");
                                tracing::info!("  [{ts}] ✅ Meeting completed: {id}");
                            }
                            Ok(None) => {}
                            Err(e) => {
                                let ts = chrono::Local::now().format("%H:%M:%S");
                                tracing::info!("  [{ts}] ❌ Meeting error: {e}");
                            }
                        }
                    });
                    return Ok(true);
                }
            }
        }

        "!stop" => {
            let stop_lookup = cancel_text_stop_token_mailbox(&data.shared, channel_id).await;
            match stop_lookup {
                TextStopLookup::Stop(token) => {
                    // #1218: send abort key first, then SIGKILL — see
                    // `stop_active_turn` doc comment.
                    super::super::turn_bridge::stop_active_turn(
                        &data.provider,
                        &token,
                        super::super::turn_bridge::TmuxCleanupPolicy::PreserveSession,
                        "!stop",
                    )
                    .await;
                    super::super::commands::notify_turn_stop(
                        &ctx.http,
                        &data.shared,
                        &data.provider,
                        channel_id,
                        "!stop",
                    )
                    .await;
                }
                TextStopLookup::AlreadyStopping => {
                    let _ = msg.reply(&ctx.http, "Already stopping...").await;
                }
                TextStopLookup::NoActiveTurn => {
                    let _ = msg.reply(&ctx.http, "No active turn to stop.").await;
                }
            }
            return Ok(true);
        }

        "!clear" => {
            super::clear_channel_session_state(
                &ctx.http,
                &data.shared,
                &data.provider,
                channel_id,
                "!clear",
            )
            .await;
            let _ = msg.reply(&ctx.http, "Session cleared.").await;
            return Ok(true);
        }

        "!pwd" => {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!("  [{ts}] ◀ [{}] !pwd", msg.author.name);

            auto_restore_session(&data.shared, channel_id, ctx).await;

            let (current_path, remote_name) = {
                let d = data.shared.core.lock().await;
                let session = d.sessions.get(&channel_id);
                (
                    session.and_then(|s| s.current_path.clone()),
                    session.and_then(|s| s.remote_profile_name.clone()),
                )
            };
            let reply = match current_path {
                Some(path) => {
                    let remote_info = remote_name
                        .map(|n| format!(" (remote: **{}**)", n))
                        .unwrap_or_else(|| " (local)".to_string());
                    format!("`{}`{}", path, remote_info)
                }
                None => "No active session. Use `!start <path>` first.".to_string(),
            };
            let _ = msg.reply(&ctx.http, &reply).await;
            return Ok(true);
        }

        "!health" => {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!("  [{ts}] ◀ [{}] !health", msg.author.name);

            let text = super::build_health_report(&data.shared, &data.provider, channel_id).await;
            send_long_message_raw(&ctx.http, channel_id, &text, &data.shared).await?;
            return Ok(true);
        }

        "!status" => {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!("  [{ts}] ◀ [{}] !status", msg.author.name);

            let text = super::build_status_report(&data.shared, &data.provider, channel_id).await;
            send_long_message_raw(&ctx.http, channel_id, &text, &data.shared).await?;
            return Ok(true);
        }

        "!inflight" => {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!("  [{ts}] ◀ [{}] !inflight", msg.author.name);

            let text = super::build_inflight_report(&data.shared, &data.provider, channel_id).await;
            send_long_message_raw(&ctx.http, channel_id, &text, &data.shared).await?;
            return Ok(true);
        }

        "!queue" => {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!("  [{ts}] ◀ [{}] !queue", msg.author.name);

            let show_all = *arg1 == "all";
            let text =
                super::build_queue_report(&data.shared, &data.provider, channel_id, show_all).await;
            send_long_message_raw(&ctx.http, channel_id, &text, &data.shared).await?;
            return Ok(true);
        }

        "!metrics" => {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!("  [{ts}] ◀ [{}] !metrics", msg.author.name);

            let metrics_data = if arg1.is_empty() {
                super::super::metrics::load_today()
            } else {
                super::super::metrics::load_date(arg1)
            };
            let label = if arg1.is_empty() { "today" } else { arg1 };
            let text = super::super::metrics::build_metrics_report(&metrics_data, label);
            send_long_message_raw(&ctx.http, channel_id, &text, &data.shared).await?;
            return Ok(true);
        }

        "!debug" => {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!("  [{ts}] ◀ [{}] !debug", msg.author.name);

            let new_state = crate::services::claude::toggle_debug();
            let status = if new_state { "ON" } else { "OFF" };
            let _ = msg
                .reply(&ctx.http, format!("Debug logging: **{}**", status))
                .await;
            tracing::info!("  [{ts}] ▶ Debug logging toggled to {status}");
            return Ok(true);
        }

        "!escalation" => {
            let ts = chrono::Local::now().format("%H:%M:%S");
            let rest = text.strip_prefix("!escalation").unwrap_or("").trim();
            tracing::info!("  [{ts}] ◀ [{}] !escalation {}", msg.author.name, rest);

            if !check_owner(msg.author.id, &data.shared).await {
                let _ = msg
                    .reply(&ctx.http, "Only the owner can change escalation settings.")
                    .await;
                return Ok(true);
            }

            let mut settings = match fetch_escalation_settings_via_api().await {
                Ok(response) => response.current,
                Err(err) => {
                    let _ = msg
                        .reply(
                            &ctx.http,
                            format!("Failed to load escalation settings: {err}"),
                        )
                        .await;
                    return Ok(true);
                }
            };

            if rest.is_empty() || rest.eq_ignore_ascii_case("status") {
                let _ = msg
                    .reply(
                        &ctx.http,
                        format!(
                            "**Escalation Settings**\n{}",
                            format_escalation_settings_summary(&settings)
                        ),
                    )
                    .await;
                return Ok(true);
            }

            let mut parts = rest.splitn(2, char::is_whitespace);
            let subcommand = parts.next().unwrap_or("").trim().to_ascii_lowercase();
            let value = parts.next().unwrap_or("").trim();

            let usage = "Usage: `!escalation status|pm|user|scheduled|schedule <HH:MM-HH:MM>|timezone <IANA>|owner <user_id>|pm-channel <channel_id>`";
            let update_error = match subcommand.as_str() {
                "pm" => {
                    settings.mode = crate::config::EscalationMode::Pm;
                    None
                }
                "user" => {
                    settings.mode = crate::config::EscalationMode::User;
                    None
                }
                "scheduled" => {
                    settings.mode = crate::config::EscalationMode::Scheduled;
                    None
                }
                "schedule" => {
                    if value.is_empty() {
                        Some("schedule value is required")
                    } else {
                        settings.mode = crate::config::EscalationMode::Scheduled;
                        settings.schedule.pm_hours = value.to_string();
                        None
                    }
                }
                "timezone" => {
                    if value.is_empty() {
                        Some("timezone value is required")
                    } else {
                        settings.schedule.timezone = value.to_string();
                        None
                    }
                }
                "owner" => match parse_discord_user_id(value) {
                    Some(user_id) => {
                        settings.owner_user_id = Some(user_id);
                        None
                    }
                    None => Some("owner must be a numeric Discord user id or mention"),
                },
                "clear-owner" => {
                    settings.owner_user_id = None;
                    None
                }
                "pm-channel" => {
                    if value.is_empty() {
                        Some("pm-channel value is required")
                    } else {
                        settings.pm_channel_id = Some(value.to_string());
                        None
                    }
                }
                "clear-pm-channel" => {
                    settings.pm_channel_id = None;
                    None
                }
                _ => Some(usage),
            };

            if let Some(err) = update_error {
                let _ = msg.reply(&ctx.http, err).await;
                return Ok(true);
            }

            match save_escalation_settings_via_api(&settings).await {
                Ok(response) => {
                    let _ = msg
                        .reply(
                            &ctx.http,
                            format!(
                                "**Escalation Settings Updated**\n{}",
                                format_escalation_settings_summary(&response.current)
                            ),
                        )
                        .await;
                }
                Err(err) => {
                    let _ = msg
                        .reply(
                            &ctx.http,
                            format!("Failed to save escalation settings: {err}"),
                        )
                        .await;
                }
            }
            return Ok(true);
        }

        "!help" => {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!("  [{ts}] ◀ [{}] !help", msg.author.name);

            let provider_name = data.provider.display_name();
            let help = format!(
                "\
**AgentDesk Discord Bot**
Manage server files & chat with {p}.
Each channel gets its own independent {p} session.

**Session**
`!start <path>` — Start session at directory
`!pwd` — Show current working directory
`!health` — Show runtime health summary
`!status` — Show this channel session status
`!inflight` — Show saved inflight turn state
`!clear` — Clear AI conversation history
`!stop` — Stop current AI request

**File Transfer**
`!down <file>` — Download file from server
Send a file/photo — Upload to session directory

**Shell**
`!shell <command>` — Run shell command directly

**AI Chat**
Any other message is sent to {p}.

**Tool Management**
`!allowedtools` — Show currently allowed tools
`!allowed +name` — Add tool (e.g. `!allowed +Bash`)
`!allowed -name` — Remove tool

**Skills**
`!cc <skill>` — Run a provider skill

**Settings**
`/model` — Open the interactive model picker
`!debug` — Toggle debug logging
`!metrics [date]` — Show turn metrics
`!queue [all]` — Show pending queue
`!escalation status` — Show escalation routing mode

**User Management** (owner only)
`!allowall on|off|status` — Allow everyone or restrict to authorized users
`!adduser <user_id>` — Allow a user to use the bot
`!removeuser <user_id>` — Remove a user's access
`!escalation pm|user|scheduled` — Change escalation routing mode
`!escalation schedule <HH:MM-HH:MM>` — Set PM hours and switch to scheduled mode
`!escalation timezone <IANA>` — Set scheduled timezone
`!escalation owner <user_id>` — Override fallback owner user id
`!escalation pm-channel <channel_id>` — Override PM channel
`!help` — Show this help

{risk_block}",
                p = provider_name,
                risk_block = super::risk_tier_summary_for_help(super::high_risk_enabled_via_env()),
            );
            send_long_message_raw(&ctx.http, channel_id, &help, &data.shared).await?;
            return Ok(true);
        }

        "!allowedtools" => {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!("  [{ts}] ◀ [{}] !allowedtools", msg.author.name);

            let tools = {
                let settings = data.shared.settings.read().await;
                settings.allowed_tools.clone()
            };

            let mut reply = String::from("**Allowed Tools**\n\n");
            for tool in &tools {
                let (desc, destructive) = super::super::formatting::tool_info(tool);
                let badge = super::super::formatting::risk_badge(destructive);
                if badge.is_empty() {
                    reply.push_str(&format!("`{}` — {}\n", tool, desc));
                } else {
                    reply.push_str(&format!("`{}` {} — {}\n", tool, badge, desc));
                }
            }
            reply.push_str(&format!(
                "\n{} = destructive\nTotal: {}",
                super::super::formatting::risk_badge(true),
                tools.len()
            ));
            send_long_message_raw(&ctx.http, channel_id, &reply, &data.shared).await?;
            return Ok(true);
        }

        "!model" => {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!("  [{ts}] ◀ [{}] !model {} {}", msg.author.name, arg1, arg2);
            let _ = msg
                .reply(
                    &ctx.http,
                    "Model picker text commands are deprecated. Use `/model`.",
                )
                .await;
            return Ok(true);
        }

        "!allowed" => {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!("  [{ts}] ◀ [{}] !allowed {}", msg.author.name, arg1);

            let arg = arg1.trim();
            let (op, raw_name) = if let Some(name) = arg.strip_prefix('+') {
                ('+', name.trim())
            } else if let Some(name) = arg.strip_prefix('-') {
                ('-', name.trim())
            } else {
                let _ = msg.reply(&ctx.http, "Use `+toolname` to add or `-toolname` to remove.\nExample: `!allowed +Bash`").await;
                return Ok(true);
            };

            if raw_name.is_empty() {
                let _ = msg.reply(&ctx.http, "Tool name cannot be empty.").await;
                return Ok(true);
            }

            let Some(tool_name) =
                super::super::formatting::canonical_tool_name(raw_name).map(str::to_string)
            else {
                let _ = msg
                    .reply(
                        &ctx.http,
                        format!(
                            "Unknown tool `{}`. Use `!allowedtools` to see valid tool names.",
                            raw_name
                        ),
                    )
                    .await;
                return Ok(true);
            };

            let response_msg = {
                let mut settings = data.shared.settings.write().await;
                match op {
                    '+' => {
                        if settings.allowed_tools.iter().any(|t| t == &tool_name) {
                            format!("`{}` is already in the list.", tool_name)
                        } else {
                            settings.allowed_tools.push(tool_name.clone());
                            save_bot_settings(&data.token, &settings);
                            format!("Added `{}`", tool_name)
                        }
                    }
                    '-' => {
                        let before_len = settings.allowed_tools.len();
                        settings.allowed_tools.retain(|t| t != &tool_name);
                        if settings.allowed_tools.len() < before_len {
                            save_bot_settings(&data.token, &settings);
                            format!("Removed `{}`", tool_name)
                        } else {
                            format!("`{}` is not in the list.", tool_name)
                        }
                    }
                    _ => unreachable!(),
                }
            };
            let _ = msg.reply(&ctx.http, &response_msg).await;
            return Ok(true);
        }

        "!adduser" => {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!("  [{ts}] ◀ [{}] !adduser {}", msg.author.name, arg1);

            if !check_owner(msg.author.id, &data.shared).await {
                let _ = msg.reply(&ctx.http, "Only the owner can add users.").await;
                return Ok(true);
            }

            let raw_id = arg1
                .trim()
                .trim_start_matches("<@")
                .trim_end_matches('>')
                .trim_start_matches('!');
            let target_id: u64 = match raw_id.parse() {
                Ok(id) => id,
                Err(_) => {
                    let _ = msg
                        .reply(&ctx.http, "Usage: `!adduser <user_id>` or `!adduser @user`")
                        .await;
                    return Ok(true);
                }
            };

            {
                let mut settings = data.shared.settings.write().await;
                if settings.allowed_user_ids.contains(&target_id) {
                    let _ = msg
                        .reply(&ctx.http, format!("`{}` is already authorized.", target_id))
                        .await;
                    return Ok(true);
                }
                settings.allowed_user_ids.push(target_id);
                save_bot_settings(&data.token, &settings);
            }

            let _ = msg
                .reply(
                    &ctx.http,
                    format!("Added `{}` as authorized user.", target_id),
                )
                .await;
            tracing::info!("  [{ts}] ▶ Added user: {target_id}");
            return Ok(true);
        }

        "!allowall" => {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!("  [{ts}] ◀ [{}] !allowall {}", msg.author.name, arg1);

            if !check_owner(msg.author.id, &data.shared).await {
                let _ = msg
                    .reply(&ctx.http, "Only the owner can change public access.")
                    .await;
                return Ok(true);
            }

            let action = arg1.trim().to_ascii_lowercase();
            if action.is_empty() || action == "status" {
                let enabled = {
                    let settings = data.shared.settings.read().await;
                    settings.allow_all_users
                };
                let message = if enabled {
                    "Public access is enabled. Any Discord user can talk to this bot in allowed channels."
                } else {
                    "Public access is disabled. Only the owner and authorized users can talk to this bot."
                };
                // Issue #1005: include the high-risk policy reminder on status
                // queries so operators always see what stays owner-only.
                let combined = format!("{message}\n\n{}", super::build_allowall_policy_note());
                let _ = msg.reply(&ctx.http, combined).await;
                return Ok(true);
            }

            let enabled = match action.as_str() {
                "on" | "true" | "enable" | "enabled" => true,
                "off" | "false" | "disable" | "disabled" => false,
                _ => {
                    let _ = msg
                        .reply(
                            &ctx.http,
                            "Usage: `!allowall on`, `!allowall off`, or `!allowall status`",
                        )
                        .await;
                    return Ok(true);
                }
            };

            let response = {
                let mut settings = data.shared.settings.write().await;
                settings.allow_all_users = enabled;
                save_bot_settings(&data.token, &settings);
                if enabled {
                    "Public access enabled. Any Discord user can talk to this bot in allowed channels."
                } else {
                    "Public access disabled. Only the owner and authorized users can talk to this bot."
                }
            };

            // Issue #1005: pin the policy reminder to the toggle response too.
            let combined = format!("{response}\n\n{}", super::build_allowall_policy_note());
            let _ = msg.reply(&ctx.http, combined).await;
            tracing::info!("  [{ts}] ▶ {response}");
            return Ok(true);
        }

        "!removeuser" => {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!("  [{ts}] ◀ [{}] !removeuser {}", msg.author.name, arg1);

            if !check_owner(msg.author.id, &data.shared).await {
                let _ = msg
                    .reply(&ctx.http, "Only the owner can remove users.")
                    .await;
                return Ok(true);
            }

            let raw_id = arg1
                .trim()
                .trim_start_matches("<@")
                .trim_end_matches('>')
                .trim_start_matches('!');
            let target_id: u64 = match raw_id.parse() {
                Ok(id) => id,
                Err(_) => {
                    let _ = msg
                        .reply(
                            &ctx.http,
                            "Usage: `!removeuser <user_id>` or `!removeuser @user`",
                        )
                        .await;
                    return Ok(true);
                }
            };

            {
                let mut settings = data.shared.settings.write().await;
                let before_len = settings.allowed_user_ids.len();
                settings.allowed_user_ids.retain(|&id| id != target_id);
                if settings.allowed_user_ids.len() == before_len {
                    let _ = msg
                        .reply(
                            &ctx.http,
                            format!("`{}` is not in the authorized list.", target_id),
                        )
                        .await;
                    return Ok(true);
                }
                save_bot_settings(&data.token, &settings);
            }

            let _ = msg
                .reply(
                    &ctx.http,
                    format!("Removed `{}` from authorized users.", target_id),
                )
                .await;
            tracing::info!("  [{ts}] ▶ Removed user: {target_id}");
            return Ok(true);
        }

        "!down" => {
            let ts = chrono::Local::now().format("%H:%M:%S");
            let file_arg = text.strip_prefix("!down").unwrap_or("").trim();
            tracing::info!("  [{ts}] ◀ [{}] !down {}", msg.author.name, file_arg);

            if file_arg.is_empty() {
                let _ = msg
                    .reply(
                        &ctx.http,
                        "Usage: `!down <filepath>`\nExample: `!down /home/user/file.txt`",
                    )
                    .await;
                return Ok(true);
            }

            let resolved_path = if std::path::Path::new(file_arg).is_absolute() {
                file_arg.to_string()
            } else {
                let current_path = {
                    let d = data.shared.core.lock().await;
                    d.sessions
                        .get(&channel_id)
                        .and_then(|s| s.current_path.clone())
                };
                match current_path {
                    Some(base) => format!("{}/{}", base.trim_end_matches('/'), file_arg),
                    None => {
                        let _ = msg
                            .reply(
                                &ctx.http,
                                "No active session. Use absolute path or `!start <path>` first.",
                            )
                            .await;
                        return Ok(true);
                    }
                }
            };

            let path = std::path::Path::new(&resolved_path);
            if !path.exists() {
                let _ = msg
                    .reply(&ctx.http, format!("File not found: {}", resolved_path))
                    .await;
                return Ok(true);
            }
            if !path.is_file() {
                let _ = msg
                    .reply(&ctx.http, format!("Not a file: {}", resolved_path))
                    .await;
                return Ok(true);
            }

            let attachment = CreateAttachment::path(path).await?;
            rate_limit_wait(&data.shared, channel_id).await;
            let _ = channel_id
                .send_message(&ctx.http, CreateMessage::new().add_file(attachment))
                .await;
            return Ok(true);
        }

        "!shell" => {
            let cmd_str = text.strip_prefix("!shell").unwrap_or("").trim();
            let ts = chrono::Local::now().format("%H:%M:%S");
            let preview = truncate_str(cmd_str, 60);
            tracing::info!("  [{ts}] ◀ [{}] !shell {}", msg.author.name, preview);

            if cmd_str.is_empty() {
                let _ = msg
                    .reply(
                        &ctx.http,
                        "Usage: `!shell <command>`\nExample: `!shell ls -la`",
                    )
                    .await;
                return Ok(true);
            }

            // Issue #1128: guard against unbounded recursive scans before
            // they reach the platform shell. The detector blocks
            // `grep -r/-R` without exclude flags, `find /Users` without a
            // name filter, and recursive scans of the workspace root.
            let guard_decision = crate::services::shell_guard::inspect_command(cmd_str);
            if let Some(block_msg) =
                crate::services::shell_guard::format_block_message(&guard_decision)
            {
                tracing::warn!(
                    "[shell_guard] blocked !shell command from {}: {:?}",
                    msg.author.name,
                    cmd_str
                );
                send_long_message_raw(&ctx.http, channel_id, &block_msg, &data.shared).await?;
                return Ok(true);
            }

            let working_dir = {
                let d = data.shared.core.lock().await;
                d.sessions
                    .get(&channel_id)
                    .and_then(|s| s.current_path.clone())
                    .unwrap_or_else(|| {
                        dirs::home_dir()
                            .map(|h| h.display().to_string())
                            .unwrap_or_else(|| "/".to_string())
                    })
            };

            let cmd_owned = cmd_str.to_string();
            let working_dir_clone = working_dir.clone();

            let result = tokio::task::spawn_blocking(move || {
                let mut builder =
                    crate::services::platform::shell::shell_command_builder(&cmd_owned);
                builder
                    .current_dir(&working_dir_clone)
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::piped());
                crate::services::process::configure_child_process_group(&mut builder);
                match builder.spawn() {
                    Ok(child) => crate::services::shell_guard::wait_with_no_output_timeout(
                        child,
                        crate::services::shell_guard::DEFAULT_NO_OUTPUT_TIMEOUT,
                        crate::services::shell_guard::DEFAULT_TOTAL_TIMEOUT,
                    ),
                    Err(e) => Err(format!("spawn failed: {}", e)),
                }
            })
            .await;

            let response = match result {
                Ok(Ok(outcome)) => {
                    let stdout = String::from_utf8_lossy(&outcome.stdout);
                    let stderr = String::from_utf8_lossy(&outcome.stderr);
                    let exit_code = outcome.exit_code;
                    let mut parts = Vec::new();
                    if !stdout.is_empty() {
                        parts.push(format!("```\n{}\n```", stdout.trim_end()));
                    }
                    if !stderr.is_empty() {
                        parts.push(format!("stderr:\n```\n{}\n```", stderr.trim_end()));
                    }
                    if let Some(cause) = outcome.timed_out {
                        parts.push(format!(
                            "killed by shell guard ({}). Issue #1128: split the command, \
                             scope the path, or add `--exclude-dir`/`-name` filters before \
                             retrying.",
                            cause.as_str()
                        ));
                    } else if parts.is_empty() {
                        parts.push(format!("(exit code: {})", exit_code));
                    } else if exit_code != 0 {
                        parts.push(format!("(exit code: {})", exit_code));
                    }
                    parts.join("\n")
                }
                Ok(Err(e)) => format!("Failed to execute: {}", e),
                Err(e) => format!("Task error: {}", e),
            };

            send_long_message_raw(&ctx.http, channel_id, &response, &data.shared).await?;
            return Ok(true);
        }

        "!cc" => {
            let skill = arg1.to_string();
            let args_str = text
                .strip_prefix("!cc")
                .unwrap_or("")
                .trim()
                .strip_prefix(&skill)
                .unwrap_or("")
                .trim();
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] ◀ [{}] !cc {} {}",
                msg.author.name,
                skill,
                args_str
            );

            if skill.is_empty() {
                let _ = msg.reply(&ctx.http, "Usage: `!cc <skill> [args]`").await;
                return Ok(true);
            }

            match skill.as_str() {
                "clear" => {
                    let _ = msg.reply(&ctx.http, "Use `!clear` instead.").await;
                    return Ok(true);
                }
                "stop" => {
                    // Issue #1005: `!cc stop` is an alias for `!stop` — same
                    // cancel path. Mirror `!stop`'s tier (Mutating, post-#1190)
                    // so the alias policy matches the canonical surface.
                    let is_owner = check_owner(msg.author.id, &data.shared).await;
                    let high_risk_enabled = super::high_risk_enabled_via_env();
                    let alias_decision = super::evaluate_policy(
                        super::CommandRisk::Mutating,
                        is_owner,
                        high_risk_enabled,
                    );
                    if let Some(reply) = alias_decision.denial_message("!cc stop") {
                        let ts = chrono::Local::now().format("%H:%M:%S");
                        tracing::warn!(
                            "  [{ts}] ⛔ CommandPolicy denied !cc stop for {} (id:{})",
                            msg.author.name,
                            msg.author.id.get(),
                        );
                        let _ = msg.reply(&ctx.http, reply).await;
                        return Ok(true);
                    }
                    let stop_lookup =
                        cancel_text_stop_token_mailbox(&data.shared, channel_id).await;
                    match stop_lookup {
                        TextStopLookup::Stop(token) => {
                            super::super::turn_bridge::cancel_active_token(
                                &token,
                                super::super::turn_bridge::TmuxCleanupPolicy::PreserveSession,
                                "!cc stop",
                            );
                            // #1117 see !stop branch above for rationale.
                            super::super::turn_bridge::interrupt_provider_cli_turn(
                                &data.provider,
                                &token,
                                "!cc stop",
                            )
                            .await;
                            super::super::commands::notify_turn_stop(
                                &ctx.http,
                                &data.shared,
                                &data.provider,
                                channel_id,
                                "!cc stop",
                            )
                            .await;
                            let _ = msg.reply(&ctx.http, "Stopping...").await;
                        }
                        TextStopLookup::AlreadyStopping => {
                            let _ = msg.reply(&ctx.http, "Already stopping...").await;
                        }
                        TextStopLookup::NoActiveTurn => {
                            let _ = msg.reply(&ctx.http, "No active request to stop.").await;
                        }
                    }
                    return Ok(true);
                }
                "pwd" => {
                    return Box::pin(handle_text_command(ctx, msg, data, channel_id, "!pwd")).await;
                }
                "health" => {
                    return Box::pin(handle_text_command(ctx, msg, data, channel_id, "!health"))
                        .await;
                }
                "status" => {
                    return Box::pin(handle_text_command(ctx, msg, data, channel_id, "!status"))
                        .await;
                }
                "inflight" => {
                    return Box::pin(handle_text_command(ctx, msg, data, channel_id, "!inflight"))
                        .await;
                }
                "help" => {
                    return Box::pin(handle_text_command(ctx, msg, data, channel_id, "!help"))
                        .await;
                }
                _ => {}
            }

            auto_restore_session(&data.shared, channel_id, ctx).await;

            let skill_exists = {
                let skills = data.shared.skills_cache.read().await;
                skills.iter().any(|(name, _)| name == &skill)
            };

            if !skill_exists {
                let _ = msg
                    .reply(
                        &ctx.http,
                        format!(
                            "Unknown skill: `{}`. Use `!cc` to see available skills.",
                            skill
                        ),
                    )
                    .await;
                return Ok(true);
            }

            let has_session = {
                let d = data.shared.core.lock().await;
                d.sessions
                    .get(&channel_id)
                    .and_then(|s| s.current_path.as_ref())
                    .is_some()
            };

            if !has_session {
                let _ = msg
                    .reply(&ctx.http, "No active session. Use `!start <path>` first.")
                    .await;
                return Ok(true);
            }

            if mailbox_has_active_turn(&data.shared, channel_id).await {
                let _ = msg
                    .reply(&ctx.http, "AI request in progress. Use `!stop` to cancel.")
                    .await;
                return Ok(true);
            }

            let skill_prompt = match build_provider_skill_prompt(&data.provider, &skill, args_str) {
                Ok(prompt) => prompt,
                Err(message) => {
                    let _ = msg.reply(&ctx.http, message).await;
                    return Ok(true);
                }
            };

            rate_limit_wait(&data.shared, channel_id).await;
            let confirm = channel_id
                .send_message(
                    &ctx.http,
                    CreateMessage::new().content(format!("Running skill: `/{skill}`")),
                )
                .await?;

            handle_text_message(
                ctx,
                channel_id,
                confirm.id,
                msg.author.id,
                &msg.author.name,
                &skill_prompt,
                &data.shared,
                &data.token,
                false,
                false,
                false,
                false,
                None,
                false,
                None,
                TurnKind::Foreground,
            )
            .await?;
            return Ok(true);
        }

        _ => {}
    }

    Ok(false)
}
