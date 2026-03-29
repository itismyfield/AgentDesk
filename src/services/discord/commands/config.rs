use std::sync::Arc;

use poise::CreateReply;
use poise::serenity_prelude as serenity;

use super::super::formatting::{canonical_tool_name, risk_badge, send_long_message_ctx, tool_info};
use super::super::settings::{resolve_role_binding, save_bot_settings};
use super::super::{Context, Error, SharedData, check_auth, check_owner};
use crate::services::provider::ProviderKind;

const MODEL_CLEAR_KEYWORDS: &[&str] = &["default", "none", "clear"];
const MODEL_INFO_KEYWORDS: &[&str] = &["info"];
const MODEL_LIST_KEYWORDS: &[&str] = &["list"];
pub(in crate::services::discord) const MODEL_PICKER_CUSTOM_ID: &str = "agentdesk:model-picker";
pub(in crate::services::discord) const MODEL_RESET_CUSTOM_ID: &str = "agentdesk:model-reset";

#[derive(Clone, Copy)]
struct ModelCatalogEntry {
    value: &'static str,
    label: &'static str,
    description: &'static str,
}

// Curated from current official provider model docs as of 2026-03-27.
const CLAUDE_MODEL_CATALOG: &[ModelCatalogEntry] = &[
    ModelCatalogEntry {
        value: "claude-opus-4-6",
        label: "Claude Opus 4.6",
        description: "latest flagship",
    },
    ModelCatalogEntry {
        value: "claude-sonnet-4-6",
        label: "Claude Sonnet 4.6",
        description: "latest balanced model",
    },
    ModelCatalogEntry {
        value: "claude-opus-4-5-20251101",
        label: "Claude Opus 4.5",
        description: "previous opus generation",
    },
    ModelCatalogEntry {
        value: "claude-sonnet-4-5-20250929",
        label: "Claude Sonnet 4.5",
        description: "strong coding and agents",
    },
    ModelCatalogEntry {
        value: "claude-haiku-4-5-20251001",
        label: "Claude Haiku 4.5",
        description: "latest fast model",
    },
    ModelCatalogEntry {
        value: "claude-opus-4-1-20250805",
        label: "Claude Opus 4.1",
        description: "stable legacy opus",
    },
    ModelCatalogEntry {
        value: "claude-opus-4-0",
        label: "Claude Opus 4",
        description: "older opus generation",
    },
    ModelCatalogEntry {
        value: "claude-sonnet-4-0",
        label: "Claude Sonnet 4",
        description: "older balanced model",
    },
];

const CODEX_MODEL_CATALOG: &[ModelCatalogEntry] = &[
    ModelCatalogEntry {
        value: "gpt-5.4",
        label: "GPT-5.4",
        description: "latest frontier model",
    },
    ModelCatalogEntry {
        value: "gpt-5.4-pro",
        label: "GPT-5.4 Pro",
        description: "highest precision reasoning",
    },
    ModelCatalogEntry {
        value: "gpt-5.4-mini",
        label: "GPT-5.4 Mini",
        description: "fast strong mini",
    },
    ModelCatalogEntry {
        value: "gpt-5-mini",
        label: "GPT-5 Mini",
        description: "lower-latency general model",
    },
    ModelCatalogEntry {
        value: "gpt-5",
        label: "GPT-5",
        description: "previous frontier baseline",
    },
    ModelCatalogEntry {
        value: "gpt-5.3-codex",
        label: "GPT-5.3 Codex",
        description: "latest codex-specific model",
    },
    ModelCatalogEntry {
        value: "gpt-5.2-codex",
        label: "GPT-5.2 Codex",
        description: "long-horizon coding",
    },
    ModelCatalogEntry {
        value: "gpt-5.1-codex-max",
        label: "GPT-5.1 Codex Max",
        description: "best long-running agent",
    },
];

const GEMINI_MODEL_CATALOG: &[ModelCatalogEntry] = &[
    ModelCatalogEntry {
        value: "gemini-3-flash-preview",
        label: "Gemini 3 Flash Preview",
        description: "latest general preview",
    },
    ModelCatalogEntry {
        value: "gemini-2.5-pro",
        label: "Gemini 2.5 Pro",
        description: "best stable reasoning",
    },
    ModelCatalogEntry {
        value: "gemini-2.5-flash",
        label: "Gemini 2.5 Flash",
        description: "stable fast model",
    },
    ModelCatalogEntry {
        value: "gemini-2.5-flash-preview-09-2025",
        label: "Gemini 2.5 Flash Preview",
        description: "latest flash preview",
    },
    ModelCatalogEntry {
        value: "gemini-2.5-flash-lite",
        label: "Gemini 2.5 Flash-Lite",
        description: "fastest cheap stable model",
    },
    ModelCatalogEntry {
        value: "gemini-2.5-flash-lite-preview-09-2025",
        label: "Gemini 2.5 Flash-Lite Preview",
        description: "latest lite preview",
    },
    ModelCatalogEntry {
        value: "gemini-2.5-flash-native-audio-preview-12-2025",
        label: "Gemini 2.5 Flash Live",
        description: "latest live audio model",
    },
    ModelCatalogEntry {
        value: "gemini-flash-latest",
        label: "Gemini Flash Latest",
        description: "latest rolling flash alias",
    },
];

const CLAUDE_MODEL_ALIASES: &[(&str, &str)] = &[
    ("opus", "claude-opus-4-6"),
    ("sonnet", "claude-sonnet-4-6"),
    ("haiku", "claude-haiku-4-5-20251001"),
];

const CODEX_MODEL_ALIASES: &[(&str, &str)] = &[
    ("gpt-5-codex", "gpt-5-codex"),
    ("o3", "o3"),
    ("o4-mini", "o4-mini"),
];

const GEMINI_MODEL_ALIASES: &[(&str, &str)] = &[
    ("gemini-2.5-pro", "gemini-2.5-pro"),
    ("gemini-2.5-flash", "gemini-2.5-flash"),
    ("gemini-2.0-flash", "gemini-2.0-flash"),
];

pub(in crate::services::discord) fn provider_supports_model_override(
    provider: &ProviderKind,
) -> bool {
    matches!(
        provider,
        ProviderKind::Claude | ProviderKind::Codex | ProviderKind::Gemini
    )
}

pub(in crate::services::discord) fn model_hint(provider: &ProviderKind) -> &'static str {
    match provider {
        ProviderKind::Claude => "default + 최신 Claude top 8",
        ProviderKind::Codex => "default + 최신 Codex top 8",
        ProviderKind::Gemini => "default + 최신 Gemini top 8",
        ProviderKind::Unsupported(_) => "모델 이름 또는 default",
    }
}

fn known_models(provider: &ProviderKind) -> &'static [ModelCatalogEntry] {
    match provider {
        ProviderKind::Claude => CLAUDE_MODEL_CATALOG,
        ProviderKind::Codex => CODEX_MODEL_CATALOG,
        ProviderKind::Gemini => GEMINI_MODEL_CATALOG,
        ProviderKind::Unsupported(_) => &[],
    }
}

fn model_aliases(provider: &ProviderKind) -> &'static [(&'static str, &'static str)] {
    match provider {
        ProviderKind::Claude => CLAUDE_MODEL_ALIASES,
        ProviderKind::Codex => CODEX_MODEL_ALIASES,
        ProviderKind::Gemini => GEMINI_MODEL_ALIASES,
        ProviderKind::Unsupported(_) => &[],
    }
}

fn normalize_keyword<'a>(raw: &'a str, keywords: &[&str]) -> Option<&'a str> {
    let trimmed = raw.trim();
    if keywords.iter().any(|kw| kw.eq_ignore_ascii_case(trimmed)) {
        Some(trimmed)
    } else {
        None
    }
}

pub(in crate::services::discord) fn is_clear_model_keyword(raw: &str) -> bool {
    normalize_keyword(raw, MODEL_CLEAR_KEYWORDS).is_some()
}

fn canonical_known_model(provider: &ProviderKind, raw: &str) -> Option<&'static str> {
    let trimmed = raw.trim();
    if let Some(entry) = known_models(provider)
        .iter()
        .find(|entry| entry.value.eq_ignore_ascii_case(trimmed))
    {
        return Some(entry.value);
    }

    model_aliases(provider)
        .iter()
        .find(|(alias, _)| alias.eq_ignore_ascii_case(trimmed))
        .map(|(_, canonical)| *canonical)
}

fn looks_like_model_identifier(raw: &str) -> bool {
    let trimmed = raw.trim();
    !trimmed.is_empty()
        && trimmed.len() <= 64
        && trimmed
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | ':'))
}

pub(in crate::services::discord) fn validate_model_input(
    provider: &ProviderKind,
    raw: &str,
) -> Result<String, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err("Model name cannot be empty.".to_string());
    }

    if let Some(canonical) = canonical_known_model(provider, trimmed) {
        return Ok(canonical.to_string());
    }

    if looks_like_model_identifier(trimmed) {
        return Ok(trimmed.to_string());
    }

    Err(format!(
        "Unrecognized model `{}` for {}.\n{}\nUse `/model list` to see known examples.",
        trimmed,
        provider.display_name(),
        model_hint(provider)
    ))
}

fn doctor_guidance_suffix(provider: &ProviderKind) -> String {
    match provider.probe_runtime() {
        Some(probe) if probe.binary_path.is_some() && probe.version.is_some() => String::new(),
        _ => format!(
            "\nRuntime check: `{}` CLI probe is unavailable. Try `agentdesk doctor` or `agentdesk doctor --json`.",
            provider.as_str()
        ),
    }
}

fn runtime_probe_detail(provider: &ProviderKind) -> (String, String, String) {
    match provider.probe_runtime() {
        Some(probe) => {
            let binary_path = probe
                .binary_path
                .unwrap_or_else(|| "(not found)".to_string());
            let version = probe
                .version
                .unwrap_or_else(|| "(version unavailable)".to_string());
            let runtime_status = if binary_path == "(not found)" {
                "missing"
            } else if version == "(version unavailable)" {
                "degraded"
            } else {
                "ok"
            };
            (runtime_status.to_string(), binary_path, version)
        }
        None => (
            "unsupported".to_string(),
            "(unsupported)".to_string(),
            "(unsupported)".to_string(),
        ),
    }
}

async fn effective_model_snapshot(
    shared: &Arc<SharedData>,
    channel_id: serenity::ChannelId,
) -> (Option<String>, Option<String>, String, String, String) {
    let override_model = shared.model_overrides.get(&channel_id).map(|v| v.clone());
    let ch_name = {
        let d = shared.core.lock().await;
        d.sessions
            .get(&channel_id)
            .and_then(|s| s.channel_name.clone())
    };
    let role_model = resolve_role_binding(channel_id, ch_name.as_deref()).and_then(|rb| rb.model);
    let effective = override_model
        .as_deref()
        .or(role_model.as_deref())
        .unwrap_or("(default)")
        .to_string();
    let source = if override_model.is_some() {
        "runtime override"
    } else if role_model.is_some() {
        "role-map"
    } else {
        "system default"
    }
    .to_string();
    let default_model = role_model
        .clone()
        .unwrap_or_else(|| "system default".to_string());

    (override_model, role_model, effective, source, default_model)
}

pub(in crate::services::discord) async fn build_model_status_message(
    shared: &Arc<SharedData>,
    channel_id: serenity::ChannelId,
    provider: &ProviderKind,
) -> String {
    let (override_model, role_model, effective, source, _) =
        effective_model_snapshot(shared, channel_id).await;

    format!(
        "Provider: **{}**\nModel: **{}**\nSource: **{}**\nRuntime override: `{}`\nRole default: `{}`\nApplies: next turn\n{}",
        provider.display_name(),
        effective,
        source,
        override_model.as_deref().unwrap_or("(none)"),
        role_model.as_deref().unwrap_or("(none)"),
        model_hint(provider)
    )
}

pub(in crate::services::discord) async fn build_model_info_message(
    shared: &Arc<SharedData>,
    channel_id: serenity::ChannelId,
    provider: &ProviderKind,
) -> String {
    let (override_model, role_model, effective, source, _) =
        effective_model_snapshot(shared, channel_id).await;
    let (runtime_status, binary_path, version) = runtime_probe_detail(provider);
    let mut msg = format!(
        "Provider: **{}**\nModel: **{}**\nSource: **{}**\nRuntime override: `{}`\nRole default: `{}`\nRuntime CLI: **{}**\nBinary: `{}`\nVersion: `{}`\nApplies: next turn\n{}",
        provider.display_name(),
        effective,
        source,
        override_model.as_deref().unwrap_or("(none)"),
        role_model.as_deref().unwrap_or("(none)"),
        runtime_status,
        binary_path,
        version,
        model_hint(provider)
    );
    msg.push_str(&doctor_guidance_suffix(provider));
    msg
}

pub(in crate::services::discord) fn build_model_list_message(provider: &ProviderKind) -> String {
    let examples = known_models(provider);
    let list = if examples.is_empty() {
        "(no known examples)".to_string()
    } else {
        examples
            .iter()
            .map(|entry| format!("- {} — `{}`", entry.label, entry.value))
            .collect::<Vec<_>>()
            .join("\n")
    };

    format!(
        "**{} model examples**\n{}\n{}\nUse `/model <name>` to set one for this channel.{}",
        provider.display_name(),
        list,
        model_hint(provider),
        doctor_guidance_suffix(provider)
    )
}

fn build_model_picker_options(
    provider: &ProviderKind,
    current_override: Option<&str>,
) -> Vec<serenity::CreateSelectMenuOption> {
    let mut options = vec![
        serenity::CreateSelectMenuOption::new("default", "__default__")
            .description("use role-map or system default")
            .default_selection(current_override.is_none()),
    ];

    for entry in known_models(provider) {
        let option = serenity::CreateSelectMenuOption::new(entry.label, entry.value)
            .description(entry.description)
            .default_selection(
                current_override.is_some_and(|active| active.eq_ignore_ascii_case(entry.value)),
            );
        options.push(option);
    }

    options
}

pub(in crate::services::discord) async fn build_model_picker_embed(
    shared: &Arc<SharedData>,
    channel_id: serenity::ChannelId,
    provider: &ProviderKind,
) -> serenity::CreateEmbed {
    let (_, _, effective, source, default_model) =
        effective_model_snapshot(shared, channel_id).await;
    let mut description = format!(
        "Provider: **{}** (fixed)\nCurrent model: **{}**\nDefault: **{}**\nSource: **{}**\nApply: next turn\n\nSelect menu changes the server value immediately.",
        provider.display_name(),
        effective,
        default_model,
        source,
    );
    let doctor_hint = doctor_guidance_suffix(provider);
    if !doctor_hint.is_empty() {
        description.push_str(&format!("\n{}", doctor_hint.trim()));
    }

    serenity::CreateEmbed::new()
        .title("Model Picker")
        .description(description)
        .color(0x5865F2)
}

pub(in crate::services::discord) async fn build_model_picker_components(
    shared: &Arc<SharedData>,
    channel_id: serenity::ChannelId,
    provider: &ProviderKind,
) -> Vec<serenity::CreateActionRow> {
    let (override_model, _, _, _, _) = effective_model_snapshot(shared, channel_id).await;
    let menu = serenity::CreateSelectMenu::new(
        MODEL_PICKER_CUSTOM_ID,
        serenity::CreateSelectMenuKind::String {
            options: build_model_picker_options(provider, override_model.as_deref()),
        },
    )
    .placeholder("Select a model override")
    .min_values(1)
    .max_values(1);

    let reset_button = serenity::CreateButton::new(MODEL_RESET_CUSTOM_ID)
        .label("Reset to default")
        .style(serenity::ButtonStyle::Secondary)
        .disabled(override_model.is_none());

    vec![
        serenity::CreateActionRow::SelectMenu(menu),
        serenity::CreateActionRow::Buttons(vec![reset_button]),
    ]
}

async fn autocomplete_model<'a>(
    ctx: Context<'a>,
    partial: &'a str,
) -> Vec<serenity::AutocompleteChoice> {
    let mut choices = Vec::new();
    let partial_lower = partial.to_ascii_lowercase();
    let provider = &ctx.data().provider;

    for keyword in ["info", "list", "default"] {
        if partial.is_empty() || keyword.contains(&partial_lower) {
            let label = format!(
                "{} — {}",
                keyword,
                match keyword {
                    "info" => "show provider, model, and source",
                    "list" => "show known model examples",
                    "default" => "clear runtime override",
                    _ => "",
                }
            );
            choices.push(serenity::AutocompleteChoice::new(
                label,
                keyword.to_string(),
            ));
        }
    }

    for entry in known_models(provider) {
        if choices.len() >= 25 {
            break;
        }
        let searchable = format!(
            "{} {} {}",
            entry.label.to_ascii_lowercase(),
            entry.value.to_ascii_lowercase(),
            entry.description.to_ascii_lowercase()
        );
        if partial.is_empty() || searchable.contains(&partial_lower) {
            let label = format!("{} — {}", entry.label, entry.description);
            choices.push(serenity::AutocompleteChoice::new(
                label,
                entry.value.to_string(),
            ));
        }
    }

    choices
}

/// /model — Set or view the model override for this channel
#[poise::command(slash_command, rename = "model")]
pub(in crate::services::discord) async fn cmd_model(
    ctx: Context<'_>,
    #[autocomplete = "autocomplete_model"]
    #[description = "Model name or 'default' to clear"]
    model: Option<String>,
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
            println!("  [{ts}] ◀ [{user_name}] /model {m}");

            if normalize_keyword(&m, MODEL_LIST_KEYWORDS).is_some() {
                let embed =
                    build_model_picker_embed(&ctx.data().shared, channel_id, &ctx.data().provider)
                        .await;
                let components = build_model_picker_components(
                    &ctx.data().shared,
                    channel_id,
                    &ctx.data().provider,
                )
                .await;
                ctx.send(CreateReply::default().embed(embed).components(components))
                    .await?;
                return Ok(());
            }

            if normalize_keyword(&m, MODEL_INFO_KEYWORDS).is_some() {
                let msg =
                    build_model_info_message(&ctx.data().shared, channel_id, &ctx.data().provider)
                        .await;
                ctx.say(msg).await?;
                return Ok(());
            }

            if normalize_keyword(&m, MODEL_CLEAR_KEYWORDS).is_some() {
                ctx.data().shared.model_overrides.remove(&channel_id);
                let msg = build_model_status_message(
                    &ctx.data().shared,
                    channel_id,
                    &ctx.data().provider,
                )
                .await;
                ctx.say(format!("Model override cleared.\n{}", msg)).await?;
                return Ok(());
            }

            let validated = match validate_model_input(&ctx.data().provider, &m) {
                Ok(model) => model,
                Err(message) => {
                    ctx.say(message).await?;
                    return Ok(());
                }
            };

            ctx.data()
                .shared
                .model_overrides
                .insert(channel_id, validated.clone());
            let msg =
                build_model_status_message(&ctx.data().shared, channel_id, &ctx.data().provider)
                    .await;
            ctx.say(format!(
                "Model set to **{}** for this channel.\n{}",
                validated, msg
            ))
            .await?;
        }
        None => {
            println!("  [{ts}] ◀ [{user_name}] /model");
            let embed =
                build_model_picker_embed(&ctx.data().shared, channel_id, &ctx.data().provider)
                    .await;
            let components =
                build_model_picker_components(&ctx.data().shared, channel_id, &ctx.data().provider)
                    .await;
            ctx.send(CreateReply::default().embed(embed).components(components))
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
