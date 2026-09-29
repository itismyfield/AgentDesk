//! The O writer: posts each transcript unit piece to its TUI channel once, only while the gateway
//! is Owned and the channel's delivery lease is held, and settles unclear results from history.

pub mod actor;
pub mod binding;
pub mod confirm;
pub mod deliver;
pub mod pieces;
pub mod rotation;
pub mod round_trip;
pub mod switch;

use std::future::Future;

use serde::{Deserialize, Serialize};

use crate::services::tui_o::shadow::SourceId;

/// `tui_o.writer` settings; nothing is posted unless explicitly enabled.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct WriterConfig {
    pub enabled: bool,
}

/// A message as Discord returned it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SeenMessage {
    pub id: u64,
    pub author_id: u64,
    pub content: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PostOutcome {
    Created(SeenMessage),
    /// 400, 403 or 404: the channel refuses the post and a retry changes nothing.
    Refused(u16),
    /// 5xx, timeout or a transport failure: the message may exist.
    Uncertain(String),
}

/// Discord as the writer uses it: one POST per piece and forward history reads.
pub trait DiscordPort: Send + Sync + 'static {
    fn bot_id(&self) -> u64;
    /// The first poll runs under the ownership gate and starts the request; the future owns
    /// everything it needs.
    fn post(
        &self,
        channel: u64,
        content: String,
    ) -> impl Future<Output = PostOutcome> + Send + 'static;
    /// Up to `confirm::HISTORY_PAGE` messages with ids above `after`, in any order.
    fn history_after(
        &self,
        channel: u64,
        after: u64,
    ) -> impl Future<Output = Result<Vec<SeenMessage>, String>> + Send;
    /// Whether reading history is provably allowed; an empty page alone proves nothing.
    fn history_readable(&self, channel: u64) -> bool;
}

/// The channel's shared delivery lease, held from before admission until the result is recorded
/// and the POST task has ended.
pub trait DeliveryLease: Send + Sync {
    type Held: Send + 'static;
    /// `None` when another holder has it; the piece waits and is never posted without it.
    fn try_acquire(&self, channel: u64, serial: u64) -> Option<Self::Held>;
}

/// Raised from the first occurrence; A1-5 routes these to health and the operator channel.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WriterAlarm {
    Blocked {
        status: u16,
    },
    PausedNoGateway,
    SchemaBlocked {
        reason: String,
    },
    LedgerViolation {
        detail: String,
    },
    Halted {
        detail: String,
    },
    ContentTransform {
        serial: u64,
    },
    Ambiguous {
        serial: u64,
    },
    Unresolved {
        serial: u64,
        reason: String,
    },
    NotFound {
        serial: u64,
    },
    /// Capture waits until delivered segments are collected; nothing is dropped.
    SpoolFull,
    /// The binding log skipped a seq; the channel stops before applying anything past it.
    BindingGap {
        expected: u64,
        found: u64,
    },
    /// The binding log could not be read; binds past `checkpoint` wait until it can.
    BindingLogUnavailable {
        checkpoint: Option<u64>,
        detail: String,
    },
    /// A bind whose transcript file is still unnamed; later binds wait behind it.
    BindingPending {
        seq: u64,
    },
    /// The source spools but posts nothing until its start is resolved.
    BoundaryPending {
        source: SourceId,
    },
    /// An old source kept growing well after its successor was bound; both stay read.
    SourceStillGrowing {
        source: SourceId,
    },
    TooManyReaders {
        count: usize,
    },
    /// A retired source grew; it is read again.
    RetiredSourceGrew {
        source: SourceId,
    },
    /// The full spool refuses an old source's tail, its successor waits behind that tail, and an
    /// announced unit keeps GC off; nothing can move, so the channel stops for an operator.
    RotationStalled {
        source: SourceId,
    },
}

pub trait AlarmSink: Send + Sync {
    fn raise(&self, channel: u64, alarm: WriterAlarm);
}

#[cfg(test)]
#[path = "writer_tests.rs"]
mod tests;
