//! The O writer: posts transcript pieces under gateway ownership and a delivery lease, settles
//! unclear results from history, and permits one durable retry after confirmed absence.

pub mod activation;
pub mod actor;
pub mod adoption;
pub(crate) mod agy_delivery;
pub mod binding;
pub(crate) mod boot;
pub mod codex_adoption;
pub(crate) mod codex_snapshot;
pub mod confirm;
mod deferred;
pub mod deliver;
pub mod host;
pub mod input_facts;
pub(crate) mod input_notices;
pub(crate) mod late_adopt;
pub(crate) mod local_retry;
pub(crate) mod owed;
pub mod pieces;
pub mod progress;
pub mod reply_anchor;
// Test builds open it to the replay tests mounted in the discord-layer test host.
#[cfg(not(any(test, feature = "test-support")))]
mod replay;
#[cfg(any(test, feature = "test-support"))]
pub(in crate::services) mod replay;
pub(crate) mod restart_end;
pub(crate) mod restart_notice;
pub use replay::ReplaySummary;
pub mod resume;
pub mod rotation;
pub mod round_trip;
pub(crate) mod suspension;
pub mod switch;
mod tool_cards;
pub(crate) mod tool_images;
#[cfg(test)]
mod tool_images_tests;
mod tool_results;
pub(crate) mod turn_summary;
#[cfg(test)]
mod turn_summary_tests;

#[cfg(test)]
mod tool_card_tests;

use std::future::Future;

use serde::{Deserialize, Serialize};

use crate::services::tui_o::shadow::SourceId;

/// Derived per channel from its boot ownership, never read from config; nothing is posted unless
/// enabled.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct WriterConfig {
    pub enabled: bool,
    pub producer_absent: bool,
}

/// Reader policy selected explicitly by an engine caller; the default keeps FDs open.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ReaderConfig {
    pub dormant: bool,
}

/// Stable producer evidence; observation revisions and repeated publications are not identities.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ProducerPresence {
    Absent,
    Present,
    #[default]
    Unknown,
}
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProducerObservation {
    pub presence: ProducerPresence,
    pub incarnation: Option<String>,
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
    /// The original execution cannot authorize a request; no request left.
    Withheld(String),
    /// 5xx, timeout or a transport failure: the message may exist.
    Uncertain(String),
}

/// Discord operations used for guarded sends and bounded history confirmation.
pub trait DiscordPort: Send + Sync + 'static {
    fn bot_id(&self) -> u64;
    fn usage_footer_pool(&self) -> Option<&sqlx::PgPool> {
        None
    }
    /// The first poll runs under the ownership gate and starts the request; the future owns
    /// everything it needs.
    fn post(
        &self,
        channel: u64,
        content: String,
    ) -> impl Future<Output = PostOutcome> + Send + 'static;
    fn post_origin(
        &self,
        channel: u64,
        content: String,
        origin: Option<
            crate::services::cluster::intake_router_hook::owner_record::output::Artifact,
        >,
    ) -> impl Future<Output = PostOutcome> + Send + 'static {
        let request = origin.is_none().then(|| self.post(channel, content));
        async move {
            match request {
                Some(request) => request.await,
                None => PostOutcome::Withheld("writer has no execution owner scope".into()),
            }
        }
    }

    /// `post_origin` for one writer unit; the gateway uses `kind` for the push decision.
    fn post_unit(
        &self,
        channel: u64,
        content: String,
        origin: Option<
            crate::services::cluster::intake_router_hook::owner_record::output::Artifact,
        >,
        kind: crate::services::tui_o::shadow::UnitKind,
    ) -> impl Future<Output = PostOutcome> + Send + 'static {
        let _ = kind;
        self.post_origin(channel, content, origin)
    }

    /// `post_unit` as a reply to `reply_to`; a port without references posts it plainly.
    fn post_unit_reply(
        &self,
        channel: u64,
        content: String,
        origin: Option<
            crate::services::cluster::intake_router_hook::owner_record::output::Artifact,
        >,
        kind: crate::services::tui_o::shadow::UnitKind,
        reply_to: Option<u64>,
    ) -> impl Future<Output = PostOutcome> + Send + 'static {
        let _ = reply_to;
        self.post_unit(channel, content, origin, kind)
    }

    /// Marks an input confirmed to have joined a running turn; a port without reactions skips it.
    fn mark_joined(&self, channel: u64, message: u64) -> impl Future<Output = ()> + Send + 'static {
        let _ = (channel, message);
        std::future::ready(())
    }

    /// One writer unit's image under its caption; a port without uploads sends nothing.
    fn post_unit_attachment(
        &self,
        channel: u64,
        caption: String,
        origin: Option<
            crate::services::cluster::intake_router_hook::owner_record::output::Artifact,
        >,
        kind: crate::services::tui_o::shadow::UnitKind,
        bytes: Vec<u8>,
        filename: String,
    ) -> impl Future<Output = PostOutcome> + Send + 'static {
        let _ = (channel, caption, origin, kind, bytes, filename);
        std::future::ready(PostOutcome::Withheld(
            "writer port has no attachment upload".into(),
        ))
    }

    /// Up to `confirm::HISTORY_PAGE` messages with ids above `after`, in any order.
    fn history_after(
        &self,
        channel: u64,
        after: u64,
    ) -> impl Future<Output = Result<Vec<SeenMessage>, String>> + Send;
    /// One message by id: `Ok(None)` is a 404; a refusal, server or transport failure, or an
    /// unreadable body is an error.
    fn message(
        &self,
        channel: u64,
        id: u64,
    ) -> impl Future<Output = Result<Option<SeenMessage>, String>> + Send;
}

/// The channel's shared delivery lease, held from before admission until the result is recorded
/// and the POST task has ended.
pub trait DeliveryLease: Send + Sync {
    type Held: Send + 'static;
    /// `None` when another holder has it; the piece waits and is never posted without it.
    fn try_acquire(&self, channel: u64, serial: u64) -> Option<Self::Held>;
    /// The channel's writer stopped for good; its verified episodes become Unknown, not settled.
    fn writer_unavailable(&self, _channel: u64) {}
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
    HaltedMissingSource {
        source: SourceId,
        detail: String,
    },
    /// The writer host stopped before any store write, so Legacy keeps the channel's output.
    Released {
        detail: String,
    },
    /// Adopted at Legacy's cursor: Legacy's undelivered records in `from..to` are posted by neither.
    Abandoned {
        source: SourceId,
        from: u64,
        to: u64,
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
    /// Ready or Prepared work has waited more than five minutes; this only reports the wait.
    WaitingTooLong {
        incident: String,
        ready: usize,
        prepared: Option<u64>,
    },
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
    /// Replaced, fully read sources still hold enough readers to require retention attention.
    ReaderRetentionStalled {
        reading: usize,
        retained: usize,
    },
    /// A retired source grew; it is read again.
    RetiredSourceGrew {
        source: SourceId,
        /// The job of an operator's transition cleanup its cursor records: a past cleanup of the
        /// source, which need not be what retired it this time.
        cleanup_history: Option<String>,
    },
    CleanupRetired {
        source: SourceId,
        offset: u64,
        modified: std::time::SystemTime,
        age: std::time::Duration,
    },
    /// The full spool refuses an old source's tail, its successor waits behind that tail, and an
    /// announced unit keeps GC off; nothing can move, so the channel stops for an operator.
    RotationStalled {
        source: SourceId,
    },
    /// The home keeps a channel it committed to O although its writer selection no longer names it.
    SelectionMissing,
    /// The home keeps output ownership, but cannot start a writer without a configured binding.
    BindingMissing,
}

pub trait AlarmSink: Send + Sync {
    fn raise(&self, channel: u64, alarm: WriterAlarm);
    /// No overdue work remains; a later overdue wait is a new incident.
    fn waiting_cleared(&self, _channel: u64) {}
    fn reconcile_reader_count(&self, _channel: u64, _count: usize) {}
    fn reconcile_retained_readers(&self, _channel: u64, _reading: usize, _retained: &[String]) {}
    /// Pending boundaries of selected sources after a successful restore or capture.
    fn reconcile_boundary_pending(&self, _channel: u64, _count: usize) {}
    /// Whether the host will start the halted writer again after a wait.
    fn resume_pending(&self, _channel: u64, _pending: bool) {}
    /// The halt ended: a recovered writer is about to start in its place.
    fn halt_cleared(&self, _channel: u64) {}
}

mod historical_hops;
mod past_bind;
mod renumbered;
#[cfg(unix)]
pub(crate) use renumbered::claude_cursor_continues;
// Test builds open it to the transition tests mounted in the upper test hosts.
#[cfg(not(any(test, feature = "test-support")))]
mod transition_cleanup;
#[cfg(any(test, feature = "test-support"))]
pub(crate) mod transition_cleanup;
/// Only the periodic retention sweep sends production cleanup requests.
pub(crate) mod transition_port;

#[cfg(test)]
#[path = "writer_tests.rs"]
pub(crate) mod tests;

#[cfg(test)]
pub(crate) mod source_read_tests;
