//! Test-only check that a claim which released a pending adoption was followed by the body it was
//! taken for: a sink showing that body settles the release and must find it released already.

use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

use super::{Adoption, Candidate};

pub(super) fn note_release(candidate: &Candidate) {
    candidate.bodiless.store(true, Ordering::SeqCst);
}

/// The write that carried content to a sink.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SinkOp {
    Post,
    Patch,
}

/// One selected channel's adoption, watched against a distinctive part of the assistant body its
/// scenario would send. Clones share the watch, so a sink on another task or thread can hold one.
#[derive(Clone, Debug)]
pub(crate) struct BodyCheck {
    channel: u64,
    candidate: Candidate,
    body: String,
    /// Adoptions a sink found as the body left without a claim before it.
    early: Arc<Mutex<Vec<Adoption>>>,
    /// Writes that carried the body to another channel; they settle no release.
    misdirected: Arc<Mutex<Vec<(u64, SinkOp)>>>,
}

impl BodyCheck {
    /// Watches `channel`'s adoption in this thread's forced snapshot.
    pub(crate) fn watch(channel: u64, body: &str) -> Self {
        let candidate = crate::services::tui_o::cutover::test_override::with_channels(|boot| {
            boot?.candidate(channel).cloned()
        })
        .expect("the watched channel has a forced candidate");
        assert!(
            !body.trim().is_empty(),
            "the watched body must be visible text"
        );
        candidate.bodiless.store(false, Ordering::SeqCst);
        Self {
            channel,
            candidate,
            body: body.trim().to_string(),
            early: Arc::default(),
            misdirected: Arc::default(),
        }
    }

    pub(crate) fn adoption(&self) -> Adoption {
        self.candidate.peek()
    }

    /// A sink is about to write `content` to `channel`. When it carries the body to the watched
    /// channel, the claim must already have released the adoption, and that release is settled.
    pub(crate) fn sink(&self, channel: u64, op: SinkOp, content: &str) {
        if !content.contains(&self.body) {
            return;
        }
        if channel != self.channel {
            self.misdirected.lock().unwrap().push((channel, op));
            return;
        }
        let state = self.candidate.peek();
        if state != Adoption::Released {
            self.early.lock().unwrap().push(state);
        }
        self.candidate.bodiless.store(false, Ordering::SeqCst);
    }

    /// An HTTP sink's request: the channel comes from its `/channels/{id}` path, and only a POST
    /// or PATCH writes. Any other request, or a path with no channel, is not a write.
    pub(crate) fn sink_request(&self, method: &str, path: &str, content: &str) {
        let op = match method {
            "POST" => SinkOp::Post,
            "PATCH" => SinkOp::Patch,
            _ => return,
        };
        let mut segments = path.split('/');
        let channel = segments.find(|segment| *segment == "channels");
        let channel = channel.and(segments.next()).and_then(|id| id.parse().ok());
        self.sink(channel.unwrap_or(0), op, content);
    }

    /// Whether a claim released the adoption and no sink has shown the body since.
    pub(crate) fn bodiless_release(&self) -> bool {
        self.candidate.bodiless.load(Ordering::SeqCst)
    }

    /// Scenario end: the body never left before its claim, and no claim released the adoption
    /// without the body following.
    pub(crate) fn assert_settled(&self) {
        let early = self.early.lock().unwrap().clone();
        assert!(
            early.is_empty(),
            "the body left before its claim, as {early:?}"
        );
        let misdirected = self.misdirected.lock().unwrap().clone();
        assert!(
            misdirected.is_empty(),
            "the body went to another channel: {misdirected:?}"
        );
        assert!(
            !self.bodiless_release(),
            "a claim released a pending adoption and no body was shown (adoption {:?})",
            self.adoption()
        );
    }
}
