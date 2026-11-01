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
    Refused(String),
}

static KILL_FAILURES: LazyLock<dashmap::DashMap<serenity::ChannelId, u8>> =
    LazyLock::new(dashmap::DashMap::new);
static DEFER_NOTICED: LazyLock<dashmap::DashSet<serenity::ChannelId>> =
    LazyLock::new(dashmap::DashSet::new);

const RELAUNCH_KILL_LIMIT: u8 = 2;

struct Target {
    tmux: String,
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
    let tmux = provider.build_tmux_session_name(session.channel_name.as_deref()?);
    Some(Target {
        tmux,
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

/// Whether the launch that follows a kill would resume `session` rather than start a new one.
#[cfg(unix)]
fn resumable(tmux: &str, session: &str, working_dir: &str) -> bool {
    let overlay =
        crate::services::discord::org_schema::overlay_from_tmux_session(ProviderKind::Claude, tmux);
    let Ok(overlay) = overlay else {
        return false;
    };
    // A pane under another account would have its session dropped by the launch.
    if !crate::services::tmux_common::tmux_session_auth_profile_matches(tmux, &overlay.profile_id) {
        return false;
    }
    let dir = std::path::Path::new(working_dir);
    let resolved = crate::services::claude::resolve_claude_tui_session_for_launch(
        dir,
        Some(session),
        overlay.home.as_deref(),
    );
    resolved.is_ok_and(|resolution| resolution.resume && resolution.session_id == session)
}

#[cfg(not(unix))]
fn resumable(_: &str, _: &str, _: &str) -> bool {
    false
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
    let Some(Target {
        tmux,
        session: Some(session),
        working_dir: Some(working_dir),
    }) = target(shared, provider, channel_id).await
    else {
        return Outcome::Fallback;
    };
    if !resumable(&tmux, &session, &working_dir) {
        return Outcome::Fallback;
    }
    let at = super::relaunch_confirm::now();
    match relaunch_channel_tmux_keeping_session(shared, provider, channel_id, &tmux, reset_source)
        .await
    {
        Relaunch::Relaunched => {
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
                let mut count = KILL_FAILURES.entry(channel_id).or_insert(0);
                *count += 1;
                *count
            };
            if failures >= RELAUNCH_KILL_LIMIT {
                forget_attempts(channel_id);
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
