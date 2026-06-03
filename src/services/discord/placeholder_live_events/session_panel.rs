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
    /// Stable session-INSTANCE marker for this snapshot, derived from the tmux
    /// runtime's `.spawn_nonce` spawn marker: `"{tmux_session_name}#{nonce}"`.
    ///
    /// `.spawn_nonce` is written exactly once per spawn (a per-spawn v4 UUID) by
    /// the provider spawn sites right after `tmux::create_session` and is never
    /// touched by the live wrapper. Its content therefore uniquely identifies
    /// one wrapper INSTANCE — guaranteed unique per spawn regardless of
    /// filesystem mtime resolution — and is invariant across:
    ///   * every status tick and every TURN of the same session (the marker is
    ///     not rewritten per turn), and
    ///   * the `None`→`Some` provider-session-id assignment that lands mid-turn
    ///     on `StreamMessage::Init` (the provider id is orthogonal to the
    ///     spawn marker).
    /// A genuinely new session is a new tmux spawn (`/clear`, idle-timeout,
    /// turn-cap, cancel→respawn, …), which mints a fresh nonce — so the instance
    /// key changes exactly once, on the real boundary.
    ///
    /// Keying the reset on THIS (instead of the per-turn `turn_id`) is what
    /// fixes #3087's two false-reset P1s: a no-provider-id session running many
    /// turns keeps one instance key (no per-turn reset), and the `None`→`Some`
    /// provider-id assignment does not change it (no mid-session reset).
    /// `None` when the tmux session/marker is unavailable (e.g. headless /
    /// pre-spawn); the reset then falls back to the provider-session delta.
    session_instance_key: Option<String>,
}

impl SessionPanelSnapshot {
    pub(super) fn from_lifecycle_event(
        session_instance_key: Option<&str>,
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
        let session_instance_key = session_instance_key
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string);

        Some(Self {
            kind,
            provider_session_id,
            tmux,
            recovery_message_count,
            session_instance_key,
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

    /// The stable session-INSTANCE marker (`"{tmux_session_name}#{nonce}"`,
    /// derived from the `.spawn_nonce` spawn marker) used to detect a genuine
    /// new-session boundary even when `provider_session_id` is `None`, WITHOUT
    /// re-resetting on every status tick / turn of an ongoing session or on the
    /// `None`→`Some` provider-id assignment (#3087). Normalized to `None` when
    /// absent or blank (no live tmux marker — the reset then relies on the
    /// provider-session delta alone).
    pub(super) fn session_instance_key(&self) -> Option<&str> {
        self.session_instance_key
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
