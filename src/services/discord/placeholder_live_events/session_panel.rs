use serde_json::Value;

use crate::services::provider::ProviderKind;

use super::common::{
    SESSION_PANEL_LINE_MAX_CHARS, first_json_bool, first_json_string, first_json_usize,
    truncate_chars,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SessionPanelKind {
    Fresh,
    Resumed,
    Fallback,
}

impl SessionPanelKind {
    const fn label(self) -> &'static str {
        match self {
            Self::Fresh => "🆕 새 세션 시작",
            Self::Resumed => "기존 세션 복원",
            Self::Fallback => "Lifecycle fallback",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum TmuxPanelState {
    Kept,
    New,
}

impl TmuxPanelState {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Kept => "kept",
            Self::New => "new",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct SessionPanelSnapshot {
    kind: SessionPanelKind,
    provider_session_id: Option<String>,
    tmux: Option<TmuxPanelState>,
    recovery_message_count: Option<usize>,
    /// Per-session instance marker: the turn id (`discord:<channel>:<user_msg_id>`)
    /// whose lifecycle row produced this snapshot. Two DIFFERENT fresh sessions
    /// arrive on DIFFERENT turns (a new `user_msg_id`), so their turn ids differ;
    /// every status tick of the SAME fresh session re-loads the SAME turn's
    /// lifecycle row, so the turn id is stable across ticks. This lets a
    /// fresh-session boundary be detected even when `provider_session_id` is
    /// `None` (the common `/clear`, idle-timeout, turn-cap, goal-fresh,
    /// no-cached-provider-session paths) WITHOUT re-resetting on every tick of an
    /// ongoing fresh session (#3087).
    turn_id: Option<String>,
}

impl SessionPanelSnapshot {
    pub(super) fn from_lifecycle_event(
        turn_id: Option<&str>,
        kind: &str,
        details: &Value,
    ) -> Option<Self> {
        if !details.as_object().is_some_and(|object| !object.is_empty()) {
            return None;
        }

        let kind = session_panel_kind(kind, details)?;
        let provider_session_id = first_json_string(
            details,
            &[
                "provider_session_id",
                "providerSessionId",
                "raw_provider_session_id",
                "rawProviderSessionId",
                "session_id",
                "sessionId",
                "claude_session_id",
                "claudeSessionId",
            ],
        )
        .map(str::to_string);
        let tmux = parse_tmux_panel_state(details);
        let recovery_message_count = parse_recovery_message_count(details);
        let turn_id = turn_id
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string);

        Some(Self {
            kind,
            provider_session_id,
            tmux,
            recovery_message_count,
            turn_id,
        })
    }

    /// The provider-issued session id carried by this snapshot, normalized to
    /// `None` when absent or blank. Used to detect a true session boundary
    /// (provider session delta) so the status panel can reset its accumulated
    /// subagents/tasks without reacting to unrelated field churn.
    pub(super) fn provider_session_id(&self) -> Option<&str> {
        self.provider_session_id
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
    }

    /// Whether this snapshot describes a freshly-started session
    /// (`🆕 새 세션 시작`). A genuinely fresh session legitimately carries
    /// `provider_session_id == None`, so the fresh-boundary reset must key off
    /// the per-instance `turn_id` rather than the (absent) provider id.
    pub(super) const fn is_fresh(&self) -> bool {
        matches!(self.kind, SessionPanelKind::Fresh)
    }

    /// The per-session instance marker (turn id) used to distinguish two
    /// DIFFERENT fresh sessions that both lack a provider session id. Normalized
    /// to `None` when absent or blank.
    pub(super) fn turn_id(&self) -> Option<&str> {
        self.turn_id
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
    }
}

pub(super) fn render_session_panel_line(
    session: &SessionPanelSnapshot,
    provider: &ProviderKind,
) -> String {
    let mut parts = vec![session.kind.label().to_string()];
    if let Some(provider_session_id) = session
        .provider_session_id
        .as_deref()
        .filter(|value| !value.trim().is_empty())
    {
        parts.push(format!(
            "provider session {}",
            render_provider_session_label(provider, provider_session_id)
        ));
    }
    if let Some(tmux) = session.tmux {
        parts.push(format!("tmux {}", tmux.as_str()));
    }
    let head = truncate_chars(&parts.join(" · "), SESSION_PANEL_LINE_MAX_CHARS);
    if let Some(count) = session.recovery_message_count.filter(|&count| count > 0) {
        format!("{head}\n(최근 대화 {count}개를 읽어들였습니다)")
    } else {
        head
    }
}

fn render_provider_session_label(provider: &ProviderKind, session_id: &str) -> String {
    let abbreviated = abbreviate_provider_session_id(session_id);
    if abbreviated.contains('#') {
        abbreviated
    } else {
        format!("{}#{}", provider.as_str(), abbreviated)
    }
}

fn abbreviate_provider_session_id(session_id: &str) -> String {
    let trimmed = session_id.trim();
    let prefix: String = trimmed.chars().take(8).collect();
    if trimmed.chars().count() > 8 {
        format!("{prefix}…")
    } else {
        prefix
    }
}

fn session_panel_kind(kind: &str, details: &Value) -> Option<SessionPanelKind> {
    if is_fallback_session_details(details) {
        return Some(SessionPanelKind::Fallback);
    }
    match kind {
        "session_fresh" => Some(SessionPanelKind::Fresh),
        "session_resumed" => Some(SessionPanelKind::Resumed),
        "session_resume_failed_with_recovery" => Some(SessionPanelKind::Fallback),
        _ => None,
    }
}

fn is_fallback_session_details(details: &Value) -> bool {
    if first_json_bool(
        details,
        &[
            "fallback",
            "recovery",
            "recovery_injected",
            "recoveryInjected",
            "resume_failed",
            "resumeFailed",
        ],
    )
    .unwrap_or(false)
    {
        return true;
    }

    first_json_string(
        details,
        &[
            "strategy",
            "status",
            "reason",
            "recovery_action",
            "recoveryAction",
        ],
    )
    .is_some_and(|value| {
        let value = value.to_ascii_lowercase();
        value.contains("fallback") || value.contains("recovery") || value.contains("resume_failed")
    })
}

fn parse_recovery_message_count(details: &Value) -> Option<usize> {
    first_json_usize(details, &["recovery_message_count", "recoveryMessageCount"])
        .filter(|&count| count > 0)
}

fn parse_tmux_panel_state(details: &Value) -> Option<TmuxPanelState> {
    if let Some(reused) = first_json_bool(
        details,
        &[
            "tmux_reused",
            "tmuxReused",
            "tmux_kept",
            "tmuxKept",
            "tmux_session_reused",
            "tmuxSessionReused",
        ],
    ) {
        return Some(if reused {
            TmuxPanelState::Kept
        } else {
            TmuxPanelState::New
        });
    }

    let status = first_json_string(details, &["tmux_status", "tmuxStatus", "tmux"])?;
    let status = status.trim().to_ascii_lowercase();
    match status.as_str() {
        "kept" | "keep" | "reused" | "reuse" | "existing" => Some(TmuxPanelState::Kept),
        "new" | "fresh" | "created" | "recreated" => Some(TmuxPanelState::New),
        _ => None,
    }
}
