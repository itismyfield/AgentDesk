use poise::serenity_prelude as serenity;

use super::super::formatting::{send_long_message_ctx, truncate_str};
use super::super::outbound::confirmation::send_command_confirmation_message;
use super::super::router::{IntakeDeps, IntakeOrigin, dispatch_skill_intake};
use super::super::{
    Context, Error, auto_restore_session, check_auth, mailbox_cancel_active_turn,
    mailbox_has_active_turn,
};
use crate::services::provider::ProviderKind;

// Keep provider-specific skill wording in one helper so /skill, /cc, !skill,
// and !cc stay aligned.

pub(in crate::services::discord) fn build_provider_skill_prompt(
    provider: &ProviderKind,
    skill: &str,
    args_str: &str,
) -> Result<String, String> {
    match provider {
        ProviderKind::Claude => {
            if args_str.is_empty() {
                Ok(format!(
                    "Execute the skill `/{skill}` now. \
                     Use the Skill tool with skill=\"{skill}\". \
                     Read files under `references/` only if the skill points to them or you need extra detail."
                ))
            } else {
                Ok(format!(
                    "Execute the skill `/{skill}` with arguments: {args_str}\n\
                     Use the Skill tool with skill=\"{skill}\", args=\"{args_str}\". \
                     Read files under `references/` only if the skill points to them or you need extra detail."
                ))
            }
        }
        ProviderKind::Codex => {
            if args_str.is_empty() {
                Ok(format!(
                    "Use the local Codex skill `/{skill}` now. \
                     Load its `SKILL.md` first, follow it exactly, and read files under `references/` only when the skill points to them or you need them."
                ))
            } else {
                Ok(format!(
                    "Use the local Codex skill `/{skill}` now with this user request: {args_str}\n\
                     Load its `SKILL.md` first, adapt it to the request, and read files under `references/` only when the skill points to them or you need them."
                ))
            }
        }
        ProviderKind::Gemini => {
            if args_str.is_empty() {
                Ok(format!(
                    "Use the local Gemini skill `/{skill}` now. \
                     Load its `SKILL.md` first, follow it exactly, and read files under `references/` only when the skill points to them or you need them."
                ))
            } else {
                Ok(format!(
                    "Use the local Gemini skill `/{skill}` now with this user request: {args_str}\n\
                     Load its `SKILL.md` first, adapt it to the request, and read files under `references/` only when the skill points to them or you need them."
                ))
            }
        }
        ProviderKind::Qwen => {
            if args_str.is_empty() {
                Ok(format!(
                    "Use the local Qwen skill `/{skill}` from the active `.qwen/skills` runtime now. \
                     Load its `SKILL.md` first, follow it exactly, and read files under `references/` only when the skill points to them or you need them."
                ))
            } else {
                Ok(format!(
                    "Use the local Qwen skill `/{skill}` from the active `.qwen/skills` runtime now with this user request: {args_str}\n\
                     Load its `SKILL.md` first, adapt it to the request, and read files under `references/` only when the skill points to them or you need them."
                ))
            }
        }
        ProviderKind::OpenCode => {
            if args_str.is_empty() {
                Ok(format!(
                    "Use the local OpenCode skill `/{skill}` now. \
                     Load its `SKILL.md` first, follow it exactly, and read files under `references/` only when the skill points to them or you need them."
                ))
            } else {
                Ok(format!(
                    "Use the local OpenCode skill `/{skill}` now with this user request: {args_str}\n\
                     Load its `SKILL.md` first, adapt it to the request, and read files under `references/` only when the skill points to them or you need them."
                ))
            }
        }
        ProviderKind::Unsupported(name) => Err(format!(
            "Provider '{}' is not installed. This skill cannot run.",
            name
        )),
    }
}

/// Autocomplete handler for /skill and /cc skill names
async fn autocomplete_skill<'a>(
    ctx: Context<'a>,
    partial: &'a str,
) -> Vec<serenity::AutocompleteChoice> {
    let builtins = [
        ("health", "Show runtime health summary"),
        ("status", "Show this channel session status"),
        ("inflight", "Show saved inflight turn state"),
        ("pwd", "Show current working directory"),
        ("stop", "Stop current AI request"),
        ("help", "Show command help"),
    ];
    let skills = ctx.data().shared.skills_cache.read().await;
    let partial_lower = partial.to_lowercase();
    let mut choices = Vec::new();

    for (name, desc) in builtins {
        if partial.is_empty() || name.contains(&partial_lower) {
            let label = format!("{} — {}", name, truncate_str(desc, 60));
            choices.push(serenity::AutocompleteChoice::new(label, name.to_string()));
        }
    }

    for (name, desc) in skills.iter() {
        if choices.len() >= 25 {
            break;
        }
        if partial.is_empty() || name.to_lowercase().contains(&partial_lower) {
            let label = format!("{} — {}", name, truncate_str(desc, 60));
            choices.push(serenity::AutocompleteChoice::new(label, name.clone()));
        }
    }

    choices
}

async fn run_skill_slash_command(
    ctx: Context<'_>,
    invoked_as: &'static str,
    skill: String,
    args: Option<String>,
) -> Result<(), Error> {
    let user_id = ctx.author().id;
    let user_name = &ctx.author().name;
    if !check_auth(user_id, user_name, &ctx.data().shared, &ctx.data().token).await {
        return Ok(());
    }

    let ts = chrono::Local::now().format("%H:%M:%S");
    let args_str = args.as_deref().unwrap_or("");
    tracing::info!("  [{ts}] ◀ [{user_name}] {invoked_as} {skill} {args_str}");

    // Handle built-in commands directly instead of sending to AI
    match skill.as_str() {
        "clear" => {
            ctx.say("`/clear` 슬래시 명령을 사용해 주세요.").await?;
            return Ok(());
        }
        "stop" => {
            // Issue #1005: `/skill stop` and `/cc stop` are runtime-control
            // aliases for `/stop`
            // — it cancels the live turn — so it must obey the same
            // owner-only policy as `/stop` itself. Without this gate a
            // non-owner allowed in via `allow_all_users=true` could drop
            // active turns by routing through an alias.
            if !super::enforce_slash_command_policy(&ctx, "/stop").await? {
                return Ok(());
            }
            let channel_id = ctx.channel_id();
            let result = mailbox_cancel_active_turn(&ctx.data().shared, channel_id).await;
            match result.token {
                Some(token) => {
                    if result.already_stopping {
                        ctx.say(super::ALREADY_STOPPING_RESPONSE).await?;
                        return Ok(());
                    }
                    ctx.say(super::STOPPING_RESPONSE).await?;
                    super::super::turn_bridge::stop_active_turn(
                        &ctx.data().provider,
                        &token,
                        super::super::turn_bridge::TmuxCleanupPolicy::PreserveSession,
                        &format!("{invoked_as} stop"),
                    )
                    .await;
                    tracing::info!("  [{ts}] ■ Cancel signal sent");
                }
                None => {
                    ctx.say(super::NO_ACTIVE_TURN_RESPONSE).await?;
                }
            }
            return Ok(());
        }
        "pwd" => {
            let current_path = {
                let data = ctx.data().shared.core.lock().await;
                let session = data.sessions.get(&ctx.channel_id());
                session.and_then(|s| s.current_path.clone())
            };
            match current_path {
                Some(path) => ctx.say(format!("`{}`", path)).await?,
                None => {
                    ctx.say("No active session. Use `/start <path>` first.")
                        .await?
                }
            };
            return Ok(());
        }
        "health" => {
            let text = super::diagnostics::build_health_report(
                &ctx.data().shared,
                &ctx.data().provider,
                ctx.channel_id(),
            )
            .await;
            send_long_message_ctx(ctx, &text).await?;
            return Ok(());
        }
        "status" => {
            let text = super::diagnostics::build_status_report(
                &ctx.data().shared,
                &ctx.data().provider,
                ctx.channel_id(),
            )
            .await;
            send_long_message_ctx(ctx, &text).await?;
            return Ok(());
        }
        "inflight" => {
            let text = super::diagnostics::build_inflight_report(
                &ctx.data().shared,
                &ctx.data().provider,
                ctx.channel_id(),
            )
            .await;
            send_long_message_ctx(ctx, &text).await?;
            return Ok(());
        }
        "help" => {
            // Redirect to help — just tell user to use /help
            ctx.say("Use `/help` to see all commands.").await?;
            return Ok(());
        }
        _ => {}
    }

    // Auto-restore session (must run before skill check to refresh skills_cache with project path)
    auto_restore_session(&ctx.data().shared, ctx.channel_id(), ctx.serenity_context()).await;

    // Verify skill exists
    let skill_exists = {
        let skills = ctx.data().shared.skills_cache.read().await;
        skills.iter().any(|(name, _)| name == &skill)
    };

    if !skill_exists {
        // Treat unregistered skill as a regular user prompt forwarded to the AI provider.
        let full_text = if args_str.is_empty() {
            format!("/{skill}")
        } else {
            format!("/{skill} {args_str}")
        };
        let channel_id = ctx.channel_id();
        let serenity_ctx = ctx.serenity_context();
        ctx.defer().await?;
        let data = ctx.data();
        let confirm_id = send_command_confirmation_message(
            &data.token,
            channel_id,
            format!(
                "↪ Forwarding unknown skill `{}` to the AI provider as a regular prompt.",
                full_text
            ),
        )
        .await?;
        auto_restore_session(&ctx.data().shared, channel_id, serenity_ctx).await;
        let deps = IntakeDeps {
            http: &serenity_ctx.http,
            cache: Some(&serenity_ctx.cache),
            ctx_for_chained_dispatch: Some(serenity_ctx),
            shared: &data.shared,
            token: &data.token,
        };
        dispatch_skill_intake(
            &deps,
            data.provider.clone(),
            channel_id,
            confirm_id,
            ctx.author().id,
            ctx.author().name.clone(),
            full_text,
            IntakeOrigin::SlashSkill,
            Vec::new(),
        )
        .await?;
        return Ok(());
    }

    // Check session exists
    let has_session = {
        let data = ctx.data().shared.core.lock().await;
        data.sessions
            .get(&ctx.channel_id())
            .and_then(|s| s.current_path.as_ref())
            .is_some()
    };

    if !has_session {
        ctx.say("No active session. Use `/start <path>` first.")
            .await?;
        return Ok(());
    }

    // Block if AI is in progress
    if mailbox_has_active_turn(&ctx.data().shared, ctx.channel_id()).await {
        ctx.say("AI request in progress. Use `/stop` to cancel.")
            .await?;
        return Ok(());
    }

    // Build the prompt that tells the active provider to invoke the skill
    let skill_prompt = match build_provider_skill_prompt(&ctx.data().provider, &skill, args_str) {
        Ok(prompt) => prompt,
        Err(message) => {
            ctx.say(message).await?;
            return Ok(());
        }
    };

    // Send a confirmation message that we can use as the "user message" for reactions
    ctx.defer().await?;
    let data = ctx.data();
    let confirm_id = send_command_confirmation_message(
        &data.token,
        ctx.channel_id(),
        format!("⚡ Running skill: `/{skill}`"),
    )
    .await?;

    // Hand off to the text message handler (it creates its own placeholder)
    let serenity_ctx = ctx.serenity_context();
    let deps = IntakeDeps {
        http: &serenity_ctx.http,
        cache: Some(&serenity_ctx.cache),
        ctx_for_chained_dispatch: Some(serenity_ctx),
        shared: &data.shared,
        token: &data.token,
    };
    dispatch_skill_intake(
        &deps,
        data.provider.clone(),
        ctx.channel_id(),
        confirm_id,
        ctx.author().id,
        ctx.author().name.clone(),
        skill_prompt,
        IntakeOrigin::SlashSkill,
        Vec::new(),
    )
    .await?;

    Ok(())
}

/// /skill <skill> [args] — Run a provider skill
#[poise::command(slash_command, rename = "skill")]
pub(in crate::services::discord) async fn cmd_skill(
    ctx: Context<'_>,
    #[description = "Skill name"]
    #[autocomplete = "autocomplete_skill"]
    skill: String,
    #[description = "Additional arguments for the skill"] args: Option<String>,
) -> Result<(), Error> {
    run_skill_slash_command(ctx, "/skill", skill, args).await
}

/// /cc <skill> [args] — Legacy alias for /skill
#[poise::command(slash_command, rename = "cc")]
pub(in crate::services::discord) async fn cmd_cc(
    ctx: Context<'_>,
    #[description = "Skill name"]
    #[autocomplete = "autocomplete_skill"]
    skill: String,
    #[description = "Additional arguments for the skill"] args: Option<String>,
) -> Result<(), Error> {
    run_skill_slash_command(ctx, "/cc", skill, args).await
}
