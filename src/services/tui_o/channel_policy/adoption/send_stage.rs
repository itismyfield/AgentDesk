//! Where each open Legacy body send last got to, so one that never finishes names its stage.

use std::collections::BTreeMap;
use std::panic::Location;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::Instant;

/// The last step an open Legacy body send's holder reported; only its guard's drop ends it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum SendStage {
    /// Claimed, with the transport still out.
    BeforeSend,
    /// The transport returned and the follow-up settlement has not.
    SentUnsettled,
    /// The settlement returned and the guard is still held.
    SettledUndropped,
}

impl SendStage {
    fn name(self) -> &'static str {
        match self {
            Self::BeforeSend => "before_send",
            Self::SentUnsettled => "sent_unsettled",
            Self::SettledUndropped => "settled_undropped",
        }
    }
}

#[derive(Debug)]
struct Open {
    origin: &'static Location<'static>,
    stage: SendStage,
    since: Instant,
}

/// The open sends by claim order, with how many ever reported each later stage. Its lock is a
/// leaf: nothing else is taken under it.
#[derive(Debug, Default)]
pub(super) struct OpenSends {
    open: Mutex<BTreeMap<u64, Open>>,
    sent: AtomicU64,
    settled: AtomicU64,
}

impl OpenSends {
    fn open(&self) -> MutexGuard<'_, BTreeMap<u64, Open>> {
        self.open.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub(super) fn begin(&self, seq: u64, origin: &'static Location<'static>) {
        let send = Open {
            origin,
            stage: SendStage::BeforeSend,
            since: Instant::now(),
        };
        self.open().insert(seq, send);
    }

    /// A stage only moves forward, and is counted the first time its send passes it.
    pub(super) fn reach(&self, seq: u64, stage: SendStage) {
        let mut open = self.open();
        let Some(send) = open.get_mut(&seq) else {
            return;
        };
        for (passed, count) in [
            (SendStage::SentUnsettled, &self.sent),
            (SendStage::SettledUndropped, &self.settled),
        ] {
            if send.stage < passed && passed <= stage {
                count.fetch_add(1, Ordering::SeqCst);
            }
        }
        send.stage = send.stage.max(stage);
    }

    pub(super) fn end(&self, seq: u64) {
        self.open().remove(&seq);
    }

    /// How many sends reported the transport back, and how many the settlement after it.
    pub(super) fn counts(&self) -> (u64, u64) {
        let sent = self.sent.load(Ordering::SeqCst);
        (sent, self.settled.load(Ordering::SeqCst))
    }

    /// The oldest open send with its last stage, or `none`.
    pub(super) fn oldest(&self) -> String {
        match self.open().iter().next() {
            Some((seq, send)) => format!(
                "seq={seq},origin={},stage={},open_secs={}",
                send.origin,
                send.stage.name(),
                send.since.elapsed().as_secs()
            ),
            None => "none".to_string(),
        }
    }
}
