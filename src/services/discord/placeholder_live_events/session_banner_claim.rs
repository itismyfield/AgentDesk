use crate::services::provider::ProviderKind;

use super::session_panel::{SessionPanelSnapshot, render_session_panel_line};

/// Session-scoped one-shot ledger plus the turn that owns the first-message
/// wire prefix. Kept outside `status_panel.rs` so the status state remains a
/// compact data owner rather than absorbing delivery composition policy.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct SessionBannerClaims {
    emitted_session_key: Option<String>,
    prefix: Option<SessionBannerPrefixClaim>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SessionBannerPrefixClaim {
    session_key: String,
    turn_id: String,
    line: String,
}

impl SessionBannerClaims {
    #[cfg(test)]
    pub(super) fn claim_once(
        &mut self,
        session: Option<&SessionPanelSnapshot>,
        provider: &ProviderKind,
    ) -> Option<String> {
        let session = session?;
        let line = render_session_panel_line(session, provider);
        let key = session_key(session, &line);
        if self.emitted_session_key.as_deref() == Some(key.as_str()) {
            return None;
        }
        self.emitted_session_key = Some(key);
        Some(line)
    }

    /// The winning turn receives the same line on every streaming/terminal
    /// re-compose. A later turn in the same session receives `None`; a genuine
    /// session-key change re-arms the prefix. If ordinary turn cleanup has
    /// temporarily cleared the snapshot, only the already-winning turn may use
    /// its stored line (#4451 redrive compatibility).
    pub(super) fn claim_prefix(
        &mut self,
        session: Option<&SessionPanelSnapshot>,
        provider: &ProviderKind,
        turn_id: &str,
    ) -> Option<String> {
        let turn_id = turn_id.trim();
        if turn_id.is_empty() {
            return None;
        }
        let Some(session) = session else {
            return self
                .prefix
                .as_ref()
                .filter(|claim| claim.turn_id == turn_id)
                .map(|claim| claim.line.clone());
        };
        let line = render_session_panel_line(session, provider);
        let session_key = session_key(session, &line);
        if let Some(claim) = self.prefix.as_ref()
            && claim.turn_id == turn_id
            && claim.session_key == session_key
        {
            return Some(claim.line.clone());
        }
        if self.emitted_session_key.as_deref() == Some(session_key.as_str()) {
            return None;
        }
        self.emitted_session_key = Some(session_key.clone());
        self.prefix = Some(SessionBannerPrefixClaim {
            session_key,
            turn_id: turn_id.to_string(),
            line: line.clone(),
        });
        Some(line)
    }

    pub(super) fn has_claim(&self) -> bool {
        self.emitted_session_key.is_some()
    }
}

fn session_key(session: &SessionPanelSnapshot, rendered_line: &str) -> String {
    session
        .session_instance_key()
        .map(str::to_owned)
        .or_else(|| session.provider_session_id().map(str::to_owned))
        .unwrap_or_else(|| rendered_line.to_string())
}
