use std::num::NonZeroU64;
use std::path::PathBuf;
use std::sync::{Arc, mpsc::Sender};
use std::time::Duration;

use poise::serenity_prelude::MessageId;
use uuid::Uuid;

use crate::services::agent_protocol::StreamMessage;
use crate::services::discord::inflight::InflightTurnState;
use crate::services::discord::router::TurnKind;
use crate::services::provider::{CancelToken, ProviderKind, StreamJsonDialectId};
use crate::services::stream_json_cli::{
    ConfiguredToolPolicy, ProviderTurnRequest, dialects::agy, execute_streaming,
};
use crate::services::tui_o::store::agy_spool::{AgyReservation, AgySpool};

use super::StreamingTurn;

#[derive(Clone)]
pub(in crate::services::discord::router::message_handler) struct AgyForeground {
    destination: u64,
    logical_channel: u64,
    message: u64,
    nonce: Uuid,
    session_key: String,
    ready: Option<ReadyWriter>,
}

#[derive(Clone)]
struct ReadyWriter {
    runtime_root: PathBuf,
    destination: u64,
    logical_channel: u64,
    generation: NonZeroU64,
}

impl AgyForeground {
    pub(in crate::services::discord::router::message_handler) fn from_claim(
        source: Option<MessageId>,
        kind: TurnKind,
        inflight_saved: bool,
        state: &InflightTurnState,
        cancel: &CancelToken,
    ) -> Option<Self> {
        let message = source?.get();
        let nonce = cancel.turn_nonce()?;
        if kind != TurnKind::Foreground
            || state.provider_kind() != Some(ProviderKind::Antigravity)
            || !inflight_saved
            || !crate::services::tui_input::input_key::is_discord_key(message)
            || state.user_msg_id != message
            || Some(nonce) != state.turn_nonce.as_deref()
        {
            return None;
        }
        Some(Self {
            destination: state.channel_id,
            logical_channel: state.logical_channel_id?,
            message,
            nonce: Uuid::parse_str(nonce).ok()?,
            session_key: state.session_key.clone().filter(|key| !key.is_empty())?,
            // Writer selection supplies this permit once the agy reader is wired.
            ready: None,
        })
    }

    fn reserve(
        &self,
        turn: &StreamingTurn<'_>,
        request: &ProviderTurnRequest,
    ) -> Result<Option<AgySpool>, String> {
        let Some(ready) = &self.ready else {
            return Ok(None);
        };
        if ready.destination != self.destination
            || turn.channel_id != self.destination
            || ready.logical_channel != self.logical_channel
            || turn
                .cancel
                .turn_nonce()
                .and_then(|nonce| Uuid::parse_str(nonce).ok())
                != Some(self.nonce)
        {
            return Err("agy recording permit does not match the Discord claim".into());
        }
        let resume = request
            .session
            .as_ref()
            .map(|token| {
                Uuid::parse_str(token.as_str()).map_err(|_| "agy recording resume id is not a UUID")
            })
            .transpose()?;
        AgySpool::reserve(
            &ready.runtime_root,
            AgyReservation {
                destination: self.destination,
                logical_channel: self.logical_channel,
                message: self.message,
                nonce: self.nonce,
                session_key: &self.session_key,
                writer_generation: ready.generation,
                resume,
            },
        )
        .map(Some)
        .map_err(|error| format!("could not reserve the agy recording: {error}"))
    }
}

pub(super) fn execute(
    dialect: StreamJsonDialectId,
    turn: &StreamingTurn<'_>,
    sender: Sender<StreamMessage>,
) -> Result<(), String> {
    execute_request(dialect, turn, request(turn)?, sender)
}

fn execute_request(
    dialect: StreamJsonDialectId,
    turn: &StreamingTurn<'_>,
    request: ProviderTurnRequest,
    sender: Sender<StreamMessage>,
) -> Result<(), String> {
    if matches!(dialect, StreamJsonDialectId::Agy)
        && let Some(input) = &turn.agy_foreground
        && let Some(mut spool) = input.reserve(turn, &request)?
    {
        return agy::execute_recorded(request, sender, &mut spool);
    }
    execute_streaming(dialect, request, sender)
}

pub(super) fn request(turn: &StreamingTurn<'_>) -> Result<ProviderTurnRequest, String> {
    ProviderTurnRequest::for_discord_turn(
        turn.provider.clone(),
        turn.prompt.to_string(),
        turn.system_prompt.map(str::to_string),
        ConfiguredToolPolicy::from_legacy_allowed_tools(turn.allowed_tools),
        turn.model.map(str::to_string),
        None,
        turn.working_dir.into(),
        turn.session_id,
        turn.force_fresh,
        Duration::from_secs(300),
        Some(Arc::clone(&turn.cancel)),
        Some(turn.channel_id),
    )
}

#[cfg(test)]
mod admission_tests;
