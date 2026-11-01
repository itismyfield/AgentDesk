//! A pending launch-setting change applied to a live Claude TUI by restarting it on the session it
//! already runs: the next turn's launch resumes that session with the new settings.

use std::sync::LazyLock;

use super::super::tmux_recreate::{Relaunch, relaunch_channel_tmux_keeping_session};
use super::*;

/// The channel's pane as the pending change finds it at its turn's start.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum PaneState {
    None,
    ClaudeTuiLive,
    OtherLive,
    /// The probe failed; nothing is decided this turn.
    Unknown,
}

/// What a relaunch attempt left for the pending change.
#[derive(Debug, Eq, PartialEq)]
pub(super) enum Outcome {
    Applied(String),
    /// Nothing ended this turn; the change waits for the next one.
    Kept,
    /// Two kills failed; the change is dropped and applies at the next launch.
    Released,
    /// No session can be resumed, so the provider reset applies instead.
    Fallback,
    /// The launch's account cannot be confirmed; the session is kept and nothing ends this turn.
    Undetermined,
    Refused(String),
}

/// Failed kills per channel, counted for the fast value they tried to apply; a new value starts
/// the count again, and a spent count stays so a drift check does not retry the same value.
static KILL_FAILURES: LazyLock<dashmap::DashMap<serenity::ChannelId, (u8, Option<bool>)>> =
    LazyLock::new(dashmap::DashMap::new);
static DEFER_NOTICED: LazyLock<dashmap::DashSet<serenity::ChannelId>> =
    LazyLock::new(dashmap::DashSet::new);

const RELAUNCH_KILL_LIMIT: u8 = 2;

struct Target {
    tmux: String,
    channel_name: String,
    session: Option<String>,
    working_dir: Option<String>,
}

async fn target(
    shared: &SharedData,
    provider: &ProviderKind,
    channel_id: serenity::ChannelId,
) -> Option<Target> {
    let data = shared.core.lock().await;
    let session = data.sessions.get(&channel_id)?;
    let channel_name = session.channel_name.clone()?;
    let tmux = provider.build_tmux_session_name(&channel_name);
    Some(Target {
        tmux,
        channel_name,
        session: session.session_id.clone(),
        working_dir: session.current_path.clone(),
    })
}

pub(super) async fn pane_state(
    shared: &SharedData,
    provider: &ProviderKind,
    channel_id: serenity::ChannelId,
) -> PaneState {
    match target(shared, provider, channel_id).await {
        Some(target) => pane_state_of(&target.tmux),
        None => PaneState::None,
    }
}

#[cfg(unix)]
fn pane_state_of(tmux: &str) -> PaneState {
    use crate::services::agent_protocol::RuntimeHandoffKind;
    use crate::services::platform::tmux::PaneLiveness;
    match crate::services::tmux_diagnostics::tmux_session_pane_liveness(tmux) {
        PaneLiveness::Live => {}
        PaneLiveness::DeadOrAbsent => return PaneState::None,
        PaneLiveness::ProbeError => return PaneState::Unknown,
    }
    let kind = crate::services::tui_prompt_dedupe::runtime_binding_for_tmux_session(tmux)
        .map(|binding| binding.runtime_kind)
        .or_else(|| crate::services::tmux_common::resolve_tmux_runtime_kind_marker(tmux));
    match kind {
        Some(RuntimeHandoffKind::ClaudeTui) => PaneState::ClaudeTuiLive,
        _ => PaneState::OtherLive,
    }
}

#[cfg(not(unix))]
fn pane_state_of(_: &str) -> PaneState {
    PaneState::None
}

/// Whether the launch that follows a kill resumes the session.
#[derive(Debug, Eq, PartialEq)]
enum Resume {
    Yes,
    /// The account the launch takes has no transcript of the session.
    No,
    /// The launch's account cannot be confirmed from here.
    Undetermined,
}

/// The agent a turn on the channel runs as: the channel's own binding, else its thread parent's.
fn launch_agent(channel_id: serenity::ChannelId, channel_name: &str) -> Option<String> {
    let binding = crate::services::discord::settings::resolve_role_binding;
    binding(channel_id, Some(channel_name))
        .or_else(|| SharedData::feature_settings_parent(channel_id).and_then(|p| binding(p, None)))
        .map(|binding| binding.role_id)
}

/// Judges the resume under the account the channel's launch is configured to take, as the pane's
/// own account marker and that account's transcripts show it.
#[cfg(unix)]
fn resumable(channel_id: serenity::ChannelId, target: &Target, session: &str, dir: &str) -> Resume {
    let agent = launch_agent(channel_id, &target.channel_name);
    let overlay = crate::services::discord::org_schema::configured_launch_overlay(
        ProviderKind::Claude,
        channel_id.get(),
        agent.as_deref(),
    );
    let Ok(Some(overlay)) = overlay else {
        return Resume::Undetermined;
    };
    let tmux = target.tmux.as_str();
    if !crate::services::tmux_common::tmux_session_auth_profile_matches(tmux, &overlay.profile_id) {
        return Resume::Undetermined;
    }
    let resolved = crate::services::claude::resolve_claude_tui_session_for_launch(
        std::path::Path::new(dir),
        Some(session),
        overlay.home.as_deref(),
    );
    match resolved {
        Ok(resolution) if resolution.resume && resolution.session_id == session => Resume::Yes,
        Ok(_) => Resume::No,
        Err(_) => Resume::Undetermined,
    }
}

#[cfg(not(unix))]
fn resumable(_: serenity::ChannelId, _: &Target, _: &str, _: &str) -> Resume {
    Resume::Undetermined
}

/// The fast setting the live TUI was launched with, from the settings file its launch wrote; `None`
/// when that file does not name `session`, so it cannot be tied to the process running it.
fn launched_fast(tmux: &str, session: &str) -> Option<Option<bool>> {
    let ext = crate::services::tmux_common::CLAUDE_TUI_HOOK_SETTINGS_TEMP_EXT;
    let path = crate::services::tmux_common::session_temp_path(tmux, ext);
    let settings: serde_json::Value = serde_json::from_slice(&std::fs::read(path).ok()?).ok()?;
    let commands = settings["hooks"]
        .as_object()?
        .values()
        .filter_map(|v| v.as_array());
    let commands = commands
        .flatten()
        .filter_map(|group| group["hooks"].as_array())
        .flatten();
    let names_session = commands
        .filter_map(|hook| hook["command"].as_str())
        .any(|command| {
            let mut words = command
                .split_whitespace()
                .skip_while(|word| *word != "--session-id");
            words.nth(1) == Some(session)
        });
    names_session.then(|| settings["fastMode"].as_bool())
}

/// Whether the channel's live TUI was launched with a fast setting other than `desired`, as when a
/// session sharing the setting consumed its pending change first.
pub(super) async fn fast_drifted(
    shared: &SharedData,
    provider: &ProviderKind,
    channel_id: serenity::ChannelId,
    desired: Option<bool>,
) -> bool {
    let Some(desired) = desired else {
        return false;
    };
    let spent = KILL_FAILURES
        .get(&channel_id)
        .is_some_and(|entry| entry.0 >= RELAUNCH_KILL_LIMIT && entry.1 == Some(desired));
    let Some(Target {
        tmux,
        session: Some(session),
        ..
    }) = target(shared, provider, channel_id).await
    else {
        return false;
    };
    !spent && launched_fast(&tmux, &session).is_some_and(|fast| fast != Some(desired))
}

/// The one send of every relaunch notice: a status line, never a turn body.
pub(super) async fn say(http: &serenity::Http, channel_id: serenity::ChannelId, notice: String) {
    if let Err(error) = channel_id.say(http, notice).await {
        tracing::warn!(channel_id = channel_id.get(), %error, "relaunch notice failed");
    }
}

fn short(session: &str) -> &str {
    session.get(..8).unwrap_or(session)
}

/// Restarts the channel's live Claude TUI so its next launch resumes the same session with the
/// settings the channel now holds.
pub(super) async fn run(
    http: &Arc<serenity::Http>,
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: serenity::ChannelId,
    reset_source: &str,
    requested_fast: Option<bool>,
) -> Outcome {
    let Some(target) = target(shared, provider, channel_id).await else {
        return Outcome::Fallback;
    };
    let (Some(session), Some(working_dir)) = (target.session.clone(), target.working_dir.clone())
    else {
        return Outcome::Fallback;
    };
    match resumable(channel_id, &target, &session, &working_dir) {
        Resume::Yes => {}
        Resume::No => return Outcome::Fallback,
        Resume::Undetermined => return Outcome::Undetermined,
    }
    let tmux = target.tmux;
    match relaunch_channel_tmux_keeping_session(shared, provider, channel_id, &tmux, reset_source)
        .await
    {
        Relaunch::Relaunched => {
            // Only a record written after the old process ended is the new process's.
            let at = super::relaunch_confirm::now();
            forget_attempts(channel_id);
            let notice = format!(
                "설정 변경을 적용하려고 같은 세션 `{}`으로 TUI를 재기동합니다.",
                short(&session)
            );
            say(http, channel_id, notice).await;
            let confirm = super::relaunch_confirm::Confirm {
                session,
                requested_fast,
                at,
            };
            super::relaunch_confirm::spawn(http.clone(), shared.clone(), channel_id, confirm);
            Outcome::Applied(tmux)
        }
        Relaunch::Deferred => {
            if DEFER_NOTICED.insert(channel_id) {
                let notice = "TUI에서 진행 중인 턴이 있어 설정 적용용 재기동을 다음 턴으로 미룹니다. 이번 턴은 기존 설정으로 처리합니다.";
                say(http, channel_id, notice.to_string()).await;
            }
            Outcome::Kept
        }
        Relaunch::Refused(reason) => Outcome::Refused(reason),
        Relaunch::KillFailed => {
            let failures = {
                let mut entry = KILL_FAILURES
                    .entry(channel_id)
                    .or_insert((0, requested_fast));
                if entry.1 != requested_fast {
                    *entry = (0, requested_fast);
                }
                entry.0 += 1;
                entry.0
            };
            if failures >= RELAUNCH_KILL_LIMIT {
                DEFER_NOTICED.remove(&channel_id);
                let notice = "TUI 재기동이 2회 실패했습니다. 설정은 저장돼 있고 다음 세션 기동(/clear·재시작) 때 적용됩니다.";
                say(http, channel_id, notice.to_string()).await;
                Outcome::Released
            } else {
                let notice = "TUI를 재기동하지 못해 이번 턴은 기존 설정으로 처리합니다. 다음 턴에 한 번 더 시도합니다.";
                say(http, channel_id, notice.to_string()).await;
                Outcome::Kept
            }
        }
    }
}

pub(super) fn forget_attempts(channel_id: serenity::ChannelId) {
    KILL_FAILURES.remove(&channel_id);
    DEFER_NOTICED.remove(&channel_id);
}

/// Drops the channel's relaunch attempt count, deferral notice and pending confirmation, as a
/// `/clear` that ends the session leaves nothing for them to report on.
pub(super) fn forget(channel_id: serenity::ChannelId) {
    forget_attempts(channel_id);
    super::relaunch_confirm::abort(channel_id);
}

pub(super) const OTHER_LIVE_NOTICE: &str =
    "실행 중 세션이 TUI가 아니라 같은 세션 재기동을 할 수 없어 세션을 리셋했습니다(맥락 소실).";

pub(super) const FALLBACK_NOTICE: &str = "같은 세션을 이어갈 기록이 없어 새 세션으로 시작합니다(맥락 소실). 새 설정은 새 세션에 적용됩니다.";
