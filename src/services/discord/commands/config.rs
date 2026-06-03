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

pub(in crate::services::discord) async fn effective_provider_for_channel(
    shared: &Arc<SharedData>,
    channel_id: serenity::ChannelId,
    provider: &ProviderKind,
    channel_name_hint: Option<&str>,
) -> ProviderKind {
    let session_channel_name = {
        let data = shared.core.lock().await;
        data.sessions
            .get(&channel_id)
            .and_then(|session| session.channel_name.clone())
    };
    let channel_name = session_channel_name.as_deref().or(channel_name_hint);
    shared
        .dispatch_role_overrides
        .get(&channel_id)
        .map(|value| *value)
        .and_then(|override_channel| resolve_role_binding(override_channel, None))
        .or_else(|| resolve_role_binding(channel_id, channel_name))
        .and_then(|binding| binding.provider)
        .unwrap_or_else(|| provider.clone())
}

pub(in crate::services::discord) fn native_fast_mode_supported(provider: &ProviderKind) -> bool {
    matches!(provider, ProviderKind::Claude | ProviderKind::Codex)
}

pub(in crate::services::discord) fn codex_goals_supported(provider: &ProviderKind) -> bool {
    matches!(provider, ProviderKind::Codex)
}

pub(in crate::services::discord) fn session_toggle_reset_line(reset_pending: bool) -> &'static str {
    if reset_pending {
        "다음 사용자 턴 시작 전에 기존 세션을 정리한 뒤 반영됩니다."
    } else {
        "현재 세션부터 반영됩니다."
    }
}

pub(in crate::services::discord) async fn fallback_channel_name_for_feature_toggle(
    ctx: Context<'_>,
    channel_id: serenity::ChannelId,
) -> Option<String> {
    let http = ctx.serenity_context().http.clone();
    if let Some((parent_id, parent_name)) =
        super::super::resolve_thread_parent(&http, channel_id).await
    {
        let parent_name = parent_name.unwrap_or_else(|| parent_id.get().to_string());
        return Some(super::super::synthetic_thread_channel_name(
            &parent_name,
            channel_id,
        ));
    }

    channel_id
        .to_channel(&http)
        .await
        .ok()
        .and_then(|channel| match channel {
            serenity::Channel::Guild(guild_channel) => Some(guild_channel.name),
            _ => None,
        })
}

pub(in crate::services::discord) async fn channel_fast_mode_setting(
    shared: &Arc<SharedData>,
    channel_id: serenity::ChannelId,
) -> Option<bool> {
    let settings = shared.settings.read().await;
    settings
        .channel_fast_modes
        .get(&channel_id.get().to_string())
        .copied()
}

pub(in crate::services::discord) async fn channel_codex_goals_setting(
    shared: &Arc<SharedData>,
    channel_id: serenity::ChannelId,
) -> Option<bool> {
    let settings = shared.settings.read().await;
    settings
        .channel_codex_goals
        .get(&channel_id.get().to_string())
        .copied()
}

pub(in crate::services::discord) fn fast_mode_reset_pending_key(
    channel_id: serenity::ChannelId,
    provider: &ProviderKind,
) -> String {
    format!("{}:{}", provider.as_str(), channel_id.get())
}

pub(in crate::services::discord) fn parse_fast_mode_reset_pending_entry(
    entry: &str,
) -> Option<(Option<&str>, serenity::ChannelId)> {
    if let Some((provider_id, raw_channel_id)) = entry.split_once(':') {
        let channel_id = raw_channel_id
            .parse::<u64>()
            .ok()
            .map(serenity::ChannelId::new)?;
        return Some((Some(provider_id), channel_id));
    }

    entry
        .parse::<u64>()
        .ok()
        .map(serenity::ChannelId::new)
        .map(|channel_id| (None, channel_id))
}

fn fast_mode_reset_entry_matches_channel(entry: &str, channel_id: serenity::ChannelId) -> bool {
    parse_fast_mode_reset_pending_entry(entry)
        .map(|(_, entry_channel_id)| entry_channel_id == channel_id)
        .unwrap_or(false)
}

fn fast_mode_reset_entry_matches_provider(
    entry: &str,
    channel_id: serenity::ChannelId,
    provider: &ProviderKind,
) -> bool {
    parse_fast_mode_reset_pending_entry(entry)
        .map(|(provider_id, entry_channel_id)| {
            entry_channel_id == channel_id
                && provider_id
                    .map(|entry_provider| entry_provider.eq_ignore_ascii_case(provider.as_str()))
                    .unwrap_or(true)
        })
        .unwrap_or(false)
}

pub(in crate::services::discord) fn fast_mode_reset_pending_for_provider(
    shared: &Arc<SharedData>,
    channel_id: serenity::ChannelId,
    provider: &ProviderKind,
) -> bool {
    shared
        .fast_mode_session_reset_pending
        .iter()
        .any(|entry| fast_mode_reset_entry_matches_provider(entry.key(), channel_id, provider))
}

pub(in crate::services::discord) fn any_fast_mode_reset_pending(
    shared: &Arc<SharedData>,
    channel_id: serenity::ChannelId,
) -> bool {
    shared
        .fast_mode_session_reset_pending
        .iter()
        .any(|entry| fast_mode_reset_entry_matches_channel(entry.key(), channel_id))
}

pub(in crate::services::discord) fn clear_fast_mode_reset_pending_for_provider(
    shared: &Arc<SharedData>,
    channel_id: serenity::ChannelId,
    provider: &ProviderKind,
) -> bool {
    let provider_key = fast_mode_reset_pending_key(channel_id, provider);
    shared
        .fast_mode_session_reset_pending
        .remove(&provider_key)
        .is_some()
}

pub(in crate::services::discord) fn clear_fast_mode_reset_pending_for_channel(
    shared: &Arc<SharedData>,
    channel_id: serenity::ChannelId,
) -> bool {
    let keys: Vec<String> = shared
        .fast_mode_session_reset_pending
        .iter()
        .filter_map(|entry| {
            fast_mode_reset_entry_matches_channel(entry.key(), channel_id)
                .then(|| entry.key().clone())
        })
        .collect();

    let had_entries = !keys.is_empty();
    for key in keys {
        shared.fast_mode_session_reset_pending.remove(&key);
    }
    had_entries
}

pub(in crate::services::discord) fn sync_session_reset_pending(
    shared: &Arc<SharedData>,
    channel_id: serenity::ChannelId,
) {
    if any_fast_mode_reset_pending(shared, channel_id)
        || shared
            .codex_goals_session_reset_pending
            .contains(&channel_id)
        || shared.model_session_reset_pending.contains(&channel_id)
    {
        shared.session_reset_pending.insert(channel_id);
    } else {
        shared.session_reset_pending.remove(&channel_id);
    }
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

    load_last_session_path(
        shared.pg_pool.as_ref(),
        &shared.token_hash,
        channel_id.get(),
    )
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
    sync_session_reset_pending(shared, channel_id);

    true
}

pub(in crate::services::discord) fn channel_fast_mode_enabled(
    shared: &Arc<SharedData>,
    channel_id: serenity::ChannelId,
) -> bool {
    shared.fast_mode_channels.contains(&channel_id)
}

pub(in crate::services::discord) fn channel_codex_goals_enabled(
    shared: &Arc<SharedData>,
    channel_id: serenity::ChannelId,
) -> bool {
    shared.codex_goals_channels.contains(&channel_id)
}

pub(in crate::services::discord) async fn update_channel_fast_mode(
    shared: &Arc<SharedData>,
    token: &str,
    channel_id: serenity::ChannelId,
    provider: &ProviderKind,
    enabled: bool,
) -> bool {
    let current_enabled = channel_fast_mode_enabled(shared, channel_id);
    if current_enabled == enabled {
        return false;
    }

    if enabled {
        shared.fast_mode_channels.insert(channel_id);
    } else {
        shared.fast_mode_channels.remove(&channel_id);
    }

    let mut settings = shared.settings.write().await;
    if enabled {
        settings
            .channel_fast_modes
            .insert(channel_id.get().to_string(), true);
    } else {
        settings
            .channel_fast_modes
            .insert(channel_id.get().to_string(), false);
    }
    settings
        .channel_fast_mode_reset_pending
        .retain(|entry| !fast_mode_reset_entry_matches_provider(entry, channel_id, provider));
    settings
        .channel_fast_mode_reset_pending
        .insert(fast_mode_reset_pending_key(channel_id, provider));
    save_bot_settings(token, &settings);

    clear_fast_mode_reset_pending_for_provider(shared, channel_id, provider);
    shared
        .fast_mode_session_reset_pending
        .insert(fast_mode_reset_pending_key(channel_id, provider));
    sync_session_reset_pending(shared, channel_id);
    true
}

pub(in crate::services::discord) fn clear_codex_goals_reset_pending_for_channel(
    shared: &Arc<SharedData>,
    channel_id: serenity::ChannelId,
) -> bool {
    shared
        .codex_goals_session_reset_pending
        .remove(&channel_id)
        .is_some()
}

pub(in crate::services::discord) async fn update_channel_codex_goals(
    shared: &Arc<SharedData>,
    token: &str,
    channel_id: serenity::ChannelId,
    enabled: bool,
) -> bool {
    let current_enabled = channel_codex_goals_enabled(shared, channel_id);
    if current_enabled == enabled {
        return false;
    }

    if enabled {
        shared.codex_goals_channels.insert(channel_id);
    } else {
        shared.codex_goals_channels.remove(&channel_id);
    }

    let channel_key = channel_id.get().to_string();
    let mut settings = shared.settings.write().await;
    settings
        .channel_codex_goals
        .insert(channel_key.clone(), enabled);
    settings
        .channel_codex_goals_reset_pending
        .insert(channel_key);
    save_bot_settings(token, &settings);

    shared.codex_goals_session_reset_pending.insert(channel_id);
    sync_session_reset_pending(shared, channel_id);
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
        ProviderKind::OpenCode => 0x8B5CF6,
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
    // Issue #1005: tool-grant tier — owner-only AND default-disabled. The
    // ability to grant new tools (e.g. `+Bash`) escalates the model's
    // capability surface and must not be reachable via `allow_all_users`.
    if !super::enforce_slash_command_policy(&ctx, "/allowed").await? {
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

    // Issue #1005: when toggling public access, remind the operator that
    // high-risk commands (shell / tool grants / runtime control / credential
    // surface) stay owner-only regardless of the flag. Surfacing the policy
    // state here makes it clear that opening the chat to all users does NOT
    // open the operational kill switches.
    let policy_note = build_allowall_policy_note();
    let combined = format!("{response}\n\n{policy_note}");
    ctx.say(&combined).await?;
    tracing::info!("  [{ts}] ▶ {response}");
    Ok(())
}

/// Render the risk-policy reminder appended to `/allowall` responses.
///
/// Pulled out so it can be unit tested without standing up the slash-command
/// machinery. Wording references both `!`-text and `/`-slash variants so the
/// guarantee is unambiguous: enabling `allow_all_users` does not move any
/// high-risk gate on either surface.
pub(in crate::services::discord) fn build_allowall_policy_note() -> String {
    let high_risk_enabled = super::high_risk_enabled_via_env();
    let shell_state = if high_risk_enabled {
        "owner-only, ENABLED"
    } else {
        "owner-only, DISABLED (set AGENTDESK_DISCORD_HIGH_RISK_ENABLED=1)"
    };
    format!(
        "**Note (issue #1005):** `allow_all_users` only governs ordinary chat \
         access. High-risk commands stay owner-only on BOTH the `!` text and \
         `/` slash surfaces, regardless:\n\
         • shell/tool-grant (`!shell`/`!allowed`, `/shell`/`/allowed`) — {shell_state}\n\
         • credential/system (`!allowall`/`!adduser`/`!removeuser`/`!escalation`, `/allowall`/`/adduser`/`/removeuser`) — owner-only"
    )
}
