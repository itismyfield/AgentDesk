//! Maps one ACP turn onto the existing outputs: `StreamMessage`s for turn_bridge and
//! the reserved agy spool. Nothing below turn_bridge learns that ACP produced them.

use std::sync::mpsc::{RecvTimeoutError, Sender, channel};

use uuid::Uuid;

use crate::services::agent_protocol::StreamMessage;
use crate::services::process::stream_child::EXIT_POLL;
use crate::services::provider::{CancelSource, CancelToken, cancel_requested};
use crate::services::tui_o::store::agy_spool::{AgySpool, Failure as SpoolFailure};

use super::actor::{Event, Failure, Handle, TurnEvent, TurnRequest};
use super::rpc::rpc_message;
use super::wire::{StopReason, Update};

pub(super) struct Outputs<'a> {
    pub sender: Sender<StreamMessage>,
    pub spool: Option<&'a mut AgySpool>,
    /// The request's own provider-session value, handed back unchanged; the ACP id never replaces it.
    pub shared_session: Option<String>,
    pub cancel: Option<&'a CancelToken>,
    pub conversation: Option<Uuid>,
}

pub(super) fn drive(
    handle: &Handle,
    mut request: TurnRequest,
    mut out: Outputs<'_>,
) -> Result<(), String> {
    let (events, rx) = channel();
    request.events = events;
    let turn = request.turn;
    if !handle.send(Event::Turn(Box::new(request))) {
        out.record_fail(SpoolFailure::Spawn);
        return Err("agy acp actor is not running".into());
    }
    // `Some(true)` once Terminate went out; a sent Cancel still yields to a later forced stop.
    let mut sent: Option<bool> = None;
    let mut body = String::new();
    loop {
        if cancel_requested(out.cancel) {
            let hard = out.cancel.is_some_and(forced);
            let stop = if hard {
                Event::Terminate { turn }
            } else {
                Event::Cancel { turn }
            };
            if sent < Some(hard) && handle.send(stop) {
                sent = Some(hard);
            }
        }
        match rx.recv_timeout(EXIT_POLL) {
            Ok(TurnEvent::Attached { session }) => out.attached(&session),
            Ok(TurnEvent::Update(update)) => out.update(update, &mut body),
            Ok(TurnEvent::Finished(result)) => return out.finish(result, body),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return out.finish(Err(Failure::Exited), body),
        }
    }
}

/// A forced stop (session cleanup or PID escalation), teardown or watchdog ends the child at
/// once; any other stop asks agy to cancel.
fn forced(token: &CancelToken) -> bool {
    token.termination_requested()
        || matches!(
            token.cancel_source_kind(),
            Some(CancelSource::SessionTeardown | CancelSource::WatchdogTimeout)
        )
}

impl Outputs<'_> {
    fn send(&self, message: StreamMessage) {
        let _ = self.sender.send(message);
    }

    fn record(&mut self, write: impl FnOnce(&mut AgySpool) -> std::io::Result<()>) {
        if let Some(spool) = self.spool.as_deref_mut()
            && let Err(error) = write(spool)
        {
            tracing::warn!(%error, "agy acp spool remains unsealed");
            // A broken prefix is never extended; the spool settles as failed when it can.
            self.spool = None;
        }
    }

    fn record_fail(&mut self, reason: SpoolFailure) {
        self.record(|spool| spool.fail(reason));
    }

    fn attached(&mut self, session: &str) {
        if let Some(shared) = self.shared_session.clone() {
            self.send(StreamMessage::Init {
                session_id: shared.clone(),
                raw_session_id: Some(shared),
            });
        }
        if let Ok(conversation) = Uuid::parse_str(session) {
            self.conversation = Some(conversation);
            self.record(|spool| spool.init(conversation));
        }
    }

    fn update(&mut self, update: Update, body: &mut String) {
        match update {
            Update::Text(content) if !content.is_empty() => {
                body.push_str(&content);
                self.record(|spool| spool.delta(&content));
                self.send(StreamMessage::Text { content });
            }
            Update::Thought => self.send(StreamMessage::Thinking { summary: None }),
            Update::ToolCall { id, title, input } => self.send(StreamMessage::ToolUse {
                name: title,
                input,
                tool_use_id: id,
            }),
            Update::ToolResult {
                id,
                content,
                is_error,
            } => self.send(StreamMessage::ToolResult {
                content,
                is_error,
                tool_use_id: id,
            }),
            Update::Text(_) | Update::Other => {}
        }
    }

    fn error(&mut self, reason: SpoolFailure, message: String) -> Result<(), String> {
        self.record_fail(reason);
        self.send(StreamMessage::Error {
            message,
            stdout: String::new(),
            stderr: String::new(),
            exit_code: None,
        });
        Ok(())
    }

    fn finish(&mut self, result: Result<StopReason, Failure>, body: String) -> Result<(), String> {
        if cancel_requested(self.cancel) {
            // As the `-p` runner: no added terminal message; turn_bridge's cancel path settles it.
            self.record_fail(SpoolFailure::Cancelled);
            return Ok(());
        }
        let (reason, message) = match result {
            Ok(stop) => return self.stopped(stop, body),
            Err(Failure::Start(message)) => {
                self.record_fail(SpoolFailure::Spawn);
                return Err(message);
            }
            Err(Failure::Rpc(error)) => (SpoolFailure::Spawn, rpc_message(&error, "prompt")),
            Err(Failure::Exited) => (
                SpoolFailure::Child,
                "agy acp exited before the turn finished".into(),
            ),
            Err(Failure::Inactive) => (
                SpoolFailure::Child,
                "agy acp produced no activity before its deadline".into(),
            ),
            Err(Failure::Cancelled) => (SpoolFailure::Cancelled, "cancelled".into()),
        };
        self.error(reason, message)
    }

    fn stopped(&mut self, stop: StopReason, mut body: String) -> Result<(), String> {
        let truncated = matches!(stop, StopReason::MaxTokens | StopReason::MaxTurnRequests);
        if truncated && !body.trim().is_empty() {
            let notice = "\n\n(agy 응답이 길이 한도에서 잘렸습니다.)".to_string();
            self.update(Update::Text(notice), &mut body);
        }
        match stop {
            StopReason::EndTurn | StopReason::MaxTokens | StopReason::MaxTurnRequests
                if !body.trim().is_empty() =>
            {
                match self.conversation {
                    Some(conversation) => self.record(|spool| spool.done(conversation, &body)),
                    None => self.record_fail(SpoolFailure::Protocol),
                }
                self.send(StreamMessage::Done {
                    result: body,
                    session_id: self.shared_session.clone(),
                });
                Ok(())
            }
            StopReason::EndTurn | StopReason::MaxTokens | StopReason::MaxTurnRequests => self
                .error(
                    SpoolFailure::Protocol,
                    format!("agy acp ended the turn ({stop:?}) without any response text"),
                ),
            StopReason::Refusal => self.error(
                SpoolFailure::Protocol,
                "agy refused this request (quota or policy limit)".into(),
            ),
            StopReason::Cancelled => {
                self.error(SpoolFailure::Cancelled, "agy cancelled the turn".into())
            }
            StopReason::Other(reason) => self.error(
                SpoolFailure::Protocol,
                format!("agy acp ended with an unknown stop reason {reason:?}"),
            ),
        }
    }
}
