use super::StreamFrame;

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
