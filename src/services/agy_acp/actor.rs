//! Per-destination actor: owns one `agy acp` child and its session, fences replies by
//! incarnation, and enforces the inactivity and cancel deadlines.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::time::{Duration, Instant};

use serde_json::Value;

use super::Launch;
use super::child::{self, Owned};
use super::rpc::{self, Inbound, RpcError};
use super::wire::{self, StopReason, Update};

pub(super) enum Event {
    Turn(Box<TurnRequest>),
    Cancel { turn: u64 },
    Terminate { turn: u64 },
    Line { incarnation: u64, inbound: Inbound },
    Eof { incarnation: u64 },
    Exited { incarnation: u64 },
    Shutdown,
}

pub(super) struct TurnRequest {
    pub turn: u64,
    pub launch: Launch,
    pub text: String,
    pub system: Option<String>,
    /// The agy recording stores conversation ids as UUIDs; other ids fail before the prompt.
    pub uuid_session: bool,
    pub events: Sender<TurnEvent>,
}

#[derive(Debug, PartialEq)]
pub(super) enum TurnEvent {
    Attached { session: String },
    Update(Update),
    Finished(Result<StopReason, Failure>),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Failure {
    /// Spawn, a held orphan sweep, or the handshake failed before any prompt was written.
    Start(String),
    Rpc(RpcError),
    Exited,
    Inactive,
    Cancelled,
}

#[derive(Clone, Copy)]
pub(super) struct Deadlines {
    pub inactivity: Duration,
    pub cancel: Duration,
}

pub(super) const DEADLINES: Deadlines = Deadlines {
    inactivity: Duration::from_secs(15 * 60),
    cancel: Duration::from_secs(15),
};

/// Dropping the last handle stops the actor and its child.
#[derive(Clone)]
pub(super) struct Handle(Arc<Sender<Event>>);

impl Handle {
    pub(super) fn send(&self, event: Event) -> bool {
        self.0.send(event).is_ok()
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        if Arc::strong_count(&self.0) == 1 {
            let _ = self.0.send(Event::Shutdown);
        }
    }
}

pub(super) fn spawn(record_dir: Option<PathBuf>, deadlines: Deadlines) -> Handle {
    let (tx, rx) = channel();
    let mut actor = Actor {
        record: record_dir.map(|dir| dir.join(child::RECORD)),
        deadlines,
        tx: tx.clone(),
        incarnation: 0,
        resident: None,
        pending: HashMap::new(),
        turn: None,
    };
    std::thread::spawn(move || actor.run(rx));
    Handle(Arc::new(tx))
}

#[derive(Clone, Copy)]
enum Step {
    Initialize,
    NewSession,
    Prompt,
}

struct Resident {
    child: Owned,
    next_id: u64,
    session: Option<String>,
    system_hash: Option<u64>,
}

struct Open {
    request: TurnRequest,
    prompts: usize,
    cancelling: Option<Instant>,
    active_at: Instant,
}

struct Actor {
    record: Option<PathBuf>,
    deadlines: Deadlines,
    tx: Sender<Event>,
    incarnation: u64,
    resident: Option<Resident>,
    pending: HashMap<(u64, u64), Step>,
    turn: Option<Open>,
}

impl Actor {
    fn run(&mut self, rx: Receiver<Event>) {
        loop {
            let wait = self.deadline().map_or(Duration::from_secs(3600), |at| {
                at.saturating_duration_since(Instant::now())
            });
            match rx.recv_timeout(wait) {
                Ok(Event::Shutdown) | Err(RecvTimeoutError::Disconnected) => break,
                Ok(event) => self.handle(event),
                Err(RecvTimeoutError::Timeout) => self.expire(),
            }
        }
        self.stop_child();
    }

    fn deadline(&self) -> Option<Instant> {
        let open = self.turn.as_ref()?;
        let idle = open.active_at + self.deadlines.inactivity;
        Some(
            open.cancelling
                .map_or(idle, |at| idle.min(at + self.deadlines.cancel)),
        )
    }

    fn expire(&mut self) {
        if self.deadline().is_some_and(|at| at <= Instant::now()) {
            self.lost(Failure::Inactive);
        }
    }

    /// The child is gone or stopped; a turn being cancelled ends as cancelled.
    fn lost(&mut self, otherwise: Failure) {
        self.stop_child();
        let cancelling = self
            .turn
            .as_ref()
            .is_some_and(|open| open.cancelling.is_some());
        self.finish(Err(if cancelling {
            Failure::Cancelled
        } else {
            otherwise
        }));
    }

    fn handle(&mut self, event: Event) {
        match event {
            Event::Turn(request) => self.start(*request),
            Event::Cancel { turn } if self.owns(turn) => self.cancel(),
            Event::Terminate { turn } if self.owns(turn) => {
                self.stop_child();
                self.finish(Err(Failure::Cancelled));
            }
            Event::Line {
                incarnation,
                inbound,
            } if incarnation == self.live() => {
                if let Some(open) = &mut self.turn {
                    open.active_at = Instant::now();
                }
                self.receive(inbound);
            }
            Event::Eof { incarnation } | Event::Exited { incarnation }
                if incarnation == self.live() =>
            {
                self.pending.retain(|(owner, _), _| *owner != incarnation);
                self.lost(Failure::Exited);
            }
            // Another incarnation's line or exit, or a stale turn's control.
            _ => {}
        }
    }

    fn live(&self) -> u64 {
        self.resident.as_ref().map_or(0, |_| self.incarnation)
    }

    fn owns(&self, turn: u64) -> bool {
        self.turn
            .as_ref()
            .is_some_and(|open| open.request.turn == turn)
    }

    fn start(&mut self, request: TurnRequest) {
        if self.turn.is_some() {
            let busy = Failure::Start("an agy acp turn is already running here".into());
            let _ = request.events.send(TurnEvent::Finished(Err(busy)));
            return;
        }
        let launch = request.launch.clone();
        self.turn = Some(Open {
            request,
            prompts: 0,
            cancelling: None,
            active_at: Instant::now(),
        });
        if self
            .resident
            .as_ref()
            .is_some_and(|resident| resident.session.is_some())
        {
            return self.prompt();
        }
        self.stop_child();
        if let Some(record) = &self.record
            && let Err(error) = child::sweep_recorded(record)
        {
            return self.finish(Err(Failure::Start(format!("이전 agy 정리 실패: {error}"))));
        }
        self.incarnation += 1;
        match child::spawn(
            &launch,
            self.record.as_deref(),
            self.incarnation,
            self.tx.clone(),
        ) {
            Ok(child) => {
                let (next_id, session, system_hash) = (0, None, None);
                self.resident = Some(Resident {
                    child,
                    next_id,
                    session,
                    system_hash,
                });
                self.request(Step::Initialize, "initialize", wire::initialize_params());
            }
            Err(error) => self.finish(Err(Failure::Start(error))),
        }
    }

    fn request(&mut self, step: Step, method: &str, params: Value) {
        let Some(resident) = &mut self.resident else {
            return;
        };
        resident.next_id += 1;
        let id = resident.next_id;
        let line = rpc::request(id, method, params);
        self.pending.insert((self.incarnation, id), step);
        self.write(&line);
    }

    fn write(&mut self, message: &Value) {
        let written = self
            .resident
            .as_mut()
            .and_then(|resident| resident.child.stdin.as_mut())
            .is_some_and(|stdin| rpc::write_line(stdin, message).is_ok());
        if !written {
            self.stop_child();
            self.finish(Err(Failure::Exited));
        }
    }

    fn receive(&mut self, inbound: Inbound) {
        match inbound {
            Inbound::Response { id, outcome } => {
                if let Some(step) = self.pending.remove(&(self.incarnation, id)) {
                    self.answered(step, outcome);
                }
            }
            Inbound::Notification { method, params } if method == "session/update" => {
                let session = self.resident.as_ref().and_then(|r| r.session.as_deref());
                let update = session.and_then(|session| wire::parse_update(&params, session));
                match (&self.turn, update) {
                    (_, None | Some(Update::Other)) => {}
                    (Some(open), Some(update)) if open.prompts > 0 => {
                        let _ = open.request.events.send(TurnEvent::Update(update));
                    }
                    // Output after the last prompt answered has no owning input.
                    _ => tracing::debug!("agy acp update outside a prompt dropped"),
                }
            }
            Inbound::Notification { .. } => {}
            Inbound::Request { id, method, params } => {
                let reply = if method == "session/request_permission" {
                    let cancelling = self.turn.as_ref().is_none_or(|o| o.cancelling.is_some());
                    rpc::response(&id, wire::permission_outcome(&params, cancelling))
                } else {
                    rpc::error_response(&id, -32601, "not supported by AgentDesk")
                };
                self.write(&reply);
            }
        }
    }

    fn answered(&mut self, step: Step, outcome: Result<Value, RpcError>) {
        match (step, outcome) {
            // The turn was cancelled during the handshake; the next turn starts a fresh child.
            (Step::Initialize, Ok(_)) if self.turn.is_none() => self.stop_child(),
            (Step::Initialize, Ok(result)) => match wire::parse_initialize(&result) {
                Ok(agent) => {
                    let capabilities = result.get("agentCapabilities").cloned().unwrap_or_default();
                    tracing::info!(version = ?agent.version, %capabilities, "agy acp initialized");
                    let system = self
                        .turn
                        .as_ref()
                        .and_then(|open| open.request.system.clone());
                    let append = system.filter(|_| agent.system_prompt_append);
                    let Some(resident) = &mut self.resident else {
                        return;
                    };
                    resident.system_hash = append.as_deref().map(hash);
                    let cwd = self.turn.as_ref().map_or_else(String::new, |open| {
                        open.request.launch.cwd.to_string_lossy().into_owned()
                    });
                    let params = wire::new_session_params(&cwd, append.as_deref());
                    self.request(Step::NewSession, "session/new", params);
                }
                Err(error) => {
                    tracing::error!(%error, "agy acp protocol version is not supported");
                    self.fail_start(error);
                }
            },
            (Step::NewSession, Ok(result)) => match wire::parse_session_id(&result) {
                Ok(session) => {
                    if let Some(resident) = &mut self.resident {
                        resident.session = Some(session);
                    }
                    self.prompt();
                }
                Err(error) => self.fail_start(error),
            },
            (Step::Initialize | Step::NewSession, Err(error)) => {
                self.fail_start(rpc::rpc_message(&error, "handshake"))
            }
            (Step::Prompt, outcome) => {
                let Some(open) = &mut self.turn else { return };
                open.prompts = open.prompts.saturating_sub(1);
                if open.prompts == 0 {
                    self.finish(
                        outcome
                            .map(|result| wire::parse_stop_reason(&result))
                            .map_err(Failure::Rpc),
                    );
                }
            }
        }
    }

    fn fail_start(&mut self, error: String) {
        self.stop_child();
        self.finish(Err(Failure::Start(error)));
    }

    fn prompt(&mut self) {
        let (Some(open), Some(resident)) = (&mut self.turn, &mut self.resident) else {
            return;
        };
        let Some(session) = resident.session.clone() else {
            return;
        };
        if open.request.uuid_session && uuid::Uuid::parse_str(&session).is_err() {
            let error = "agy acp session id is not a UUID; the agy recording cannot hold it";
            return self.finish(Err(Failure::Start(error.into())));
        }
        let _ = open.request.events.send(TurnEvent::Attached {
            session: session.clone(),
        });
        let mut text = open.request.text.clone();
        if let Some(system) = &open.request.system
            && resident.system_hash != Some(hash(system))
        {
            text = crate::services::stream_json_cli::dialects::agy::compose_envelope(system, &text);
            resident.system_hash = Some(hash(system));
        }
        open.prompts += 1;
        open.active_at = Instant::now();
        self.request(
            Step::Prompt,
            "session/prompt",
            wire::prompt_params(&session, &text),
        );
    }

    fn cancel(&mut self) {
        let Some(open) = &mut self.turn else { return };
        if open.cancelling.is_some() {
            return;
        }
        open.cancelling = Some(Instant::now());
        let session = self.resident.as_ref().and_then(|r| r.session.clone());
        match session.filter(|_| open.prompts > 0) {
            Some(session) => {
                self.write(&rpc::notification(
                    "session/cancel",
                    serde_json::json!({ "sessionId": session }),
                ));
            }
            // Nothing was prompted yet: the input is not written at all.
            None => self.finish(Err(Failure::Cancelled)),
        }
    }

    fn stop_child(&mut self) {
        if let Some(mut resident) = self.resident.take() {
            resident.child.terminate();
        }
    }

    fn finish(&mut self, result: Result<StopReason, Failure>) {
        if let Some(open) = self.turn.take() {
            let _ = open.request.events.send(TurnEvent::Finished(result));
        }
    }
}

fn hash(text: &str) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut hasher);
    hasher.finish()
}
