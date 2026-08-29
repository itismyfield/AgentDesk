use super::StreamFrame;

/// Observer-only process-local order; may reset, be a hybrid, and never authorizes delivery.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct SourceEpoch(u64);

impl SourceEpoch {
    pub const fn from_observation(value: u64) -> Self {
        Self(value)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum GenerationSourceIdentity {
    #[cfg(unix)]
    Unix {
        mtime_ns: i64,
        dev: u64,
        ino: u64,
    },
    Unsupported {
        mtime_ns: i64,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct SourceWitness {
    pub generation: Option<GenerationSourceIdentity>,
    pub spawn_nonce_hash: Option<[u8; 32]>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SourceFileIdentity {
    #[cfg(unix)]
    Unix {
        dev: u64,
        ino: u64,
    },
    Unavailable,
}

impl SourceFileIdentity {
    pub fn from_open_file(file: &std::fs::File) -> Self {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            file.metadata()
                .map(|metadata| Self::Unix {
                    dev: metadata.dev(),
                    ino: metadata.ino(),
                })
                .unwrap_or(Self::Unavailable)
        }
        #[cfg(not(unix))]
        {
            let _ = file;
            Self::Unavailable
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SourceStamp {
    pub epoch: SourceEpoch,
    pub file: SourceFileIdentity,
    pub witness: SourceWitness,
}

/// #3041 P1-3 (Part a, B1): the commit-fence data the producer rides on the
/// RESULT-bearing frame. The watcher computes the authoritative consumed-terminal
/// `end` and pins the delegating turn's identity (from the inflight loaded BEFORE
/// the relay, matching #3141 pinned-id semantics) so the sink can advance the
/// offset authority identity-gated on a confirmed delivery.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TerminalCommitFence {
    pub consumed_end: u64,
    pub source_range: (u64, u64),
    pub reset_incarnation: u64,
    pub turn_user_msg_id: u64,
    pub turn_started_at: String,
    /// #3041 P1-3 (codex P1-3 issue 2): the turn's `turn_start_offset` — added to
    /// the sink's identity gate so two consecutive `user_msg_id == 0` turns started
    /// in the same `now_string` second (identical `started_at`) cannot collide.
    pub turn_start_offset: Option<u64>,
}

/// The turn identity stamped on a relayed frame. Terminal frames use this as the
/// commit-fence identity gate; non-terminal frames may also carry it so producer
/// backpressure can attribute an evicted frame to the affected turn.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RelayTurnIdentity {
    pub turn_user_msg_id: u64,
    pub turn_started_at: String,
    pub turn_start_offset: Option<u64>,
}

impl RelayTurnIdentity {
    pub fn has_strict_turn_start_offset(&self) -> bool {
        self.turn_start_offset.is_some()
    }
}

/// A frame evicted from the producer queue before the relay task consumed it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RelayDroppedFrame {
    pub sequence: u64,
    pub turn_identity: RelayTurnIdentity,
}

impl RelayDroppedFrame {
    pub(super) fn from_frame(frame: StreamFrame) -> Self {
        Self {
            sequence: frame.sequence,
            turn_identity: RelayTurnIdentity {
                turn_user_msg_id: frame.turn_user_msg_id,
                turn_started_at: frame.turn_started_at,
                turn_start_offset: frame.turn_start_offset,
            },
        }
    }
}

impl super::RelayProducer {
    pub fn try_send_frame_for_range_with_source(
        &self,
        payload: String,
        start: u64,
        end: u64,
        generation_mtime_ns: i64,
        relay_source_stamp: Option<SourceStamp>,
    ) -> bool {
        self.enqueue(
            payload,
            None,
            None,
            Some((start, end)),
            Some(generation_mtime_ns),
            relay_source_stamp,
        )
        .is_alive()
    }

    pub fn try_send_frame_with_source(
        &self,
        payload: String,
        frame_identity: Option<RelayTurnIdentity>,
        relay_generation_mtime_ns: i64,
        relay_source_stamp: Option<SourceStamp>,
    ) -> super::RelaySendOutcome {
        self.enqueue(
            payload,
            None,
            frame_identity,
            None,
            (relay_generation_mtime_ns != 0).then_some(relay_generation_mtime_ns),
            relay_source_stamp,
        )
    }

    pub fn try_send_terminal_frame_with_source(
        &self,
        payload: String,
        terminal: TerminalCommitFence,
        relay_generation_mtime_ns: i64,
        relay_source_stamp: Option<SourceStamp>,
    ) -> super::RelaySendOutcome {
        self.enqueue(
            payload,
            Some(terminal),
            None,
            None,
            (relay_generation_mtime_ns != 0).then_some(relay_generation_mtime_ns),
            relay_source_stamp,
        )
    }
}
