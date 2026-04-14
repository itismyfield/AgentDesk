use std::sync::Arc;
use std::time::{Duration, Instant};

use poise::serenity_prelude as serenity;

use super::super::formatting::{canonical_tool_name, risk_badge, send_long_message_ctx, tool_info};
use super::super::model_catalog::{
    SOURCE_DISPATCH_ROLE, SOURCE_PROVIDER_DEFAULT, SOURCE_ROLE_MAP, SOURCE_RUNTIME_OVERRIDE,
    is_default_picker_value,
};
use super::super::settings::{load_last_session_path, resolve_role_binding, save_bot_settings};
use super::super::{Context, Error, SharedData, check_auth, check_owner};
use super::model_ui::{
    build_model_picker_options, build_model_picker_summary_lines, has_pending_model_change,
};
use crate::services::provider::ProviderKind;

const MODEL_PICKER_PENDING_TTL: Duration = Duration::from_secs(30 * 60);
pub(in crate::services::discord) const MODEL_PICKER_CUSTOM_ID: &str = "agentdesk:model-picker";
pub(in crate::services::discord) const MODEL_SUBMIT_CUSTOM_ID: &str = "agentdesk:model-submit";
pub(in crate::services::discord) const MODEL_RESET_CUSTOM_ID: &str = "agentdesk:model-reset";
pub(in crate::services::discord) const MODEL_CANCEL_CUSTOM_ID: &str = "agentdesk:model-cancel";
const MODEL_PICKER_SUBMIT_LABEL: &str = "저장";
const MODEL_PICKER_RESET_LABEL: &str = "기본값";
const MODEL_PICKER_CANCEL_LABEL: &str = "취소";
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::services::discord) enum ModelPickerAction {
    Select,
    Submit,
    Reset,
    Cancel,
}

pub(in crate::services::discord) fn same_model_override(
    current: Option<&str>,
    next: Option<&str>,
) -> bool {
    match (current, next) {
        (None, None) => true,
        (Some(lhs), Some(rhs)) => lhs.eq_ignore_ascii_case(rhs),
        _ => false,
    }
}

fn session_reset_required_for_model_change(
    provider: &ProviderKind,
    current_override: Option<&str>,
    next_override: Option<&str>,
) -> bool {
    if same_model_override(current_override, next_override) {
        return false;
    }

    if next_override.is_none() {
        return !provider.default_model_behavior().resume_without_reset;
    }

    true
}

// Source-label constants live in model_catalog; re-export locally for test readability.
use SOURCE_DISPATCH_ROLE as DISPATCH_ROLE_OVERRIDE_SOURCE;
use SOURCE_PROVIDER_DEFAULT as PROVIDER_DEFAULT_SOURCE;
use SOURCE_ROLE_MAP as ROLE_MAP_SOURCE;
use SOURCE_RUNTIME_OVERRIDE as RUNTIME_OVERRIDE_SOURCE;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(in crate::services::discord) struct EffectiveModelSnapshot {
    pub(in crate::services::discord) override_model: Option<String>,
    pub(in crate::services::discord) dispatch_role_model: Option<String>,
    pub(in crate::services::discord) role_model: Option<String>,
    pub(in crate::services::discord) effective: String,
    pub(in crate::services::discord) source: &'static str,
    pub(in crate::services::discord) default_model: String,
    pub(in crate::services::discord) default_source: &'static str,
}

fn resolve_effective_model(
    override_model: Option<&str>,
    dispatch_role_model: Option<&str>,
    role_model: Option<&str>,
) -> (String, &'static str) {
    if let Some(model) = override_model {
        (model.to_string(), RUNTIME_OVERRIDE_SOURCE)
    } else if let Some(model) = dispatch_role_model {
        (model.to_string(), DISPATCH_ROLE_OVERRIDE_SOURCE)
    } else if let Some(model) = role_model {
        (model.to_string(), ROLE_MAP_SOURCE)
    } else {
        ("default".to_string(), PROVIDER_DEFAULT_SOURCE)
    }
}

fn resolve_default_model(
    dispatch_role_model: Option<&str>,
    role_model: Option<&str>,
) -> (String, &'static str) {
    if let Some(model) = dispatch_role_model {
        (model.to_string(), DISPATCH_ROLE_OVERRIDE_SOURCE)
    } else if let Some(model) = role_model {
        (model.to_string(), ROLE_MAP_SOURCE)
    } else {
        ("default".to_string(), PROVIDER_DEFAULT_SOURCE)
    }
}

fn resolve_dispatch_role_model(
    shared: &Arc<SharedData>,
    channel_id: serenity::ChannelId,
) -> Option<String> {
    let override_channel = shared
        .dispatch_role_overrides
        .get(&channel_id)
        .map(|value| *value)?;
    resolve_role_binding(override_channel, None).and_then(|binding| binding.model)
}

pub(in crate::services::discord) async fn effective_model_snapshot(
    shared: &Arc<SharedData>,
    channel_id: serenity::ChannelId,
) -> EffectiveModelSnapshot {
    let override_model = shared
        .model_overrides
        .get(&channel_id)
        .map(|value| value.clone());
    let dispatch_role_model = resolve_dispatch_role_model(shared, channel_id);
    let channel_name = {
        let data = shared.core.lock().await;
        data.sessions
            .get(&channel_id)
            .and_then(|session| session.channel_name.clone())
    };
    let role_model =
        resolve_role_binding(channel_id, channel_name.as_deref()).and_then(|binding| binding.model);
    let (effective, source) = resolve_effective_model(
        override_model.as_deref(),
        dispatch_role_model.as_deref(),
        role_model.as_deref(),
    );
    let (default_model, default_source) =
        resolve_default_model(dispatch_role_model.as_deref(), role_model.as_deref());

    EffectiveModelSnapshot {
        override_model,
        dispatch_role_model,
        role_model,
        effective,
        source,
        default_model,
        default_source,
    }
}

pub(in crate::services::discord) async fn current_working_dir(
    shared: &Arc<SharedData>,
    channel_id: serenity::ChannelId,
) -> Option<String> {
    if let Some(path) = {
        let data = shared.core.lock().await;
        data.sessions
            .get(&channel_id)
            .and_then(|session| session.current_path.clone())
    } {
        return Some(path);
    }

    load_last_session_path(shared.db.as_ref(), &shared.token_hash, channel_id.get())
}

fn runtime_model_for_turn(
    provider: &ProviderKind,
    effective_model: &str,
    source: &'static str,
) -> Option<String> {
    if source == PROVIDER_DEFAULT_SOURCE && effective_model.eq_ignore_ascii_case("default") {
        provider
            .default_model_behavior()
            .runtime_model
            .map(ToString::to_string)
    } else {
        Some(effective_model.to_string())
    }
}

pub(in crate::services::discord) async fn update_channel_model_override(
    shared: &Arc<SharedData>,
    token: &str,
    channel_id: serenity::ChannelId,
    provider: &ProviderKind,
    next_override: Option<String>,
) -> bool {
    if !would_channel_model_override_change(shared, channel_id, next_override.as_deref()) {
        return false;
    }

    let current_override = shared
        .model_overrides
        .get(&channel_id)
        .map(|value| value.clone());
    let reset_required = session_reset_required_for_model_change(
        provider,
        current_override.as_deref(),
        next_override.as_deref(),
    );

    match next_override {
        Some(model) => {
            shared.model_overrides.insert(channel_id, model.clone());
            let mut settings = shared.settings.write().await;
            settings
                .channel_model_overrides
                .insert(channel_id.get().to_string(), model);
            save_bot_settings(token, &settings);
        }
        None => {
            shared.model_overrides.remove(&channel_id);
            let mut settings = shared.settings.write().await;
            settings
                .channel_model_overrides
                .remove(&channel_id.get().to_string());
            save_bot_settings(token, &settings);
        }
    }

    if reset_required {
        shared.model_session_reset_pending.insert(channel_id);
    } else {
        shared.model_session_reset_pending.remove(&channel_id);
    }

    true
}

pub(in crate::services::discord) fn would_channel_model_override_change(
    shared: &Arc<SharedData>,
    channel_id: serenity::ChannelId,
    next_override: Option<&str>,
) -> bool {
    let current_override = shared
        .model_overrides
        .get(&channel_id)
        .map(|value| value.clone());
    !same_model_override(current_override.as_deref(), next_override)
}

pub(in crate::services::discord) async fn resolve_model_for_turn(
    shared: &Arc<SharedData>,
    channel_id: serenity::ChannelId,
    provider: &ProviderKind,
) -> Option<String> {
    let snapshot = effective_model_snapshot(shared, channel_id).await;
    runtime_model_for_turn(provider, &snapshot.effective, snapshot.source)
}

fn prune_model_picker_pending(shared: &Arc<SharedData>) {
    let now = Instant::now();
    let expired: Vec<_> = shared
        .model_picker_pending
        .iter()
        .filter_map(|entry| {
            if now.duration_since(entry.updated_at) > MODEL_PICKER_PENDING_TTL {
                Some(*entry.key())
            } else {
                None
            }
        })
        .collect();

    for message_id in expired {
        shared.model_picker_pending.remove(&message_id);
    }
}

pub(in crate::services::discord) fn remember_model_picker_pending(
    shared: &Arc<SharedData>,
    message_id: serenity::MessageId,
    owner_user_id: serenity::UserId,
    target_channel_id: serenity::ChannelId,
    pending_model: Option<String>,
) {
    prune_model_picker_pending(shared);
    shared.model_picker_pending.insert(
        message_id,
        super::super::ModelPickerPendingState {
            owner_user_id,
            target_channel_id,
            pending_model,
            updated_at: Instant::now(),
        },
    );
}

pub(in crate::services::discord) fn clear_model_picker_pending(
    shared: &Arc<SharedData>,
    message_id: serenity::MessageId,
) {
    shared.model_picker_pending.remove(&message_id);
}

pub(in crate::services::discord) fn model_picker_pending_to_override(
    pending_model: Option<&str>,
) -> Option<Option<String>> {
    match pending_model {
        None => None,
        Some(value) if is_default_picker_value(value) => Some(None),
        Some(value) => Some(Some(value.to_string())),
    }
}

fn provider_card_color(provider: &ProviderKind) -> u32 {
    match provider {
        ProviderKind::Claude => 0xD97706,
        ProviderKind::Codex => 0x10B981,
        ProviderKind::Gemini => 0x3B82F6,
        ProviderKind::Qwen => 0x0EA5A4,
        ProviderKind::Unsupported(_) => 0x5865F2,
    }
}

pub(in crate::services::discord) fn build_model_picker_custom_id(
    target_channel_id: serenity::ChannelId,
) -> String {
    format!("{}:{}", MODEL_PICKER_CUSTOM_ID, target_channel_id.get())
}

pub(in crate::services::discord) fn build_model_submit_custom_id(
    target_channel_id: serenity::ChannelId,
) -> String {
    format!("{}:{}", MODEL_SUBMIT_CUSTOM_ID, target_channel_id.get())
}

pub(in crate::services::discord) fn build_model_reset_custom_id(
    target_channel_id: serenity::ChannelId,
) -> String {
    format!("{}:{}", MODEL_RESET_CUSTOM_ID, target_channel_id.get())
}

pub(in crate::services::discord) fn build_model_cancel_custom_id(
    target_channel_id: serenity::ChannelId,
) -> String {
    format!("{}:{}", MODEL_CANCEL_CUSTOM_ID, target_channel_id.get())
}

pub(in crate::services::discord) fn parse_model_picker_custom_id(
    custom_id: &str,
    fallback_channel_id: serenity::ChannelId,
) -> Option<(ModelPickerAction, serenity::ChannelId)> {
    fn parse_target(
        custom_id: &str,
        prefix: &str,
        fallback_channel_id: serenity::ChannelId,
    ) -> Option<serenity::ChannelId> {
        if custom_id == prefix {
            return Some(fallback_channel_id);
        }

        let raw_id = custom_id.strip_prefix(prefix)?.strip_prefix(':')?;
        let parsed = raw_id.parse::<u64>().ok()?;
        Some(serenity::ChannelId::new(parsed))
    }

    parse_target(custom_id, MODEL_PICKER_CUSTOM_ID, fallback_channel_id)
        .map(|channel_id| (ModelPickerAction::Select, channel_id))
        .or_else(|| {
            parse_target(custom_id, MODEL_SUBMIT_CUSTOM_ID, fallback_channel_id)
                .map(|channel_id| (ModelPickerAction::Submit, channel_id))
        })
        .or_else(|| {
            parse_target(custom_id, MODEL_RESET_CUSTOM_ID, fallback_channel_id)
                .map(|channel_id| (ModelPickerAction::Reset, channel_id))
        })
        .or_else(|| {
            parse_target(custom_id, MODEL_CANCEL_CUSTOM_ID, fallback_channel_id)
                .map(|channel_id| (ModelPickerAction::Cancel, channel_id))
        })
}

pub(in crate::services::discord) fn build_model_picker_embed_from_snapshot(
    snapshot: &EffectiveModelSnapshot,
    provider: &ProviderKind,
    pending_model: Option<&str>,
    notice: Option<&str>,
    working_dir: Option<&str>,
) -> serenity::CreateEmbed {
    let lines = build_model_picker_summary_lines(
        provider,
        &snapshot.effective,
        snapshot.source,
        pending_model,
        snapshot.override_model.as_deref(),
        notice,
        working_dir,
    );
    serenity::CreateEmbed::new()
        .title("Model Picker")
        .description(lines.join("\n"))
        .color(provider_card_color(provider))
}

pub(in crate::services::discord) fn build_model_picker_action_rows(
    target_channel_id: serenity::ChannelId,
    provider: &ProviderKind,
    pending_model: Option<&str>,
    override_model: Option<&str>,
    default_model: &str,
    default_source: &'static str,
    working_dir: Option<&str>,
) -> Vec<serenity::CreateActionRow> {
    let menu = serenity::CreateSelectMenu::new(
        build_model_picker_custom_id(target_channel_id),
        serenity::CreateSelectMenuKind::String {
            options: build_model_picker_options(
                provider,
                pending_model,
                override_model,
                default_model,
                default_source,
                working_dir,
            ),
        },
    )
    .placeholder("모델 선택")
    .min_values(1)
    .max_values(1);

    let can_submit = has_pending_model_change(pending_model, override_model);

    let submit_button =
        serenity::CreateButton::new(build_model_submit_custom_id(target_channel_id))
            .label(MODEL_PICKER_SUBMIT_LABEL)
            .style(serenity::ButtonStyle::Primary)
            .disabled(!can_submit);

    let can_reset = override_model.is_some()
        || pending_model.is_some_and(|pending| !is_default_picker_value(pending));
    let reset_button = serenity::CreateButton::new(build_model_reset_custom_id(target_channel_id))
        .label(MODEL_PICKER_RESET_LABEL)
        .style(serenity::ButtonStyle::Secondary)
        .disabled(!can_reset);

    let cancel_button =
        serenity::CreateButton::new(build_model_cancel_custom_id(target_channel_id))
            .label(MODEL_PICKER_CANCEL_LABEL)
            .style(serenity::ButtonStyle::Danger);

    vec![
        serenity::CreateActionRow::SelectMenu(menu),
        serenity::CreateActionRow::Buttons(vec![submit_button, reset_button, cancel_button]),
    ]
}

pub(in crate::services::discord) fn build_model_picker_components_from_snapshot(
    snapshot: &EffectiveModelSnapshot,
    target_channel_id: serenity::ChannelId,
    provider: &ProviderKind,
    pending_model: Option<&str>,
    working_dir: Option<&str>,
) -> Vec<serenity::CreateActionRow> {
    build_model_picker_action_rows(
        target_channel_id,
        provider,
        pending_model,
        snapshot.override_model.as_deref(),
        &snapshot.default_model,
        snapshot.default_source,
        working_dir,
    )
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
    tracing::info!("  [{ts}] ◀ [{user_name}] /allowedtools");

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
    tracing::info!("  [{ts}] ◀ [{user_name}] /allowed {action}");

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
    tracing::info!("  [{ts}] ◀ [{author_name}] /adduser {target_name}");

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
    tracing::info!("  [{ts}] ▶ Added user: {target_name} (id:{target_id})");
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
    tracing::info!("  [{ts}] ◀ [{author_name}] /removeuser {target_name}");

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
    tracing::info!("  [{ts}] ▶ Removed user: {target_name} (id:{target_id})");
    Ok(())
}

/// /allowall <enabled> — Allow every Discord user to use the bot (owner only)
#[poise::command(slash_command, rename = "allowall")]
pub(in crate::services::discord) async fn cmd_allowall(
    ctx: Context<'_>,
    #[description = "Enable public access for all Discord users"] enabled: bool,
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
        ctx.say("Only the owner can change public access.").await?;
        return Ok(());
    }

    let ts = chrono::Local::now().format("%H:%M:%S");
    tracing::info!("  [{ts}] ◀ [{author_name}] /allowall {enabled}");

    let response = {
        let mut settings = ctx.data().shared.settings.write().await;
        settings.allow_all_users = enabled;
        save_bot_settings(&ctx.data().token, &settings);
        if enabled {
            "Public access enabled. Any Discord user can talk to this bot in allowed channels."
                .to_string()
        } else {
            "Public access disabled. Only the owner and authorized users can talk to this bot."
                .to_string()
        }
    };

    ctx.say(&response).await?;
    tracing::info!("  [{ts}] ▶ {response}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::services::discord::model_catalog::{
        DEFAULT_PICKER_VALUE, known_models, validate_model_input,
    };

    use super::super::model_ui::{
        build_model_picker_option_specs, build_model_picker_summary_lines,
    };
    use super::*;

    #[test]
    fn model_picker_custom_id_round_trip() {
        let channel_id = serenity::ChannelId::new(123_456_789);
        let custom_id = build_model_picker_custom_id(channel_id);
        let parsed = parse_model_picker_custom_id(&custom_id, serenity::ChannelId::new(1));
        assert_eq!(parsed, Some((ModelPickerAction::Select, channel_id)));
    }

    #[test]
    fn model_reset_custom_id_round_trip() {
        let channel_id = serenity::ChannelId::new(987_654_321);
        let custom_id = build_model_reset_custom_id(channel_id);
        let parsed = parse_model_picker_custom_id(&custom_id, serenity::ChannelId::new(1));
        assert_eq!(parsed, Some((ModelPickerAction::Reset, channel_id)));
    }

    #[test]
    fn model_submit_custom_id_round_trip() {
        let channel_id = serenity::ChannelId::new(111_222_333);
        let custom_id = build_model_submit_custom_id(channel_id);
        let parsed = parse_model_picker_custom_id(&custom_id, serenity::ChannelId::new(1));
        assert_eq!(parsed, Some((ModelPickerAction::Submit, channel_id)));
    }

    #[test]
    fn model_cancel_custom_id_round_trip() {
        let channel_id = serenity::ChannelId::new(777_888_999);
        let custom_id = build_model_cancel_custom_id(channel_id);
        let parsed = parse_model_picker_custom_id(&custom_id, serenity::ChannelId::new(1));
        assert_eq!(parsed, Some((ModelPickerAction::Cancel, channel_id)));
    }

    #[test]
    fn legacy_custom_id_uses_fallback_channel() {
        let fallback = serenity::ChannelId::new(42);
        let parsed = parse_model_picker_custom_id(MODEL_PICKER_CUSTOM_ID, fallback);
        assert_eq!(parsed, Some((ModelPickerAction::Select, fallback)));
    }

    #[test]
    fn model_picker_option_specs_include_default_first_for_all_supported_providers() {
        for provider in [
            ProviderKind::Claude,
            ProviderKind::Codex,
            ProviderKind::Gemini,
        ] {
            let options = build_model_picker_option_specs(
                &provider,
                None,
                None,
                "default",
                PROVIDER_DEFAULT_SOURCE,
                None,
            );
            assert_eq!(options[0].value, DEFAULT_PICKER_VALUE);
            assert_eq!(options[0].label, "기본값");
            let expected = match provider {
                ProviderKind::Claude => "오버라이드 해제 -> default (Claude default alias)",
                ProviderKind::Codex | ProviderKind::Gemini | ProviderKind::Qwen => {
                    "오버라이드 해제 -> provider default"
                }
                ProviderKind::Unsupported(_) => unreachable!(),
            };
            assert_eq!(options[0].description, expected);
            assert!(options[0].selected);
        }
    }

    #[test]
    fn model_picker_option_specs_show_role_map_default_metadata() {
        let options = build_model_picker_option_specs(
            &ProviderKind::Claude,
            None,
            Some("claude-sonnet-4-6"),
            "claude-opus-4-6",
            ROLE_MAP_SOURCE,
            None,
        );
        assert_eq!(options[0].label, "기본값");
        assert_eq!(
            options[0].description,
            "오버라이드 해제 -> claude-opus-4-6 (role-map)"
        );
        assert!(!options[0].selected);
        assert_eq!(options[1].value, "sonnet");
    }

    #[test]
    fn model_picker_pending_to_override_interprets_default_sentinel() {
        assert_eq!(
            model_picker_pending_to_override(Some(DEFAULT_PICKER_VALUE)),
            Some(None)
        );
        assert_eq!(
            model_picker_pending_to_override(Some("gpt-5.4")),
            Some(Some("gpt-5.4".to_string()))
        );
    }

    #[test]
    fn pending_model_change_treats_default_as_clear_override() {
        assert!(!has_pending_model_change(None, None));
        assert!(!has_pending_model_change(Some(DEFAULT_PICKER_VALUE), None));
        assert!(has_pending_model_change(
            Some(DEFAULT_PICKER_VALUE),
            Some("gpt-5.4")
        ));
        assert!(!has_pending_model_change(Some("gpt-5.4"), Some("gpt-5.4")));
        assert!(has_pending_model_change(
            Some("gpt-5.4"),
            Some("gpt-5.4-mini")
        ));
    }

    #[test]
    fn clearing_override_skips_session_reset_for_codex_and_gemini() {
        assert!(!session_reset_required_for_model_change(
            &ProviderKind::Codex,
            Some("gpt-5.4"),
            None,
        ));
        assert!(!session_reset_required_for_model_change(
            &ProviderKind::Gemini,
            Some("gemini-2.5-pro"),
            None,
        ));
    }

    #[test]
    fn clearing_override_skips_session_reset_for_claude_default_alias() {
        assert!(!session_reset_required_for_model_change(
            &ProviderKind::Claude,
            Some("opus"),
            None,
        ));
    }

    #[test]
    fn runtime_turn_model_uses_claude_default_alias_after_clear() {
        assert_eq!(
            runtime_model_for_turn(&ProviderKind::Claude, "default", PROVIDER_DEFAULT_SOURCE),
            Some("default".to_string())
        );
    }

    #[test]
    fn runtime_turn_model_omits_provider_default_for_codex_and_gemini() {
        assert_eq!(
            runtime_model_for_turn(&ProviderKind::Codex, "default", PROVIDER_DEFAULT_SOURCE),
            None
        );
        assert_eq!(
            runtime_model_for_turn(&ProviderKind::Gemini, "default", PROVIDER_DEFAULT_SOURCE),
            None
        );
    }

    #[test]
    fn effective_resolution_prefers_dispatch_role_over_role_map() {
        let (model, source) = resolve_effective_model(None, Some("gpt-5.4"), Some("gpt-5.4-mini"));
        assert_eq!(model, "gpt-5.4");
        assert_eq!(source, DISPATCH_ROLE_OVERRIDE_SOURCE);
    }

    #[test]
    fn default_resolution_reports_provider_default_when_no_fallbacks_exist() {
        let (model, source) = resolve_default_model(None, None);
        assert_eq!(model, "default");
        assert_eq!(source, PROVIDER_DEFAULT_SOURCE);
    }

    #[test]
    fn validate_model_input_accepts_claude_1m_aliases_and_full_names() {
        assert_eq!(
            validate_model_input(&ProviderKind::Claude, "sonnet[1m]", None).unwrap(),
            "sonnet[1m]"
        );
        assert_eq!(
            validate_model_input(
                &ProviderKind::Claude,
                "anthropic.claude-sonnet-4-20250514-v1:0[1m]",
                None,
            )
            .unwrap(),
            "anthropic.claude-sonnet-4-20250514-v1:0[1m]"
        );
    }

    #[test]
    fn validate_model_input_accepts_gemini_31_preview_and_auto_aliases() {
        assert_eq!(
            validate_model_input(&ProviderKind::Gemini, "gemini-3.1-pro-preview", None).unwrap(),
            "gemini-3.1-pro-preview"
        );
        assert_eq!(
            validate_model_input(&ProviderKind::Gemini, "auto", None).unwrap(),
            "auto-gemini-3"
        );
        assert_eq!(
            validate_model_input(&ProviderKind::Gemini, "pro", None).unwrap(),
            "gemini-3.1-pro-preview"
        );
        assert_eq!(
            validate_model_input(&ProviderKind::Gemini, "flash-lite", None).unwrap(),
            "gemini-2.5-flash-lite"
        );
    }

    #[test]
    fn codex_catalog_includes_spark_preview_entry() {
        let spark = known_models(&ProviderKind::Codex)
            .iter()
            .find(|entry| entry.value == "gpt-5.3-codex-spark")
            .expect("spark entry missing");
        assert_eq!(spark.label, "GPT-5.3-Codex-Spark");
        let description = spark.picker_description();
        assert!(description.to_ascii_lowercase().contains("text-only"));
        assert!(description.contains("API"));
    }

    #[test]
    fn all_catalog_entries_use_compact_pipe_summaries() {
        for catalog in [
            known_models(&ProviderKind::Claude),
            known_models(&ProviderKind::Codex),
            known_models(&ProviderKind::Gemini),
        ] {
            for entry in catalog {
                let description = entry.picker_description();
                assert!(
                    description.matches('|').count() == 1,
                    "description format drift for {}",
                    entry.value
                );
                assert!(
                    description.len() <= 100,
                    "description too long for {}",
                    entry.value
                );
            }
        }
    }

    #[test]
    fn picker_options_keep_descriptions_in_dropdown_for_curated_models() {
        let options = build_model_picker_option_specs(
            &ProviderKind::Codex,
            None,
            None,
            "gpt-5.4",
            ROLE_MAP_SOURCE,
            None,
        );
        assert_eq!(options[0].label, "기본값");
        assert_eq!(
            options[0].description,
            "오버라이드 해제 -> gpt-5.4 (role-map)"
        );
        let gpt_54 = options
            .iter()
            .find(|entry| entry.value == "gpt-5.4")
            .expect("gpt-5.4 entry missing");
        assert_eq!(gpt_54.label, "gpt-5.4");
        assert_eq!(
            gpt_54.description,
            "Frontier coding baseline | API $2.5/$15"
        );
        assert!(
            options
                .iter()
                .any(|entry| entry.label == "GPT-5.3-Codex-Spark"
                    && entry.description == "Text-only preview | No API")
        );
    }

    #[test]
    fn picker_options_use_system_default_description_without_role_map() {
        let options = build_model_picker_option_specs(
            &ProviderKind::Gemini,
            None,
            None,
            "default",
            PROVIDER_DEFAULT_SOURCE,
            None,
        );
        assert_eq!(options[0].label, "기본값");
        assert_eq!(
            options[0].description,
            "오버라이드 해제 -> provider default"
        );
        assert!(options.iter().any(|entry| entry.label == "Auto (Gemini 3)"
            && entry.description == "Preview auto routing | Pro/Flash preview"));
        assert!(
            options
                .iter()
                .any(|entry| entry.label == "Auto (Gemini 2.5)"
                    && entry.description == "Stable auto routing | Pro/Flash stable")
        );
        assert!(
            options
                .iter()
                .any(|entry| entry.label == "gemini-3.1-pro-preview"
                    && entry.description == "Gemini 3.1 Pro preview | Local CLI catalog")
        );
        assert!(
            options
                .iter()
                .any(|entry| entry.label == "gemini-3-pro-preview"
                    && entry.description == "Frontier reasoning and coding | $2/$12")
        );
        assert!(
            options
                .iter()
                .any(|entry| entry.label == "gemini-3-flash-preview"
                    && entry.description == "Low-latency frontier work | $0.5/$3")
        );
        assert!(
            options
                .iter()
                .any(|entry| entry.label == "gemini-2.5-flash-lite"
                    && entry.description == "Low-cost flash-lite | Local CLI catalog")
        );
        if let Some(entry) = options
            .iter()
            .find(|entry| entry.label == "gemini-3.1-flash-lite-preview")
        {
            assert_eq!(
                entry.description,
                "Preview flash-lite variant | Local CLI catalog"
            );
        }
    }

    #[test]
    fn claude_picker_mentions_default_alias_without_role_map() {
        let options = build_model_picker_option_specs(
            &ProviderKind::Claude,
            None,
            None,
            "default",
            PROVIDER_DEFAULT_SOURCE,
            None,
        );
        assert_eq!(options[0].label, "기본값");
        assert_eq!(
            options[0].description,
            "오버라이드 해제 -> default (Claude default alias)"
        );
    }

    #[test]
    fn claude_picker_uses_cli_safe_aliases() {
        let options = build_model_picker_option_specs(
            &ProviderKind::Claude,
            None,
            None,
            "default",
            PROVIDER_DEFAULT_SOURCE,
            None,
        );
        assert!(
            options
                .iter()
                .any(|entry| entry.value == "sonnet" && entry.label == "Sonnet 4.6")
        );
        assert!(options.iter().any(|entry| entry.value == "sonnet[1m]"
            && entry.label == "Sonnet 4.6 1M"
            && entry.description == "1M context window | Sonnet 4.6 alias"));
        assert!(options.iter().any(|entry| entry.value == "opus[1m]"
            && entry.label == "Opus 4.6 1M"
            && entry.description == "1M context window | Opus 4.6 alias"));
        assert!(options.iter().any(|entry| entry.value == "opusplan"
            && entry.label == "Opus Plan 4.6"
            && entry.description == "Opus 4.6 planning | Sonnet 4.6 executes"));
    }

    #[test]
    fn model_picker_summary_lines_stay_compact() {
        let lines = build_model_picker_summary_lines(
            &ProviderKind::Gemini,
            "gemini-3-flash-preview",
            ROLE_MAP_SOURCE,
            None,
            None,
            None,
            None,
        );
        assert_eq!(
            lines,
            [
                "Provider : `gemini`".to_string(),
                "Current Model : `gemini-3-flash-preview`".to_string(),
                "현재 작업 상태 : 기본 설정 사용 중".to_string(),
            ]
        );
    }

    #[test]
    fn model_picker_summary_lines_show_pending_save_state() {
        let lines = build_model_picker_summary_lines(
            &ProviderKind::Codex,
            "gpt-5.4",
            ROLE_MAP_SOURCE,
            Some("gpt-5.4-mini"),
            Some("gpt-5.4"),
            None,
            None,
        );
        assert_eq!(lines[2], "현재 작업 상태 : `gpt-5.4-mini` 저장 대기");
    }

    #[test]
    fn model_picker_action_rows_include_submit_reset_cancel_buttons() {
        let rows = build_model_picker_action_rows(
            serenity::ChannelId::new(42),
            &ProviderKind::Codex,
            None,
            None,
            "gpt-5.4",
            ROLE_MAP_SOURCE,
            None,
        );
        assert_eq!(rows.len(), 2);
        let controls = format!("{:?}", rows[1]);
        assert!(controls.contains(MODEL_PICKER_SUBMIT_LABEL));
        assert!(controls.contains(MODEL_PICKER_RESET_LABEL));
        assert!(controls.contains(MODEL_PICKER_CANCEL_LABEL));
    }
}
