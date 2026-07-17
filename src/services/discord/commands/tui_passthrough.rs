use crate::services::claude_tui::input::SelectorNavigation;
use crate::services::provider::ProviderKind;

use super::super::{Context, Error, check_auth};
use super::config::{effective_provider_for_channel, fallback_channel_name_for_feature_toggle};

#[derive(Debug, Clone, Copy, poise::ChoiceParameter)]
enum EffortLevel {
    #[name = "low"]
    Low,
    #[name = "medium"]
    Medium,
    #[name = "high"]
    High,
    #[name = "xhigh"]
    Xhigh,
    #[name = "max"]
    Max,
    #[name = "ultracode"]
    Ultracode,
}

/// The *physical* stops Claude Code's `/effort` horizontal slider presents, in
/// left-to-right order. This MUST include `ultracode` even though passthrough
/// never targets it: the live slider still has `ultracode` as its rightmost
/// stop, so the deterministic Left-home move (`total_items - 1` Left presses)
/// must account for the full physical width. If the slider is currently parked
/// on `ultracode` and we undercounted, the home move would stop short and apply
/// the wrong level (e.g. `/effort max` confirming `ultracode`).
///
/// `ultracode` is guarded off the passthrough path elsewhere (see
/// `ultracode_notice` / `provider_preflight_notice`), so it is never a
/// navigable *target*; `selector_index` simply maps each targetable level to
/// its physical stop position within this list.
const EFFORT_SLIDER_STOPS: [EffortLevel; 6] = [
    EffortLevel::Low,
    EffortLevel::Medium,
    EffortLevel::High,
    EffortLevel::Xhigh,
    EffortLevel::Max,
    EffortLevel::Ultracode,
];

impl EffortLevel {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Xhigh => "xhigh",
            Self::Max => "max",
            Self::Ultracode => "ultracode",
        }
    }

    /// 0-based physical stop index of this level on the `/effort` slider.
    fn selector_index(self) -> usize {
        match self {
            Self::Low => 0,
            Self::Medium => 1,
            Self::High => 2,
            Self::Xhigh => 3,
            Self::Max => 4,
            Self::Ultracode => 5,
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum ClaudeSlashPassthrough {
    Effort(EffortLevel),
    Compact,
    Cost,
    Context,
}

impl ClaudeSlashPassthrough {
    const fn slash_name(self) -> &'static str {
        match self {
            Self::Effort(_) => "/effort",
            Self::Compact => "/compact",
            Self::Cost => "/cost",
            Self::Context => "/context",
        }
    }

    fn prompt(self) -> String {
        match self {
            Self::Effort(level) => format!("/effort {}", level.as_str()),
            Self::Compact => "/compact".to_string(),
            Self::Cost => "/cost".to_string(),
            Self::Context => "/context".to_string(),
        }
    }

    /// `/effort` is an interactive horizontal slider, not an inline-argument
    /// command: it must be driven with left/right arrow navigation + Enter
    /// rather than submitted as a single-line prompt. Returns the navigation
    /// plan for the requested level, or `None` for commands that take the
    /// plain-prompt path.
    fn selector_navigation(self) -> Option<SelectorNavigation> {
        match self {
            // `ultracode` is guarded off the passthrough path before this point
            // (see `provider_preflight_notice`), so it never reaches the send
            // step. The `total_items` here is the full physical slider width
            // (including the ultracode stop) so the Left-home move is
            // deterministic regardless of the slider's current position.
            Self::Effort(level) => Some(SelectorNavigation {
                slash_command: "/effort",
                total_items: EFFORT_SLIDER_STOPS.len(),
                target_index: level.selector_index(),
            }),
            Self::Compact | Self::Cost | Self::Context => None,
        }
    }
}

/// #3305 SSOT: the slash commands AgentDesk passes through to the Claude TUI that
/// complete LOCALLY (they render in the TUI but never start a model turn). Their
/// transcript `<command-*>` echo otherwise tricks the idle relay into minting a
/// synthetic external turn (⏳ anchor + inflight) that never finalizes — stranding
/// the hourglass and FOREIGN-ABORTing the next injection (#3302 marker pollution).
///
/// The relay (`tui_prompt_relay`) consults this list to skip lifecycle (NOT the
/// guidance note) for exactly these kinds, an ALLOW-list so any unknown / future
/// command keeps full lifecycle by default (fail-safe). ONLY add a command here if
/// it is genuinely local-completing: a pass-through that DOES start a model turn
/// (e.g. anything `/loop`-shaped) MUST stay off this list or its turn loses its
/// `⏳`→`✅` lifecycle. The `local_only_whitelist_matches_passthrough_command_set`
/// anti-drift test pins this against the [`ClaudeSlashPassthrough`] variant set.
pub(in crate::services::discord) const LOCAL_ONLY_SLASH_COMMANDS: [&str; 4] =
    crate::services::tui_prompt_control::LOCAL_ONLY_SLASH_COMMANDS;

/// #3500: local-completing slash commands that are NOT AgentDesk pass-throughs.
/// These are Claude-NATIVE commands (e.g. `/model`) the idle relay only OBSERVES
/// echoed in the pane transcript — they change local state and start NO model
/// turn, so like [`LOCAL_ONLY_SLASH_COMMANDS`] they must SKIP the turn lifecycle.
/// Otherwise the post-#3178 "SlashCommandControl == full active turn" rule mints a
/// synthetic inflight that never finalizes (no model turn to complete it),
/// stranding the mailbox so the next real message queues — the #3500 bug. They are
/// intentionally absent from the [`ClaudeSlashPassthrough`] variant set (no
/// pass-through handler), so they live here rather than in `LOCAL_ONLY_SLASH_COMMANDS`
/// (which the `local_only_whitelist_matches_passthrough_command_set` anti-drift test
/// pins to that variant set).
pub(in crate::services::discord) const OBSERVATION_ONLY_LOCAL_SLASH_COMMANDS: [&str; 1] =
    crate::services::tui_prompt_control::OBSERVATION_ONLY_LOCAL_SLASH_COMMANDS;

/// #3305/#3500: whether `kind` (a canonical `slash_command_control_kind`, e.g.
/// `/compact`) is a local-completing command that SKIPS the turn lifecycle — either
/// an AgentDesk pass-through ([`LOCAL_ONLY_SLASH_COMMANDS`]) or a Claude-native
/// observation-only command ([`OBSERVATION_ONLY_LOCAL_SLASH_COMMANDS`]).
#[cfg(test)]
pub(in crate::services::discord) fn is_local_only_slash_command_kind(kind: &str) -> bool {
    crate::services::tui_prompt_control::is_local_only_slash_command_kind(kind)
}

fn ultracode_notice() -> &'static str {
    "`/effort ultracode`는 live Claude 세션에 안전하게 passthrough하지 않습니다. \
현재 범위에서는 세션 재시작/별도 설정 연동 없이 보장되는 경로만 열었고, 안정 경로는 \
`/effort max`까지입니다."
}

fn codex_effort_notice() -> &'static str {
    "`/effort`는 Claude live TUI passthrough로만 연결됩니다. Codex의 reasoning effort는 \
wrapper/env 시작 옵션 경로는 있지만 AgentDesk에 채널 단위 설정면이 아직 없어 여기서는 \
즉시 토글하지 않습니다."
}

fn unsupported_notice(provider: &ProviderKind, command: ClaudeSlashPassthrough) -> String {
    if matches!(command, ClaudeSlashPassthrough::Effort(_))
        && matches!(provider, ProviderKind::Codex)
    {
        return codex_effort_notice().to_string();
    }
    format!(
        "{} is only available for live Claude TUI channels. Current provider: {}.",
        command.slash_name(),
        provider.display_name(),
    )
}

fn live_session_required_notice(command: ClaudeSlashPassthrough) -> String {
    format!(
        "{} needs a live Claude tmux session for this channel. Start or resume the Claude session first.",
        command.slash_name(),
    )
}

fn provider_preflight_notice(
    provider: &ProviderKind,
    command: ClaudeSlashPassthrough,
) -> Option<String> {
    if !matches!(provider, ProviderKind::Claude) {
        return Some(unsupported_notice(provider, command));
    }
    if let ClaudeSlashPassthrough::Effort(EffortLevel::Ultracode) = command {
        return Some(ultracode_notice().to_string());
    }
    None
}

async fn resolve_effective_provider_and_tmux_name(
    ctx: Context<'_>,
) -> (ProviderKind, Option<String>) {
    let channel_id = ctx.channel_id();
    let channel_name_hint = fallback_channel_name_for_feature_toggle(ctx, channel_id).await;
    let effective_provider = effective_provider_for_channel(
        &ctx.data().shared,
        channel_id,
        &ctx.data().provider,
        channel_name_hint.as_deref(),
    )
    .await;
    let session_channel_name = {
        let data = ctx.data().shared.core.lock().await;
        data.sessions
            .get(&channel_id)
            .and_then(|session| session.channel_name.clone())
    };
    let tmux_name = session_channel_name
        .as_deref()
        .or(channel_name_hint.as_deref())
        .map(|channel_name| effective_provider.build_tmux_session_name(channel_name));
    (effective_provider, tmux_name)
}

async fn run_claude_passthrough(
    ctx: Context<'_>,
    command: ClaudeSlashPassthrough,
) -> Result<(), Error> {
    let user_id = ctx.author().id;
    let user_name = &ctx.author().name;
    if !check_auth(user_id, user_name, &ctx.data().shared, &ctx.data().token).await {
        return Ok(());
    }

    let ts = chrono::Local::now().format("%H:%M:%S");
    tracing::info!("  [{ts}] ◀ [{user_name}] {}", command.prompt());

    let (effective_provider, tmux_name) = resolve_effective_provider_and_tmux_name(ctx).await;
    if let Some(notice) = provider_preflight_notice(&effective_provider, command) {
        ctx.say(notice).await?;
        return Ok(());
    }

    let Some(tmux_name) = tmux_name else {
        ctx.say(live_session_required_notice(command)).await?;
        return Ok(());
    };
    if !crate::services::tmux_diagnostics::tmux_session_has_live_pane(&tmux_name) {
        ctx.say(live_session_required_notice(command)).await?;
        return Ok(());
    }

    ctx.defer().await?;

    let tmux_name_for_send = tmux_name.clone();
    let send_result = match command.selector_navigation() {
        Some(nav) => tokio::task::spawn_blocking(move || {
            crate::services::claude_tui::input::send_selector_followup(
                &tmux_name_for_send,
                nav,
                None,
            )
        })
        .await
        .unwrap_or_else(|error| Err(format!("join error: {error}"))),
        None => {
            let prompt = command.prompt();
            tokio::task::spawn_blocking(move || {
                crate::services::claude_tui::input::send_followup_prompt(
                    &tmux_name_for_send,
                    &prompt,
                    None,
                )
            })
            .await
            .unwrap_or_else(|error| Err(format!("join error: {error}")))
        }
    };

    match send_result {
        Ok(()) => match command {
            ClaudeSlashPassthrough::Effort(level) => {
                ctx.say(format!(
                    "`/effort {}`를 live Claude session `{}`에 적용했습니다. selector가 닫힌 것을 확인했습니다.",
                    level.as_str(),
                    tmux_name,
                ))
                .await?;
            }
            _ => {
                ctx.say(format!(
                    "{}를 live Claude session `{}`에 전달했습니다. Claude 응답은 채널에 이어서 올라옵니다.",
                    command.slash_name(),
                    tmux_name,
                ))
                .await?;
            }
        },
        Err(error) if crate::services::claude_tui::input::is_prompt_ready_timeout_error(&error) => {
            ctx.say(format!(
                "{} 전달 대기 중 timeout이 났습니다. Claude turn이 아직 바쁘거나 prompt ready 상태가 아닙니다.",
                command.slash_name(),
            ))
            .await?;
        }
        Err(error)
            if crate::services::claude_tui::input::is_prompt_ready_cancelled_error(&error) =>
        {
            ctx.say(format!(
                "{} 전달이 취소됐습니다. 다른 stop/restart/reset이 먼저 들어온 상태입니다.",
                command.slash_name(),
            ))
            .await?;
        }
        Err(error) => {
            ctx.say(format!(
                "{} passthrough failed for `{}`: {}",
                command.slash_name(),
                tmux_name,
                error,
            ))
            .await?;
        }
    }

    Ok(())
}

/// /effort <level> — pass through Claude native effort control to the live TUI.
#[poise::command(slash_command, rename = "effort")]
pub(in crate::services::discord) async fn cmd_effort(
    ctx: Context<'_>,
    #[description = "Level: low / medium / high / xhigh / max / ultracode"] level: EffortLevel,
) -> Result<(), Error> {
    run_claude_passthrough(ctx, ClaudeSlashPassthrough::Effort(level)).await
}

/// /compact — pass through Claude native /compact to the live TUI.
#[poise::command(slash_command, rename = "compact")]
pub(in crate::services::discord) async fn cmd_compact(ctx: Context<'_>) -> Result<(), Error> {
    run_claude_passthrough(ctx, ClaudeSlashPassthrough::Compact).await
}

/// /cost — pass through Claude native /cost to the live TUI.
#[poise::command(slash_command, rename = "cost")]
pub(in crate::services::discord) async fn cmd_cost(ctx: Context<'_>) -> Result<(), Error> {
    run_claude_passthrough(ctx, ClaudeSlashPassthrough::Cost).await
}

/// /context — pass through Claude native /context to the live TUI.
#[poise::command(slash_command, rename = "context")]
pub(in crate::services::discord) async fn cmd_context(ctx: Context<'_>) -> Result<(), Error> {
    run_claude_passthrough(ctx, ClaudeSlashPassthrough::Context).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    /// Every `ClaudeSlashPassthrough` variant in this file is a LOCAL-completing
    /// command (none of them start a model turn), so the relay's lifecycle-skip
    /// allow-list MUST equal the variant set exactly. This anti-drift guard fails
    /// the build whenever a new pass-through is added without a deliberate decision
    /// about its lifecycle: add it to `LOCAL_ONLY_SLASH_COMMANDS` only if it is
    /// genuinely local-completing, otherwise this test stays RED until the author
    /// confirms the lifecycle classification (see the const doc on `/loop`-shaped
    /// commands, #3305).
    #[test]
    fn local_only_whitelist_matches_passthrough_command_set() {
        // The full ClaudeSlashPassthrough surface. `Effort` carries a level but its
        // slash_name() is level-independent, so a single representative suffices.
        let variants = [
            ClaudeSlashPassthrough::Effort(EffortLevel::High),
            ClaudeSlashPassthrough::Compact,
            ClaudeSlashPassthrough::Cost,
            ClaudeSlashPassthrough::Context,
        ];
        let variant_names: BTreeSet<&str> = variants.iter().map(|cmd| cmd.slash_name()).collect();
        let whitelist: BTreeSet<&str> = LOCAL_ONLY_SLASH_COMMANDS.iter().copied().collect();
        assert_eq!(
            variant_names, whitelist,
            "LOCAL_ONLY_SLASH_COMMANDS must equal the ClaudeSlashPassthrough variant set; \
             a new pass-through needs an explicit local-vs-model-turn lifecycle decision",
        );
        // Sanity: the kind predicate agrees with the const for both members and
        // a non-member (e.g. /loop is a model-turn command and must be excluded).
        for name in &whitelist {
            assert!(is_local_only_slash_command_kind(name));
        }
        assert!(!is_local_only_slash_command_kind("/loop"));
        // #3500: `/model` is local-only (observation-only, Claude-native) but is
        // intentionally NOT a pass-through variant, so it must stay OUT of
        // LOCAL_ONLY_SLASH_COMMANDS (the variant-pinned set) — see the dedicated
        // test below for its local-only behavior.
        assert!(!LOCAL_ONLY_SLASH_COMMANDS.contains(&"/model"));
    }

    /// #3500: `/model` (Claude-native model change, no model turn) must be treated
    /// as local-only so the idle relay SKIPS the turn lifecycle — otherwise it
    /// strands a synthetic inflight that queues the next real message.
    #[test]
    fn model_command_is_observation_only_local() {
        assert!(is_local_only_slash_command_kind("/model"));
        assert!(OBSERVATION_ONLY_LOCAL_SLASH_COMMANDS.contains(&"/model"));
        // Intentionally not a pass-through variant (no handler) → stays out of the
        // variant-pinned LOCAL_ONLY_SLASH_COMMANDS set.
        assert!(!LOCAL_ONLY_SLASH_COMMANDS.contains(&"/model"));
    }
}
