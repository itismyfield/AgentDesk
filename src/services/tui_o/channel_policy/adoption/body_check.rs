//! Test-only check that a selected channel's held body never leaves through Legacy: no sink write
//! carries it while O has not adopted the channel, and the body path leaves the adoption as it was.

use std::sync::{Arc, Mutex};

use super::{Adoption, Candidate};

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
    candidate: Candidate,
    body: String,
    start: Adoption,
    /// Writes that carried the body, with the adoption each found.
    leaked: Arc<Mutex<Vec<(u64, SinkOp, Adoption)>>>,
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
        Self {
            start: candidate.peek(),
            candidate,
            body: body.trim().to_string(),
            leaked: Arc::default(),
        }
    }

    pub(crate) fn adoption(&self) -> Adoption {
        self.candidate.peek()
    }

    /// A sink is about to write `content` to `channel`; carrying the watched body is a leak.
    pub(crate) fn sink(&self, channel: u64, op: SinkOp, content: &str) {
        if content.contains(&self.body) {
            let state = self.candidate.peek();
            self.leaked.lock().unwrap().push((channel, op, state));
        }
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

    /// Scenario end: no write carried the held body, and the adoption is still the one watched.
    pub(crate) fn assert_settled(&self) {
        let leaked = self.leaked.lock().unwrap().clone();
        assert!(leaked.is_empty(), "the held body left: {leaked:?}");
        assert_eq!(
            self.adoption(),
            self.start,
            "the body path changed the adoption"
        );
    }
}

/// A Legacy body send racing a writer test: on an unadopted channel it is counted by a direct
/// claim, as the gate holds such a body; an adopted or unselected channel answers through the gate.
pub(crate) async fn racing_legacy_body<T, F, Fut>(channel: u64, send: F) -> RacingSend<T>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = T>,
{
    use crate::services::tui_o::cutover::{self, BodyClaim, BodySend};
    let (candidate, kind) = cutover::test_override::with_channels(|boot| {
        let boot = boot?;
        Some((boot.candidate(channel).cloned(), boot.kind(channel)))
    })
    .expect("a forced snapshot");
    let Some(candidate) = candidate.filter(|candidate| !candidate.peek().owned()) else {
        return cutover::claim_then_send(Some(BodyClaim::new(channel, kind)), send).await;
    };
    let claimed = candidate.claim_body(channel);
    let sent = send().await;
    drop(claimed);
    Ok(BodySend::Sent(sent))
}

type RacingSend<T> = Result<
    crate::services::tui_o::cutover::BodySend<T>,
    crate::services::tui_o::cutover::IdentityError,
>;
