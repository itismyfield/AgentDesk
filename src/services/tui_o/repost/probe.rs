//! Bounded history probe for one piece: two timed passes from the newest message down to the
//! piece's own anchor, each under a fresh permission proof. Reads only: no POST, slot or SQL.

#[path = "evidence.rs"]
pub(crate) mod evidence;
#[path = "matcher.rs"]
pub(crate) mod matcher;
#[cfg(test)]
#[path = "probe_tests.rs"]
pub(crate) mod tests;

use std::future::Future;

use tokio::time::{Duration, Instant};

use evidence::{
    CompletedPass, EvidenceScope, FIRST_PASS_AFTER, NotFoundEvidence, PermissionProof, RunScope,
    SECOND_PASS_AFTER,
};
use matcher::{Attribution, AttributionSnapshot, ObservedMessage, match_observations};

/// Bounds each read, rate-limit wait and body decode included.
pub(crate) const READ_TIMEOUT: Duration = Duration::from_secs(5);
/// Discord's history page limit.
pub(crate) const PAGE_LIMIT: u8 = 100;
/// Pages one `advance` reads before yielding the channel back as `Partial`.
pub(crate) const PAGES_PER_STEP: usize = 10;

/// Read-only Discord access. A failure or timeout is never an empty answer.
pub(crate) trait ProbeRead {
    /// Names the credentials both reads use, so a change discards the pass.
    fn credentials(&self) -> String;
    /// One message by id; `Ok(None)` is a 404.
    fn message(
        &self,
        channel: u64,
        id: u64,
    ) -> impl Future<Output = Result<Option<ObservedMessage>, String>> + Send;
    /// Up to `limit` messages older than `before`, or the newest when `None`.
    fn history(
        &self,
        channel: u64,
        before: Option<u64>,
        limit: u8,
    ) -> impl Future<Output = Result<Vec<ObservedMessage>, String>> + Send;
}

async fn bounded<T>(read: impl Future<Output = Result<T, String>>) -> Result<T, String> {
    let late = |_| Err(format!("read exceeded {READ_TIMEOUT:?}"));
    tokio::time::timeout(READ_TIMEOUT, read)
        .await
        .unwrap_or_else(late)
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Progress {
    /// Too early for the next pass.
    Waiting { until: Instant },
    /// The page budget ran out above the anchor; this session may resume the pass.
    Partial,
    /// The first pass covered the range and saw nothing of the piece.
    FirstPassDone { second_from: Instant },
    /// The range was covered and something of, or possibly of, the piece was seen.
    Present,
    /// A failed or late read, a page outside the cursor, no permission proof, or another run:
    /// the pass is dropped, and nothing is concluded.
    Incomplete(String),
    /// Two clean passes. Evidence for the grant to check, not a permit.
    Absent(Box<NotFoundEvidence>),
}

impl Progress {
    /// Whether the last pass read its whole range; found ids stand either way.
    pub(crate) fn covered(&self) -> bool {
        matches!(
            self,
            Self::FirstPassDone { .. } | Self::Present | Self::Absent(_)
        )
    }
}

/// Where an unfinished pass stands. It lives only in its session, so a restart or a new run
/// starts the pass over.
#[derive(Debug)]
pub(crate) struct PassCursor {
    number: u8,
    upper: Option<u64>,
    before: Option<u64>,
    started_at: Instant,
    proof: PermissionProof,
}

pub(crate) struct ProbeSession {
    scope: EvidenceScope,
    /// Known messages of the sender in this channel, the anchor first, to prove read permission.
    proof_ids: Vec<u64>,
    /// When the last send of the scope was seen settled, on this process's clock.
    settled_at: Instant,
    cursor: Option<PassCursor>,
    first: Option<CompletedPass>,
    seen: Attribution,
}

impl ProbeSession {
    pub(crate) fn new(scope: EvidenceScope, proof_ids: Vec<u64>, settled_at: Instant) -> Self {
        Self {
            scope,
            proof_ids,
            settled_at,
            cursor: None,
            first: None,
            seen: Attribution::default(),
        }
    }

    /// What this session has seen of the piece, kept across partial and failed pages.
    pub(crate) fn attribution(&self) -> &Attribution {
        &self.seen
    }

    /// Reads up to `PAGES_PER_STEP` pages of the current pass, starting one when it is due.
    pub(crate) async fn advance(
        &mut self,
        reader: &impl ProbeRead,
        run: &RunScope,
        snapshot: &AttributionSnapshot,
    ) -> Progress {
        if *run != self.scope.run || reader.credentials() != self.scope.run.credentials {
            self.cursor = None;
            self.first = None;
            return Progress::Incomplete("the run or credentials changed".into());
        }
        let cursor = match self.cursor.take() {
            Some(cursor) => cursor,
            None => match self.start(reader).await {
                Ok(cursor) => cursor,
                Err(progress) => return progress,
            },
        };
        self.scan(reader, snapshot, cursor).await
    }

    async fn start(&mut self, reader: &impl ProbeRead) -> Result<PassCursor, Progress> {
        let (number, due) = match &self.first {
            Some(first) => (2, first.completed_at() + SECOND_PASS_AFTER),
            None => (1, self.settled_at + FIRST_PASS_AFTER),
        };
        let started_at = Instant::now();
        if started_at < due {
            return Err(Progress::Waiting { until: due });
        }
        let channel = self.scope.key.unit().channel_id;
        for &id in &self.proof_ids {
            // A 404, another author or another channel proves nothing; try the next message.
            match bounded(reader.message(channel, id)).await {
                Ok(Some(read)) => {
                    if let Some(proof) = PermissionProof::verify(&self.scope, id, &read) {
                        let (upper, before) = (None, None);
                        return Ok(PassCursor {
                            number,
                            upper,
                            before,
                            started_at,
                            proof,
                        });
                    }
                }
                Ok(None) => {}
                Err(error) => return Err(Progress::Incomplete(error)),
            }
        }
        Err(Progress::Incomplete("read permission unproven".into()))
    }

    async fn scan(
        &mut self,
        reader: &impl ProbeRead,
        snapshot: &AttributionSnapshot,
        mut cursor: PassCursor,
    ) -> Progress {
        let channel = self.scope.key.unit().channel_id;
        let lower = self.scope.original_anchor;
        for _ in 0..PAGES_PER_STEP {
            let page = match bounded(reader.history(channel, cursor.before, PAGE_LIMIT)).await {
                Ok(page) => page,
                Err(error) => return Progress::Incomplete(error),
            };
            let outside = cursor
                .before
                .is_some_and(|before| page.iter().any(|message| message.id >= before));
            if outside || page.len() > usize::from(PAGE_LIMIT) {
                return Progress::Incomplete("a history page outside the cursor".into());
            }
            // The first page fixes the upper bound; later messages belong to the next pass.
            if cursor.before.is_none() {
                cursor.upper = page.iter().map(|message| message.id).max();
            }
            let upper = cursor.upper.unwrap_or(lower);
            let in_range = page
                .iter()
                .filter(|message| message.id > lower && message.id <= upper);
            match_observations(&self.scope, snapshot, in_range, &mut self.seen);
            let oldest = page.iter().map(|message| message.id).min();
            if page.len() < usize::from(PAGE_LIMIT) || oldest.is_some_and(|id| id <= lower) {
                return self.complete(cursor);
            }
            cursor.before = oldest;
            tokio::task::yield_now().await;
        }
        self.cursor = Some(cursor);
        Progress::Partial
    }

    fn complete(&mut self, cursor: PassCursor) -> Progress {
        let pass = CompletedPass::new(
            self.scope.clone(),
            cursor.number,
            cursor.upper,
            cursor.started_at,
            cursor.proof,
        );
        if !self.seen.is_clear() {
            self.first = None;
            return Progress::Present;
        }
        match self.first.take() {
            None => {
                let second_from = pass.completed_at() + SECOND_PASS_AFTER;
                self.first = Some(pass);
                Progress::FirstPassDone { second_from }
            }
            Some(first) => NotFoundEvidence::from_passes(self.settled_at, first, pass)
                .map(Box::new)
                .map_or_else(
                    || Progress::Incomplete("the passes do not pair".into()),
                    Progress::Absent,
                ),
        }
    }
}
