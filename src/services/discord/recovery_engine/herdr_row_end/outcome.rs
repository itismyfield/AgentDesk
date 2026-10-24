//! What a restarted turn's end shows: the input's own turn as its source reads, and whether the row
//! settles silently, with the recovery notice, or with the neutral notice.

use super::*;
use crate::services::claude::herdr_turn::TurnRead;

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
}

/// Says neither that the turn finished nor that it was cut off.
pub(super) const NEUTRAL_NOTICE: &str = "ℹ️ 이전 실행은 종료됐거나 입력 가능한 상태지만, 이전 요청의 완료 여부는 확인하지 못했습니다. 요청을 자동으로 다시 실행하지는 않았습니다.";

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
        (true, Outcome::NoTerminal) | (false, Outcome::NoTerminal) => {
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
