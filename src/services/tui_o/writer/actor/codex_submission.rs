use super::*;
use crate::services::tui_o::store::StoreError;
use crate::services::tui_o::store::codex_provenance::submit_floor::{
    DurableClosedEpisode, NativeFloor, PreparedLaunch, SubmitFloor,
};
use crate::services::tui_o::store::codex_provenance::{
    DurableDenyRecorded, projection::ReadBackProjection,
};
use crate::services::tui_prompt_dedupe::codex_verified::provenance::{
    CodexEpisodeDeny, decisions::PersistenceReason,
};
use crate::services::tui_prompt_dedupe::codex_verified::provenance::{
    CodexEpisodeSpan, ExecutionProofRef,
};
use tokio::sync::{mpsc, oneshot};

#[derive(Clone, Debug)]
pub(crate) struct Port {
    suspension: super::super::suspension::Suspension,
    generation: u64,
    run: Arc<()>,
    send: mpsc::Sender<Request>,
}

pub(crate) struct Receiver {
    generation: u64,
    run: Arc<()>,
    receive: mpsc::Receiver<Request>,
}

pub(crate) enum Operation {
    #[cfg(test)]
    Prepare(SubmitFloor),
    Capture(Box<(PreparedLaunch, ExecutionProofRef)>),
    Close(Box<CodexEpisodeSpan>),
    ReadBack(Box<ExecutionProofRef>),
    Deny(Box<CodexEpisodeDeny>),
}

pub(crate) struct Request {
    run: Arc<()>,
    operation: Operation,
    reply: oneshot::Sender<Result<Reply, CommandFailure>>,
}

struct CommandFailure {
    error: StoreError,
    reason: PersistenceReason,
}

fn suspended_failure() -> CommandFailure {
    CommandFailure {
        error: StoreError::Rejected("suspended_sources".into()),
        reason: PersistenceReason::ActorUnavailable,
    }
}

#[derive(Debug)]
pub(crate) struct Reply {
    generation: u64,
    run: Arc<()>,
    closed: Option<Closed>,
    prepared: Option<Prepared>,
    read_back: Option<ReadBack>,
    deny: Option<DenyRecorded>,
}

#[derive(Debug)]
pub(crate) struct Prepared {
    run: Arc<()>,
    floor: NativeFloor,
}

impl Prepared {
    pub(crate) fn floor(&self) -> &NativeFloor {
        &self.floor
    }
}

#[derive(Debug)]
pub(crate) struct Closed {
    run: Arc<()>,
    durable: DurableClosedEpisode,
}

impl Closed {
    pub(crate) fn span(&self) -> &CodexEpisodeSpan {
        self.durable.span()
    }
}

#[derive(Debug)]
pub(crate) struct ReadBack {
    run: Arc<()>,
    projection: ReadBackProjection,
}
impl ReadBack {
    pub(crate) fn projection(&self) -> &ReadBackProjection {
        &self.projection
    }
}
#[derive(Debug)]
pub(crate) struct DenyRecorded {
    run: Arc<()>,
    durable: DurableDenyRecorded,
}
impl DenyRecorded {
    pub(crate) fn deny(&self) -> &CodexEpisodeDeny {
        self.durable.deny()
    }
}

impl Port {
    pub(crate) fn with_suspension(
        mut self,
        suspension: super::super::suspension::Suspension,
    ) -> Self {
        self.suspension = suspension;
        self
    }

    pub(crate) fn sources_unsuspended(&self) -> bool {
        #[cfg(test)]
        if crate::services::claude_tui::source_verify::n2b_mutant("b1b-submission") {
            return true;
        }
        self.suspension.is_empty()
    }

    pub(crate) fn new(generation: u64) -> (Self, Receiver) {
        let run = Arc::new(());
        let (send, receive) = mpsc::channel(4);
        let receiver = Receiver {
            generation,
            run: Arc::clone(&run),
            receive,
        };
        (
            Self {
                suspension: Default::default(),
                generation,
                run,
                send,
            },
            receiver,
        )
    }

    #[cfg(test)]
    pub(crate) fn same_run(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.run, &other.run)
    }

    async fn request_inner(&self, operation: Operation) -> Result<Reply, CommandFailure> {
        if !self.sources_unsuspended() {
            return Err(suspended_failure());
        }
        let (reply, receive) = oneshot::channel();
        self.send
            .send(Request {
                run: Arc::clone(&self.run),
                operation,
                reply,
            })
            .await
            .map_err(|_| CommandFailure {
                error: StoreError::Rejected("actor ended".into()),
                reason: PersistenceReason::ActorUnavailable,
            })?;
        let answer = receive.await.map_err(|_| CommandFailure {
            error: StoreError::Rejected("actor reply unavailable".into()),
            reason: PersistenceReason::ActorUnavailable,
        })?;
        if !self.sources_unsuspended() {
            return Err(suspended_failure());
        }
        answer
    }

    pub(crate) async fn request(&self, operation: Operation) -> Result<Reply, String> {
        self.request_inner(operation)
            .await
            .map_err(|failure| match failure.reason {
                PersistenceReason::ActorUnavailable => match failure.error {
                    StoreError::Rejected(message) => message,
                    error => format!("{error:?}"),
                },
                _ => format!("{:?}", failure.error),
            })
    }

    pub(crate) async fn request_evidence(
        &self,
        operation: Operation,
    ) -> Result<Reply, PersistenceReason> {
        self.request_inner(operation)
            .await
            .map_err(|failure| failure.reason)
    }

    #[cfg(test)]
    pub(crate) fn replace_read_back_run_for_tests(reply: &mut Reply) {
        reply.read_back.as_mut().unwrap().run = Arc::new(());
    }

    pub(crate) fn take_read_back(&self, reply: Reply) -> Result<ReadBack, PersistenceReason> {
        if self.generation != reply.generation || !Arc::ptr_eq(&self.run, &reply.run) {
            return Err(PersistenceReason::ActorStale);
        }
        let read = reply.read_back.ok_or(PersistenceReason::WrongReply)?;
        if !Arc::ptr_eq(&self.run, &read.run) {
            return Err(PersistenceReason::ActorStale);
        }
        Ok(read)
    }

    pub(crate) fn take_deny(&self, reply: Reply) -> Result<DenyRecorded, PersistenceReason> {
        if self.generation != reply.generation || !Arc::ptr_eq(&self.run, &reply.run) {
            return Err(PersistenceReason::ActorStale);
        }
        let deny = reply.deny.ok_or(PersistenceReason::WrongReply)?;
        if !Arc::ptr_eq(&self.run, &deny.run) {
            return Err(PersistenceReason::ActorStale);
        }
        Ok(deny)
    }

    pub(crate) fn take_closed(&self, reply: Reply) -> Result<Closed, &'static str> {
        if self.generation != reply.generation || !Arc::ptr_eq(&self.run, &reply.run) {
            return Err("stale_actor_reply");
        }
        let closed = reply.closed.ok_or("episode_not_closed")?;
        if !Arc::ptr_eq(&self.run, &closed.run) {
            return Err("stale_actor_reply");
        }
        Ok(closed)
    }

    pub(crate) fn accepts_prepared(&self, prepared: &Prepared) -> bool {
        Arc::ptr_eq(&self.run, &prepared.run)
    }

    pub(crate) fn take_prepared(&self, reply: Reply) -> Result<Prepared, &'static str> {
        if self.generation != reply.generation || !Arc::ptr_eq(&self.run, &reply.run) {
            return Err("stale_actor_reply");
        }
        let prepared = reply.prepared.ok_or("episode_not_prepared")?;
        if !Arc::ptr_eq(&self.run, &prepared.run) {
            return Err("stale_actor_reply");
        }
        Ok(prepared)
    }
}

impl Receiver {
    pub(super) async fn next(&mut self) -> Option<Request> {
        self.receive.recv().await
    }

    pub(super) fn apply<P: DiscordPort, L: DeliveryLease, A: AlarmSink>(
        &self,
        writer: &mut ChannelWriter<P, L, A>,
        request: Request,
    ) {
        let mut prepared = None;
        let mut read_back = None;
        let mut deny = None;
        let mut typed_failure = None;
        let suspended = !writer.suspension.is_empty();
        #[cfg(test)]
        let suspended = suspended
            && !crate::services::claude_tui::source_verify::n2b_mutant("b1b-submission-apply");
        let result = if suspended {
            Err(StoreError::Rejected("suspended_sources".into()))
        } else if !Arc::ptr_eq(&self.run, &request.run) {
            Err(StoreError::Rejected("foreign actor command".into()))
        } else {
            match request.operation {
                #[cfg(test)]
                Operation::Prepare(floor) => {
                    if let SubmitFloor::NativeFloor(floor) = &floor {
                        prepared = Some((**floor).clone());
                    }
                    writer.store().prepare_codex_floor(floor).map(|()| None)
                }
                Operation::Capture(intent) => {
                    let (launch, execution) = *intent;
                    let capture = || -> Result<NativeFloor, StoreError> {
                        let cursor = writer
                            .store()
                            .cursor(&execution.source)
                            .ok_or_else(|| {
                                StoreError::Rejected("canonical cursor unavailable".into())
                            })?
                            .clone();
                        let modified = std::fs::metadata(&execution.source.path)?.modified()?;
                        let generation = modified
                            .duration_since(std::time::UNIX_EPOCH)
                            .ok()
                            .and_then(|duration| i64::try_from(duration.as_nanos()).ok())
                            .ok_or_else(|| {
                                StoreError::Rejected("native generation unavailable".into())
                            })?;
                        NativeFloor::capture(launch, execution, &cursor, generation)
                            .map_err(|reason| StoreError::Rejected(reason.into()))
                    };
                    capture().and_then(|floor| {
                        writer
                            .store()
                            .prepare_codex_floor(SubmitFloor::NativeFloor(Box::new(
                                floor.clone(),
                            )))?;
                        prepared = Some(floor);
                        Ok(None)
                    })
                }
                Operation::Close(span) => writer.store().close_codex_episode(*span).map(Some),
                Operation::ReadBack(execution) => writer
                    .store()
                    .codex_read_back(&execution)
                    .map(|projection| {
                        read_back = Some(projection);
                        None
                    })
                    .map_err(|reason| {
                        typed_failure = Some(reason);
                        StoreError::Rejected(format!("{reason:?}"))
                    }),
                Operation::Deny(episode) => writer
                    .store()
                    .record_codex_deny(*episode)
                    .map(|recorded| {
                        deny = Some(recorded);
                        None
                    })
                    .map_err(|reason| {
                        typed_failure = Some(reason);
                        StoreError::Rejected(format!("{reason:?}"))
                    }),
            }
        };
        if writer.store().io_failure().is_some() {
            writer.stop(WriterAlarm::Halted {
                detail: "Codex provenance publication failed".into(),
            });
        }
        let result = result.map_err(|error| CommandFailure {
            reason: typed_failure.unwrap_or_else(|| PersistenceReason::from_store(&error)),
            error,
        });
        let closed = result.map(|closed| {
            closed.map(|durable| Closed {
                run: Arc::clone(&self.run),
                durable,
            })
        });
        let reply = closed.map(|closed| Reply {
            generation: self.generation,
            run: Arc::clone(&self.run),
            closed,
            prepared: prepared.map(|floor| Prepared {
                run: Arc::clone(&self.run),
                floor,
            }),
            read_back: read_back.map(|projection| ReadBack {
                run: Arc::clone(&self.run),
                projection,
            }),
            deny: deny.map(|durable| DenyRecorded {
                run: Arc::clone(&self.run),
                durable,
            }),
        });
        let _ = request.reply.send(reply);
    }
}

#[cfg(all(test, unix))]
#[path = "scoped_submission_tests.rs"]
pub(super) mod scoped_tests;

impl crate::services::tui_o::writer::host::Readiness {
    /// Actor waits precede source/Live re-entry; the callback only consumes owned evidence.
    pub(crate) async fn read_back_original<T>(
        &self,
        snapshot: &crate::services::tui_prompt_dedupe::codex_verified::VerifiedSourceSnapshot,
        effect: impl FnOnce(ReadBack) -> T,
    ) -> Result<T, PersistenceReason> {
        let channel = snapshot
            .context()
            .channel_id
            .ok_or(PersistenceReason::IdentityMismatch)?;
        let port = self
            .submission(channel)
            .ok_or(PersistenceReason::ActorUnavailable)?;
        let reply = port
            .request_evidence(Operation::ReadBack(Box::new(snapshot.execution().clone())))
            .await?;
        crate::services::tmux_common::with_tmux_source_authority(
            &snapshot.context().tmux_session,
            |authority| {
                snapshot
                    .recheck(authority)
                    .map_err(|_| PersistenceReason::SourceChanged)?;
                self.with_submission(channel, |current| {
                    Ok(current.take_read_back(reply).map(effect))
                })
                .map_err(|_| PersistenceReason::ActorUnavailable)?
            },
        )
    }

    pub(crate) fn with_verified_prepared<T>(
        &self,
        channel: u64,
        prepared: &Prepared,
        effect: impl FnOnce() -> T,
    ) -> Result<T, PersistenceReason> {
        self.with_submission(channel, |port| {
            Ok(if port.accepts_prepared(prepared) {
                Ok(effect())
            } else {
                Err(PersistenceReason::ActorStale)
            })
        })
        .map_err(|_| PersistenceReason::ActorUnavailable)?
    }

    pub(crate) fn with_deny_reply<T>(
        &self,
        channel: u64,
        reply: Reply,
        effect: impl FnOnce(DenyRecorded) -> T,
    ) -> Result<T, PersistenceReason> {
        self.with_submission(channel, |port| Ok(port.take_deny(reply).map(effect)))
            .map_err(|_| PersistenceReason::ActorUnavailable)?
    }

    pub(crate) fn prepared_reply(
        &self,
        channel: u64,
        reply: Reply,
    ) -> Result<Prepared, &'static str> {
        self.with_submission(channel, |port| port.take_prepared(reply))
    }

    /// Rechecks this run under Live through one synchronous source-guarded effect.
    pub(crate) fn with_current_prepared<T>(
        &self,
        channel: u64,
        prepared: &Prepared,
        effect: impl FnOnce() -> T,
    ) -> Result<T, &'static str> {
        self.with_submission(channel, |port| {
            if !port.accepts_prepared(prepared) {
                return Err("stale_actor_reply");
            }
            Ok(effect())
        })
    }

    /// Consumers must be bounded and synchronous, without re-entering Readiness.
    #[cfg(test)]
    pub(crate) fn with_prepared_reply<T>(
        &self,
        channel: u64,
        reply: Reply,
        consume: impl FnOnce(&Prepared) -> T,
    ) -> Result<T, &'static str> {
        self.with_submission(channel, |port| Ok(consume(&port.take_prepared(reply)?)))
    }

    /// Consumers must be bounded and synchronous, without re-entering Readiness.
    pub(crate) fn with_closed_reply<T>(
        &self,
        channel: u64,
        reply: Reply,
        issue: impl FnOnce(&Closed) -> T,
    ) -> Result<T, &'static str> {
        self.with_submission(channel, |port| Ok(issue(&port.take_closed(reply)?)))
    }
}

thread_local! {
    static SPAWN: std::cell::RefCell<Option<Receiver>> = const { std::cell::RefCell::new(None) };
}

pub(crate) fn spawning<T>(receiver: Receiver, spawn: impl FnOnce() -> T) -> T {
    struct Scope;
    impl Drop for Scope {
        fn drop(&mut self) {
            SPAWN.with(|slot| slot.borrow_mut().take());
        }
    }
    SPAWN.with(|slot| {
        assert!(slot.borrow().is_none());
        *slot.borrow_mut() = Some(receiver);
    });
    let _scope = Scope;
    spawn()
}

pub(super) fn take_spawn_receiver() -> Option<Receiver> {
    SPAWN.with(|slot| slot.borrow_mut().take())
}
