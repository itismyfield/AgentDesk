use serde_json::Value;

use crate::services::provider::ProviderKind;

use super::common::{
    SESSION_PANEL_LINE_MAX_CHARS, first_json_bool, first_json_string, truncate_chars,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SessionPanelKind {
    Fresh,
    Resumed,
    Fallback,
}

impl SessionPanelKind {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Fresh => "fresh",
            Self::Resumed => "resumed",
            Self::Fallback => "fallback",
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
}

impl SessionPanelSnapshot {
    pub(super) fn from_lifecycle_event(kind: &str, details: &Value) -> Option<Self> {
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

        Some(Self {
            kind,
            provider_session_id,
            tmux,
        })
    }
}

pub(super) fn render_session_panel_line(
    session: &SessionPanelSnapshot,
    provider: &ProviderKind,
) -> String {
    let mut parts = vec![format!("Lifecycle {}", session.kind.as_str())];
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
    truncate_chars(&parts.join(" · "), SESSION_PANEL_LINE_MAX_CHARS)
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
