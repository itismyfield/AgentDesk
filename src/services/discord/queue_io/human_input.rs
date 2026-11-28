//! Request, result, and dedupe ledger shared by the human-input paths.

use poise::serenity_prelude::{ChannelId, MessageId};

use crate::services::codex_tui::busy_inject as codex_inject;
use crate::services::provider::ProviderKind;

pub struct HumanInputRequest {
    pub channel_id: ChannelId,
    pub provider: ProviderKind,
    pub text: String,
    pub author_id: u64,
    pub source: String,
    pub metadata: Option<serde_json::Value>,
    pub channel_name_hint: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum InjectAttempt {
    /// Nothing was reserved or sent; the caller's own start or queue follows.
    NotSent(&'static str),
    Injected {
        turn_id: Option<String>,
    },
    Unconfirmed {
        turn_id: Option<String>,
        detail: &'static str,
    },
    /// The pane vetoed after the reservation; the input took the queue front as `turn_id`.
    HandedBack {
        turn_id: String,
        veto: &'static str,
    },
    /// The pane vetoed or an admitted GUID stayed unbound, and the queue front was not written
    /// or its result is unknown.
    HandbackFailed(&'static str),
    /// The owner died outside the pane effect: nothing was recorded and the paste is unknown.
    OwnerFailed {
        turn_id: Option<String>,
    },
}

/// The head was left Unknown by an earlier attempt; nothing types it again.
pub(in crate::services::discord) const HEAD_UNKNOWN: &str = "queued_head_unknown";
/// The Unknown head was consumed on transcript proof alone; nothing was typed for it.
pub(in crate::services::discord) const RECONCILED: &str = "queued_head_reconciled";

impl InjectAttempt {
    /// `(outcome, reason, head_state)` of a finished attempt: `terminal`, or `queued` for a
    /// handed-back NotSent, only where the actor committed it; otherwise unknown or unavailable.
    pub(crate) fn queued_report(&self) -> (&'static str, &'static str, &'static str) {
        match self {
            Self::Injected { .. } => ("injected", "", "terminal"),
            Self::NotSent(RECONCILED) => ("reconciled", RECONCILED, "terminal"),
            Self::NotSent(veto) => ("not_sent", veto, "unavailable"),
            Self::HandbackFailed(HEAD_UNKNOWN) => ("blocked", HEAD_UNKNOWN, "unknown"),
            Self::HandbackFailed(detail) => ("blocked", detail, "unavailable"),
            Self::Unconfirmed { detail, .. } => ("unconfirmed", detail, "unknown"),
            Self::HandedBack { veto, .. } => ("not_sent", veto, "queued"),
            Self::OwnerFailed { .. } => ("unconfirmed", "owner_failed", "unavailable"),
        }
    }
}

/// Tells the dedupe observers the nonce is this Discord message, aimed at the target turn.
pub(in crate::services::discord) struct CodexLedger<'a> {
    pub(in crate::services::discord) session: &'a str,
    pub(in crate::services::discord) nonce: &'a str,
    pub(in crate::services::discord) target_turn: &'a str,
    pub(in crate::services::discord) message: Option<MessageId>,
}

impl codex_inject::Ledger for CodexLedger<'_> {
    fn register(&self) -> bool {
        let message = self.message.map(MessageId::get);
        let register = crate::services::tui_prompt_dedupe::register_injected_steer;
        register("codex", self.session, self.nonce, self.target_turn, message)
    }

    fn withdraw(&self) {
        let withdraw = crate::services::tui_prompt_dedupe::withdraw_injected_steer;
        withdraw("codex", self.session, self.nonce);
    }
    fn message_id(&self) -> Option<u64> {
        self.message.map(MessageId::get)
    }
    fn admitted(&self) {
        self.withdraw();
    }
}
