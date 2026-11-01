use std::sync::Arc;
use std::time::{Duration, Instant};

use poise::serenity_prelude as serenity;

use super::super::model_catalog::{
    SOURCE_DISPATCH_ROLE, SOURCE_PROVIDER_DEFAULT, SOURCE_ROLE_MAP, SOURCE_RUNTIME_OVERRIDE,
    is_default_picker_value,
};
use super::super::settings::{load_last_session_path, resolve_role_binding, save_bot_settings};
use super::super::{Context, Error, SharedData, check_auth, check_owner};
use super::model_ui::{
    build_model_picker_options, build_model_picker_summary_lines, has_pending_model_change,
    provider_card_color,
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
        .dispatch
        .role_overrides
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
        .dispatch
        .role_overrides
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
        "다음 사용자 턴 시작 전 세션 리셋이 필요합니다(맥락 소실). 리셋 후 새 세션에 반영됩니다."
    } else {
        "현재 세션을 유지하며 다음 사용자 턴부터 반영됩니다."
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
        .or_else(|| {
            SharedData::feature_settings_parent(channel_id)
                .and_then(|parent| settings.channel_fast_modes.get(&parent.get().to_string()))
        })
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
        .or_else(|| {
            SharedData::feature_settings_parent(channel_id)
                .and_then(|parent| settings.channel_codex_goals.get(&parent.get().to_string()))
        })
        .copied()
}

// #3038 S2: the session-override bookkeeping helpers (the fast-mode
// reset-entry codec, the reset-pending probes/clears, the fast-mode /
// Codex-goals enablement probes, and `sync_session_reset_pending`) moved
// verbatim to the `shared_state` sibling module alongside
// `SessionOverrideState` — they are the free-function surface that
// exclusively owns that cluster. Re-exported here so every existing
// `super::config::*` importer (control.rs / fast_mode.rs / goals.rs) and
// every unqualified call site in this module stays frozen.
pub(in crate::services::discord) use super::super::shared_state::{
    channel_codex_goals_enabled, channel_fast_mode_enabled,
    clear_codex_goals_reset_pending_for_channel, clear_fast_mode_reset_pending_for_channel,
    clear_fast_mode_reset_pending_for_provider, fast_mode_reset_entry_matches_provider,
    fast_mode_reset_pending_for_provider, fast_mode_reset_pending_key, sync_session_reset_pending,
};
// #3038 S2: `any_fast_mode_reset_pending`'s only caller outside `shared_state`
// is the S2-0 characterization test module below (which resolves it through
// `use super::*`), so its path-freezing re-export is test-gated to keep the
// production build warning-free. `parse_fast_mode_reset_pending_entry` has no
// caller outside `shared_state` at all and is intentionally not re-exported.
#[cfg(test)]
pub(in crate::services::discord) use super::super::shared_state::any_fast_mode_reset_pending;

pub(in crate::services::discord) async fn effective_model_snapshot(
    shared: &Arc<SharedData>,
    channel_id: serenity::ChannelId,
) -> EffectiveModelSnapshot {
    let override_model = shared
        .overrides
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

mod provider_settings;
pub(in crate::services::discord) use provider_settings::{
    claude_local_tui_relaunch_applies, update_channel_codex_goals, update_channel_fast_mode,
    update_channel_model_override,
};

pub(in crate::services::discord) fn would_channel_model_override_change(
    shared: &Arc<SharedData>,
    channel_id: serenity::ChannelId,
    next_override: Option<&str>,
) -> bool {
    let current_override = shared
        .overrides
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
        .overrides
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
        shared.overrides.model_picker_pending.remove(&message_id);
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
    shared.overrides.model_picker_pending.insert(
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
    shared.overrides.model_picker_pending.remove(&message_id);
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

/// `/allowall` action; `status` reports the setting without changing it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, poise::ChoiceParameter)]
enum AllowAllAction {
    #[name = "on"]
    On,
    #[name = "off"]
    Off,
    #[name = "status"]
    Status,
}

/// /allowall <on|off|status> — Allow every Discord user to use this bot (owner only)
#[poise::command(slash_command, rename = "allowall")]
pub(in crate::services::discord) async fn cmd_allowall(
    ctx: Context<'_>,
    #[description = "on, off, or status (show the current setting)"] action: AllowAllAction,
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

    log_command_received!(
        ctx.channel_id().get(),
        author_name,
        "/allowall",
        action = ?action
    );

    let response = {
        let mut settings = ctx.data().shared.settings.write().await;
        match action {
            AllowAllAction::Status if settings.allow_all_users => {
                "Public access is enabled. Any Discord user can talk to this bot in allowed channels."
            }
            AllowAllAction::Status => {
                "Public access is disabled. Only the owner and authorized users can talk to this bot."
            }
            AllowAllAction::On | AllowAllAction::Off => {
                let enabled = action == AllowAllAction::On;
                settings.allow_all_users = enabled;
                save_bot_settings(&ctx.data().token, &settings);
                if enabled {
                    "Public access enabled. Any Discord user can talk to this bot in allowed channels."
                } else {
                    "Public access disabled. Only the owner and authorized users can talk to this bot."
                }
            }
        }
    };

    ctx.say(format!("{response}\n\n{}", build_allowall_policy_note()))
        .await?;
    if action != AllowAllAction::Status {
        log_info_event!(
            "discord_public_access_changed",
            channel_id = ctx.channel_id().get(),
            status = if action == AllowAllAction::On {
                "enabled"
            } else {
                "disabled"
            },
        );
    }
    Ok(())
}

/// Note appended to `/allowall` replies: what public access really grants, and
/// that it covers only this bot on this node.
fn build_allowall_policy_note() -> &'static str {
    "**Note:** this setting applies only to this bot on this node. Agents run with \
     permission prompts bypassed, so anyone who can chat here can make the agent run \
     commands on this host. `/allowall` itself stays owner-only."
}

#[cfg(all(test, unix))]
mod allowall_tests;

#[cfg(test)]
mod session_override_cluster_characterization_tests {
    //! #3038 S2-0 — characterization tests for the session-override cluster
    //! (cluster D) function surface. These fix the observable behaviour of the
    //! fast-mode / Codex-goals / model-override toggle round-trips BEFORE the
    //! field group is extracted into `SessionOverrideState`, so the same tests
    //! passing unchanged after the move is the behaviour-equivalence proof (the
    //! S1 `QueuedPlaceholderState` characterization standard). The tests call
    //! only the function surface of this module — never the `SharedData` fields
    //! directly — so the extraction's field-path rewiring cannot require a test
    //! edit. (`model_picker_pending` and `session_reset_pending` have no pure
    //! read surface — their rewiring is enforced by the compiler alone; the
    //! bootstrap literal behaviour is pinned separately by
    //! `bootstrap_session_reset_pending_excludes_restored_model_overrides`.)

    use super::*;

    use crate::services::discord::make_shared_data_for_tests;

    const AGENTDESK_ROOT_DIR_ENV: &str = "AGENTDESK_ROOT_DIR";
    const AGENTDESK_CONFIG_ENV: &str = "AGENTDESK_CONFIG";

    pub(super) struct EnvGuard;

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            unsafe {
                std::env::remove_var(AGENTDESK_ROOT_DIR_ENV);
                std::env::remove_var(AGENTDESK_CONFIG_ENV);
            }
        }
    }

    /// Pin BOTH the runtime root AND the explicit config path to the temp dir
    /// so `save_bot_settings` (called by the `update_channel_*` helpers under
    /// test) can never fall back to the developer's real cwd/home config.
    pub(super) fn isolate_persistence(tmp: &std::path::Path) -> EnvGuard {
        unsafe {
            std::env::set_var(AGENTDESK_ROOT_DIR_ENV, tmp.to_str().unwrap());
            std::env::set_var(
                AGENTDESK_CONFIG_ENV,
                tmp.join("config").join("agentdesk.yaml"),
            );
        }
        EnvGuard
    }

    // Current-thread runtime so the async update helpers can be driven from a
    // synchronous `#[test]`: the `test_support` env-lock guard is then never
    // held across an `.await`, keeping the await_holding_lock ratchet flat
    // (same pattern as the S1/S0 queued-placeholder characterization tests).
    pub(super) fn test_rt() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
    }

    #[test]
    fn fast_mode_toggle_round_trip_marks_and_clears_provider_reset() {
        // #3167 B3: serialize process-global env mutation via the single
        // crate-wide `test_support` lock (no local per-module Mutex).
        let _lock = crate::services::turn_orchestrator::test_support::lock_test_env();
        let tmp = tempfile::tempdir().unwrap();
        let _env_guard = isolate_persistence(tmp.path());

        let shared = make_shared_data_for_tests();
        let channel_id = serenity::ChannelId::new(3_038_400);
        let provider = ProviderKind::Codex;
        let rt = test_rt();

        assert!(!channel_fast_mode_enabled(&shared, channel_id));
        assert!(!any_fast_mode_reset_pending(&shared, channel_id));

        // Enabling reports a change, flips the enablement set, and marks the
        // provider-scoped session reset as pending.
        assert!(
            rt.block_on(update_channel_fast_mode(
                &shared,
                "test-token",
                channel_id,
                &provider,
                true,
                channel_id,
            ))
            .unwrap()
        );
        assert!(channel_fast_mode_enabled(&shared, channel_id));
        assert!(fast_mode_reset_pending_for_provider(
            &shared, channel_id, &provider
        ));
        assert!(any_fast_mode_reset_pending(&shared, channel_id));

        // Re-enabling is a no-op: no change is reported.
        assert!(
            !rt.block_on(update_channel_fast_mode(
                &shared,
                "test-token",
                channel_id,
                &provider,
                true,
                channel_id,
            ))
            .unwrap()
        );

        // The provider-scoped clear reports the pending entry exactly once.
        assert!(clear_fast_mode_reset_pending_for_provider(
            &shared, channel_id, &provider
        ));
        assert!(!clear_fast_mode_reset_pending_for_provider(
            &shared, channel_id, &provider
        ));
        assert!(!any_fast_mode_reset_pending(&shared, channel_id));

        // Disabling re-marks the provider reset; the channel-wide clear
        // sweeps it (and is a no-op the second time).
        assert!(
            rt.block_on(update_channel_fast_mode(
                &shared,
                "test-token",
                channel_id,
                &provider,
                false,
                channel_id,
            ))
            .unwrap()
        );
        assert!(!channel_fast_mode_enabled(&shared, channel_id));
        assert!(any_fast_mode_reset_pending(&shared, channel_id));
        assert!(clear_fast_mode_reset_pending_for_channel(
            &shared, channel_id
        ));
        assert!(!clear_fast_mode_reset_pending_for_channel(
            &shared, channel_id
        ));
        assert!(!any_fast_mode_reset_pending(&shared, channel_id));
    }

    #[test]
    fn fast_mode_reset_entries_stay_provider_scoped() {
        let _lock = crate::services::turn_orchestrator::test_support::lock_test_env();
        let tmp = tempfile::tempdir().unwrap();
        let _env_guard = isolate_persistence(tmp.path());

        let shared = make_shared_data_for_tests();
        let channel_id = serenity::ChannelId::new(3_038_410);
        let rt = test_rt();

        assert!(
            rt.block_on(update_channel_fast_mode(
                &shared,
                "test-token",
                channel_id,
                &ProviderKind::Codex,
                true,
                channel_id,
            ))
            .unwrap()
        );

        // The pending entry is keyed `provider:channel`, so the other
        // provider neither observes nor clears it...
        assert!(!fast_mode_reset_pending_for_provider(
            &shared,
            channel_id,
            &ProviderKind::Claude
        ));
        assert!(!clear_fast_mode_reset_pending_for_provider(
            &shared,
            channel_id,
            &ProviderKind::Claude
        ));
        // ...while the channel-wide probe still sees it for the channel.
        assert!(any_fast_mode_reset_pending(&shared, channel_id));
        assert!(fast_mode_reset_pending_for_provider(
            &shared,
            channel_id,
            &ProviderKind::Codex
        ));
    }

    #[test]
    fn codex_goals_toggle_round_trip_marks_and_clears_reset() {
        let _lock = crate::services::turn_orchestrator::test_support::lock_test_env();
        let tmp = tempfile::tempdir().unwrap();
        let _env_guard = isolate_persistence(tmp.path());

        let shared = make_shared_data_for_tests();
        let channel_id = serenity::ChannelId::new(3_038_420);
        let rt = test_rt();

        assert!(!channel_codex_goals_enabled(&shared, channel_id));
        assert!(
            rt.block_on(update_channel_codex_goals(
                &shared,
                "test-token",
                channel_id,
                true,
            ))
            .unwrap()
        );
        assert!(channel_codex_goals_enabled(&shared, channel_id));

        // Re-enabling is a no-op.
        assert!(
            !rt.block_on(update_channel_codex_goals(
                &shared,
                "test-token",
                channel_id,
                true,
            ))
            .unwrap()
        );

        // The toggle marked the goals session reset pending exactly once.
        assert!(clear_codex_goals_reset_pending_for_channel(
            &shared, channel_id
        ));
        assert!(!clear_codex_goals_reset_pending_for_channel(
            &shared, channel_id
        ));

        // Disabling re-marks it.
        assert!(
            rt.block_on(update_channel_codex_goals(
                &shared,
                "test-token",
                channel_id,
                false,
            ))
            .unwrap()
        );
        assert!(!channel_codex_goals_enabled(&shared, channel_id));
        assert!(clear_codex_goals_reset_pending_for_channel(
            &shared, channel_id
        ));
    }

    #[test]
    fn model_override_round_trip_observed_via_would_change_probe() {
        let _hosting = crate::services::provider_hosting::TEST_CONFIG_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _lock = crate::services::turn_orchestrator::test_support::lock_test_env();
        let tmp = tempfile::tempdir().unwrap();
        let _env_guard = isolate_persistence(tmp.path());
        // A headless Claude channel, whose model change resets the session; a local TUI relaunches.
        let mut config = crate::config::Config::default();
        let headless = crate::config::ProviderConfig {
            tui_hosting: Some(false),
            ..Default::default()
        };
        config.providers.insert("claude".into(), headless);
        struct Restore;
        impl Drop for Restore {
            fn drop(&mut self) {
                let default = crate::config::Config::default();
                crate::services::provider_hosting::install_provider_hosting_config(&default);
            }
        }
        let _restore = Restore;
        crate::services::provider_hosting::install_provider_hosting_config(&config);

        let shared = make_shared_data_for_tests();
        let channel_id = serenity::ChannelId::new(3_038_430);
        let provider = ProviderKind::Claude;
        let rt = test_rt();

        assert!(would_channel_model_override_change(
            &shared,
            channel_id,
            Some("model-a")
        ));
        assert!(
            rt.block_on(update_channel_model_override(
                &shared,
                "test-token",
                channel_id,
                &provider,
                Some("model-a".to_string()),
            ))
            .unwrap()
        );
        assert!(
            shared
                .overrides
                .model_session_reset_pending
                .contains(&channel_id)
        );
        assert!(shared.overrides.session_reset_pending.contains(&channel_id));
        assert!(!would_channel_model_override_change(
            &shared,
            channel_id,
            Some("model-a")
        ));
        assert!(would_channel_model_override_change(
            &shared, channel_id, None
        ));

        // Setting the same override again is a no-op.
        assert!(
            !rt.block_on(update_channel_model_override(
                &shared,
                "test-token",
                channel_id,
                &provider,
                Some("model-a".to_string()),
            ))
            .unwrap()
        );

        // Clearing restores the no-override state.
        assert!(
            rt.block_on(update_channel_model_override(
                &shared,
                "test-token",
                channel_id,
                &provider,
                None,
            ))
            .unwrap()
        );
        assert!(
            !shared
                .overrides
                .model_session_reset_pending
                .contains(&channel_id)
        );
        assert!(!shared.overrides.session_reset_pending.contains(&channel_id));
        assert!(!would_channel_model_override_change(
            &shared, channel_id, None
        ));
    }
    #[test]
    fn truthful_failed_save_does_not_publish_model_or_reset() {
        let _lock = crate::services::turn_orchestrator::test_support::lock_test_env();
        let tmp = tempfile::tempdir().unwrap();
        let _env = isolate_persistence(tmp.path());
        std::fs::create_dir_all(tmp.path().join("config/bot_settings.json")).unwrap();
        let shared = make_shared_data_for_tests();
        let channel = serenity::ChannelId::new(6486);
        let result = test_rt().block_on(update_channel_model_override(
            &shared,
            "test-token",
            channel,
            &ProviderKind::Codex,
            Some("model-a".into()),
        ));
        assert!(result.is_err());
        assert!(!shared.overrides.model_overrides.contains_key(&channel));
        assert!(!shared.overrides.session_reset_pending.contains(&channel));
    }

    #[test]
    fn truthful_foreign_owner_cannot_publish_a_setting() {
        let _lock = crate::services::turn_orchestrator::test_support::lock_test_env();
        let tmp = tempfile::tempdir().unwrap();
        let _env = isolate_persistence(tmp.path());
        let _hosts =
            crate::config::session_hosts::force_for_test(Some("local"), &[(6487, "remote")]);
        let shared = make_shared_data_for_tests();
        let channel = serenity::ChannelId::new(6487);
        let result = test_rt().block_on(update_channel_codex_goals(
            &shared,
            "test-token",
            channel,
            true,
        ));
        assert!(result.is_err());
        assert!(!shared.overrides.codex_goals_channels.contains(&channel));
        assert!(!shared.overrides.session_reset_pending.contains(&channel));
    }

    #[test]
    fn truthful_claude_tui_fast_does_not_reset_or_publish() {
        let _lock = crate::services::turn_orchestrator::test_support::lock_test_env();
        let tmp = tempfile::tempdir().unwrap();
        let _env = isolate_persistence(tmp.path());
        let _hosts = provider_settings::local_tui_for_test(6488);
        let shared = make_shared_data_for_tests();
        let channel = serenity::ChannelId::new(6488);
        let result = test_rt().block_on(update_channel_fast_mode(
            &shared,
            "test-token",
            channel,
            &ProviderKind::Claude,
            true,
            channel,
        ));
        assert!(result.is_err());
        assert!(!shared.overrides.fast_mode_channels.contains(&channel));
        assert!(!shared.overrides.session_reset_pending.contains(&channel));
    }
}
