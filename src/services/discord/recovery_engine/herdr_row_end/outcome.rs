//! What a restarted turn's end shows: the input's own turn as its source reads, and whether the row
//! settles silently, with the recovery notice, or with the neutral notice.

use super::*;
use crate::services::claude::herdr_turn::TurnRead;
use crate::services::tui_o::shadow::ShadowProvider;
use crate::services::tui_o::store::restart_notice::{Card, Intent};

/// The input's own turn: its terminal, none, or nothing that names it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Outcome {
    Terminal(Terminal),
    NoTerminal,
    /// Another turn's records share its range; `after_prompt` once the input's prompt was read.
    Mixed {
        after_prompt: bool,
    },
    /// No input time, an unread source, or a rollout that cannot bound the input's turn.
    Unknown,
}

impl From<TurnRead> for Outcome {
    fn from(read: TurnRead) -> Self {
        match read {
            TurnRead::Terminal(terminal) => Self::Terminal(terminal),
            TurnRead::NoTerminal => Self::NoTerminal,
            TurnRead::Mixed { after_prompt } => Self::Mixed { after_prompt },
        }
    }
}

/// How the row settles once its end is seen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Settle {
    /// O owns the turn's output and had nothing to lose: no write, and the clear rechecks O.
    Silent,
    /// Through the recovery notice, which shows only its marker on an O-owned channel.
    Notice(String),
    /// Through transport alone: no marker, body claim or delivery context.
    Neutral(String),
    /// O owns the cut turn's output: its writer posts the one restart notice from this intent.
    Restart(Intent),
}

/// Says neither that the turn finished nor that it was cut off.
pub(super) use super::super::completion_delivery::RECOVERY_NEUTRAL_NOTICE as NEUTRAL_NOTICE;

/// `idle` is I's evidence, else E's; `o_owned` is the row channel's output owner read now.
pub(super) fn settle(
    provider: &ProviderKind,
    row: &inflight::InflightTurnState,
    outcome: &Outcome,
    idle: bool,
    o_owned: bool,
) -> Settle {
    use crate::services::agent_protocol::NativeTerminalKind::Completed;
    match (o_owned, outcome) {
        (true, Outcome::Terminal(_)) => Settle::Silent,
        // The input reached the provider and the latest turn closed; O relays what followed.
        (true, Outcome::Mixed { after_prompt: true }) if idle => Settle::Silent,
        (true, Outcome::NoTerminal) => match restart_intent(provider, row) {
            Some(intent) => Settle::Restart(intent),
            None => Settle::Notice(interrupted_recovery_message(row, &row.full_response)),
        },
        (false, Outcome::NoTerminal) => {
            Settle::Notice(interrupted_recovery_message(row, &row.full_response))
        }
        (true, _) => Settle::Neutral(NEUTRAL_NOTICE.to_string()),
        (false, Outcome::Terminal(terminal)) if terminal.kind == Completed => {
            Settle::Notice(match turn_body(Some(terminal.clone())) {
                Some(body) => {
                    super::super::formatting::format_for_discord_with_provider(&body, provider)
                }
                None => super::super::completion_delivery::RECOVERED_WITHOUT_TEXT.to_string(),
            })
        }
        (false, Outcome::Terminal(_)) => {
            Settle::Notice(interrupted_recovery_message(row, &row.full_response))
        }
        (false, _) => Settle::Notice(neutral_with_partial(&row.full_response)),
    }
}

/// The exact episode a restart cut; a row without one keeps the transport notice.
fn restart_intent(provider: &ProviderKind, row: &inflight::InflightTurnState) -> Option<Intent> {
    let shadow = match provider {
        ProviderKind::Claude => ShadowProvider::Claude,
        ProviderKind::Codex => ShadowProvider::Codex,
        _ => return None,
    };
    let nonce = row.turn_nonce.as_deref();
    let text = crate::services::tui_o::writer::restart_notice::text(&row.user_text);
    let mut intent = Intent::exact(row.channel_id, shadow, nonce, row.born_generation, text)?;
    intent.card = (row.current_msg_id != 0).then(|| Card {
        msg_id: row.current_msg_id,
        marker: interrupted_recovery_message(row, ""),
    });
    Some(intent)
}

/// Seals the notice under the input's original admission, so a managed channel's output check
/// admits it; with none found it stays unstamped and that check decides.
pub(super) async fn with_origin(
    pool: &sqlx::PgPool,
    provider: &ProviderKind,
    row: &inflight::InflightTurnState,
    mut intent: Intent,
) -> Intent {
    use crate::services::cluster::intake_router_hook::owner_record::output::source_artifact;
    let (channel, message) = (row.channel_id.to_string(), row.user_msg_id.to_string());
    let found = source_artifact(
        Some(pool),
        provider.as_str(),
        &channel,
        &message,
        None,
        &intent.text,
    );
    match found.await {
        Ok(origin) => intent.origin = origin,
        Err(error) => {
            tracing::warn!(channel_id = row.channel_id, %error, "restart notice unsealed")
        }
    }
    intent
}

/// The neutral notice after the row's saved partial, which it never calls complete.
fn neutral_with_partial(partial: &str) -> String {
    match partial.trim() {
        "" => NEUTRAL_NOTICE.to_string(),
        partial => format!(
            "{}\n\n{NEUTRAL_NOTICE}",
            super::super::formatting::format_for_discord(partial)
        ),
    }
}
