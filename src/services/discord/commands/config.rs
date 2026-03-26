use poise::serenity_prelude as serenity;

use super::super::formatting::{canonical_tool_name, risk_badge, send_long_message_ctx, tool_info};
use super::super::settings::{resolve_role_binding, save_bot_settings};
use super::super::{Context, Error, check_auth, check_owner};
use crate::services::provider::ProviderKind;

fn provider_supports_model_override(provider: &ProviderKind) -> bool {
    matches!(
        provider,
        ProviderKind::Claude | ProviderKind::Codex | ProviderKind::Gemini
    )
}

fn model_hint(provider: &ProviderKind) -> &'static str {
    match provider {
        ProviderKind::Claude => "예: opus / sonnet / haiku",
        ProviderKind::Codex => "예: gpt-5-codex / o3 / o4-mini",
        ProviderKind::Gemini => "예: gemini-2.5-pro / gemini-2.5-flash",
        ProviderKind::Unsupported(_) => "모델 이름 또는 default",
    }
}

/// /model — Set or view the model override for this channel
#[poise::command(slash_command, rename = "model")]
pub(in crate::services::discord) async fn cmd_model(
    ctx: Context<'_>,
    #[description = "Model name or 'default' to clear"] model: Option<String>,
) -> Result<(), Error> {
    let user_id = ctx.author().id;
    let user_name = &ctx.author().name;
    if !check_auth(user_id, user_name, &ctx.data().shared, &ctx.data().token).await {
        return Ok(());
    }

    let ts = chrono::Local::now().format("%H:%M:%S");
    let channel_id = ctx.channel_id();

    if !provider_supports_model_override(&ctx.data().provider) {
        println!("  [{ts}] ◀ [{user_name}] /model (unsupported provider)");
        ctx.say("Model override is only supported for Claude, Codex, and Gemini channels.")
            .await?;
        return Ok(());
    }

    match model {
        Some(m) => {
            if m == "default" || m == "none" || m == "clear" {
                ctx.data().shared.model_overrides.remove(&channel_id);
            } else {
                ctx.data()
                    .shared
                    .model_overrides
                    .insert(channel_id, m.clone());
            }
            let display = ctx
                .data()
                .shared
                .model_overrides
                .get(&channel_id)
                .map(|v| v.clone())
                .unwrap_or_else(|| "(default)".to_string());
            println!("  [{ts}] ◀ [{user_name}] /model {m}");
            ctx.say(format!(
                "Model set to **{display}** for this channel. Takes effect on next turn.\n{}",
                model_hint(&ctx.data().provider)
            ))
            .await?;
        }
        None => {
            let override_model = ctx
                .data()
                .shared
                .model_overrides
                .get(&channel_id)
                .map(|v| v.clone());
            let ch_name = {
                let d = ctx.data().shared.core.lock().await;
                d.sessions
                    .get(&channel_id)
                    .and_then(|s| s.channel_name.clone())
            };
            let role_model =
                resolve_role_binding(channel_id, ch_name.as_deref()).and_then(|rb| rb.model);
            let effective = override_model
                .as_deref()
                .or(role_model.as_deref())
                .unwrap_or("(default)");
            let source = if override_model.is_some() {
                "runtime override"
            } else if role_model.is_some() {
                "role-map"
            } else {
                "system default"
            };
            println!("  [{ts}] ◀ [{user_name}] /model");
            ctx.say(format!(
                "Model: **{effective}** (source: {source})\n{}",
                model_hint(&ctx.data().provider)
            ))
            .await?;
        }
    }
    Ok(())
}

/// /allowedtools — Show currently allowed tools
#[poise::command(slash_command, rename = "allowedtools")]
pub(in crate::services::discord) async fn cmd_allowedtools(ctx: Context<'_>) -> Result<(), Error> {
    let user_id = ctx.author().id;
    let user_name = &ctx.author().name;
    if !check_auth(user_id, user_name, &ctx.data().shared, &ctx.data().token).await {
        return Ok(());
    }

    let ts = chrono::Local::now().format("%H:%M:%S");
    println!("  [{ts}] ◀ [{user_name}] /allowedtools");

    let tools = {
        let settings = ctx.data().shared.settings.read().await;
        settings.allowed_tools.clone()
    };

    let mut msg = String::from("**Allowed Tools**\n\n");
    for tool in &tools {
        let (desc, destructive) = tool_info(tool);
        let badge = risk_badge(destructive);
        if badge.is_empty() {
            msg.push_str(&format!("`{}` — {}\n", tool, desc));
        } else {
            msg.push_str(&format!("`{}` {} — {}\n", tool, badge, desc));
        }
    }
    msg.push_str(&format!(
        "\n{} = destructive\nTotal: {}",
        risk_badge(true),
        tools.len()
    ));

    send_long_message_ctx(ctx, &msg).await?;
    Ok(())
}

/// /allowed <+/-tool> — Add or remove a tool
#[poise::command(slash_command, rename = "allowed")]
pub(in crate::services::discord) async fn cmd_allowed(
    ctx: Context<'_>,
    #[description = "Use +name to add, -name to remove (e.g. +Bash or -Bash)"] action: String,
) -> Result<(), Error> {
    let user_id = ctx.author().id;
    let user_name = &ctx.author().name;
    if !check_auth(user_id, user_name, &ctx.data().shared, &ctx.data().token).await {
        return Ok(());
    }

    let ts = chrono::Local::now().format("%H:%M:%S");
    println!("  [{ts}] ◀ [{user_name}] /allowed {action}");

    let arg = action.trim();
    let (op, raw_name) = if let Some(name) = arg.strip_prefix('+') {
        ('+', name.trim())
    } else if let Some(name) = arg.strip_prefix('-') {
        ('-', name.trim())
    } else {
        ctx.say("Use `+toolname` to add or `-toolname` to remove.\nExample: `/allowed +Bash`")
            .await?;
        return Ok(());
    };

    if raw_name.is_empty() {
        ctx.say("Tool name cannot be empty.").await?;
        return Ok(());
    }

    let Some(tool_name) = canonical_tool_name(raw_name).map(str::to_string) else {
        ctx.say(format!(
            "Unknown tool `{}`. Use `/allowedtools` to see valid tool names.",
            raw_name
        ))
        .await?;
        return Ok(());
    };

    let response_msg = {
        let mut settings = ctx.data().shared.settings.write().await;
        match op {
            '+' => {
                if settings.allowed_tools.iter().any(|t| t == &tool_name) {
                    format!("`{}` is already in the list.", tool_name)
                } else {
                    settings.allowed_tools.push(tool_name.clone());
                    save_bot_settings(&ctx.data().token, &settings);
                    format!("Added `{}`", tool_name)
                }
            }
            '-' => {
                let before_len = settings.allowed_tools.len();
                settings.allowed_tools.retain(|t| t != &tool_name);
                if settings.allowed_tools.len() < before_len {
                    save_bot_settings(&ctx.data().token, &settings);
                    format!("Removed `{}`", tool_name)
                } else {
                    format!("`{}` is not in the list.", tool_name)
                }
            }
            _ => unreachable!(),
        }
    };

    ctx.say(&response_msg).await?;
    Ok(())
}

/// /adduser @user — Allow another user to use the bot (owner only)
#[poise::command(slash_command, rename = "adduser")]
pub(in crate::services::discord) async fn cmd_adduser(
    ctx: Context<'_>,
    #[description = "User to add"] user: serenity::User,
) -> Result<(), Error> {
    let author_id = ctx.author().id;
    let author_name = &ctx.author().name;
    if !check_auth(
        author_id,
        author_name,
        &ctx.data().shared,
        &ctx.data().token,
    )
    .await
    {
        return Ok(());
    }
    if !check_owner(author_id, &ctx.data().shared).await {
        ctx.say("Only the owner can add users.").await?;
        return Ok(());
    }

    let target_id = user.id.get();
    let target_name = &user.name;

    let ts = chrono::Local::now().format("%H:%M:%S");
    println!("  [{ts}] ◀ [{author_name}] /adduser {target_name}");

    {
        let mut settings = ctx.data().shared.settings.write().await;
        if settings.allowed_user_ids.contains(&target_id) {
            ctx.say(format!("`{}` is already authorized.", target_name))
                .await?;
            return Ok(());
        }
        settings.allowed_user_ids.push(target_id);
        save_bot_settings(&ctx.data().token, &settings);
    }

    ctx.say(format!("Added `{}` as authorized user.", target_name))
        .await?;
    println!("  [{ts}] ▶ Added user: {target_name} (id:{target_id})");
    Ok(())
}

/// /removeuser @user — Remove a user's access (owner only)
#[poise::command(slash_command, rename = "removeuser")]
pub(in crate::services::discord) async fn cmd_removeuser(
    ctx: Context<'_>,
    #[description = "User to remove"] user: serenity::User,
) -> Result<(), Error> {
    let author_id = ctx.author().id;
    let author_name = &ctx.author().name;
    if !check_auth(
        author_id,
        author_name,
        &ctx.data().shared,
        &ctx.data().token,
    )
    .await
    {
        return Ok(());
    }
    if !check_owner(author_id, &ctx.data().shared).await {
        ctx.say("Only the owner can remove users.").await?;
        return Ok(());
    }

    let target_id = user.id.get();
    let target_name = &user.name;

    let ts = chrono::Local::now().format("%H:%M:%S");
    println!("  [{ts}] ◀ [{author_name}] /removeuser {target_name}");

    {
        let mut settings = ctx.data().shared.settings.write().await;
        let before_len = settings.allowed_user_ids.len();
        settings.allowed_user_ids.retain(|&id| id != target_id);
        if settings.allowed_user_ids.len() == before_len {
            ctx.say(format!("`{}` is not in the authorized list.", target_name))
                .await?;
            return Ok(());
        }
        save_bot_settings(&ctx.data().token, &settings);
    }

    ctx.say(format!("Removed `{}` from authorized users.", target_name))
        .await?;
    println!("  [{ts}] ▶ Removed user: {target_name} (id:{target_id})");
    Ok(())
}
