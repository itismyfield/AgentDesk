//! Delivery and terminal replay for a verified agy seal. The channel actor must opt in explicitly.

#[cfg(all(test, unix))]
mod agy_delivery_tests;
pub(crate) mod plan;

use std::future::{Future, poll_fn};
use std::num::NonZeroU64;
use std::task::Poll;

pub(crate) use plan::{Piece, Plan, hash};

use crate::services::tui_o::ownership::OwnershipGate;
pub(crate) use crate::services::tui_o::store::agy_delivery::TurnLease;
use crate::services::tui_o::store::agy_delivery::{Attempt, Journal, Receipt, Stage, State};
use crate::services::tui_o::store::{ChannelStore, StoreError};

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Progress {
    More,
    Complete,
    Held(String),
}

#[derive(Debug)]
pub(crate) enum Error {
    Authority,
    Store(StoreError),
}

impl From<StoreError> for Error {
    fn from(error: StoreError) -> Self {
        Self::Store(error)
    }
}

pub(crate) enum Post {
    Created(NonZeroU64),
    NotSent(String),
    Unknown,
}

pub(crate) struct Evidence {
    pub id: NonZeroU64,
    pub channel: u64,
    pub bot: u64,
    pub content: String,
    pub correlation: Option<String>,
}

pub(crate) trait Transport {
    fn bot(&self) -> Option<NonZeroU64>;
    fn post(&self, channel: u64, piece: &Piece) -> impl Future<Output = Post> + Send;
    fn history_before(
        &self,
        channel: u64,
        before: Option<NonZeroU64>,
    ) -> impl Future<Output = Result<Vec<Evidence>, String>> + Send;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Effect {
    ClearInflight,
    ReleaseLease,
    WakeQueue,
}

/// Idempotent against the supplied full turn key and lease revision. A missing projection
/// counts as settled only after checking identity. Queue wakes may be coalesced or replayed.
pub(crate) trait Projection {
    fn apply(
        &self,
        lease: TurnLease,
        effect: Effect,
    ) -> impl Future<Output = Result<(), String>> + Send;
}

pub(crate) struct Replay<'a> {
    gate: &'a OwnershipGate,
    epoch: u64,
    lease: TurnLease,
    plan: &'a Plan,
    journal: Journal<'a>,
}

impl<'a> Replay<'a> {
    pub(crate) fn open(
        store: &'a mut ChannelStore,
        gate: &'a OwnershipGate,
        epoch: u64,
        lease: TurnLease,
        plan: &'a Plan,
    ) -> Result<Self, Error> {
        Self::open_journal(store, gate, epoch, plan, Some(lease))
    }

    /// Reopens a durable obligation after its original mailbox row or lease has been cleared.
    pub(crate) fn recover(
        store: &'a mut ChannelStore,
        gate: &'a OwnershipGate,
        epoch: u64,
        plan: &'a Plan,
    ) -> Result<Self, Error> {
        Self::open_journal(store, gate, epoch, plan, None)
    }

    fn open_journal(
        store: &'a mut ChannelStore,
        gate: &'a OwnershipGate,
        epoch: u64,
        plan: &'a Plan,
        lease: Option<TurnLease>,
    ) -> Result<Self, Error> {
        let journal = gate
            .admit(|current| (current == epoch).then(|| Journal::open(store, plan.stamp(), lease)))
            .flatten()
            .ok_or(Error::Authority)?;
        let journal = journal?;
        Ok(Self {
            gate,
            epoch,
            lease: journal.lease(),
            plan,
            journal,
        })
    }

    pub(crate) fn confirmed_bytes(&self) -> usize {
        let end = self.journal.state().receipts.len();
        self.plan.body[..end]
            .last()
            .map_or(0, |piece| piece.source.end)
    }

    /// Runs one durable transition. Reopen after a Store or Authority error.
    pub(crate) async fn advance(
        &mut self,
        port: &impl Transport,
        gate_lease: &impl super::DeliveryLease,
        projection: &impl Projection,
    ) -> Result<Progress, Error> {
        if self.gate.current().epoch() != Some(self.epoch) {
            return Err(Error::Authority);
        }
        let Some(bot) = port.bot() else {
            return Ok(Progress::Held(
                "agy delivery bot identity is missing".into(),
            ));
        };
        let match_bot = |receipt: &Receipt| receipt.bot == bot;
        let state = self.journal.state();
        if !state
            .receipts
            .iter()
            .chain(state.footer.iter())
            .all(match_bot)
        {
            return Ok(Progress::Held(
                "agy delivery bot differs from saved receipts".into(),
            ));
        }
        match state.ended {
            None => {
                let index = state.receipts.len();
                if let Some(piece) = self.plan.body.get(index) {
                    return self
                        .deliver(port, gate_lease, piece, index as u64, false, bot)
                        .await;
                }
                self.commit(|state| state.ended = Some(Stage::Footer))
            }
            Some(Stage::Footer) => {
                let serial = self.plan.body.len() as u64;
                self.deliver(port, gate_lease, &self.plan.footer, serial, true, bot)
                    .await
            }
            Some(Stage::Inflight) => {
                self.project(projection, Effect::ClearInflight, Stage::Release)
                    .await
            }
            Some(Stage::Release) => {
                self.project(projection, Effect::ReleaseLease, Stage::Queue)
                    .await
            }
            Some(Stage::Queue) => {
                self.project(projection, Effect::WakeQueue, Stage::Complete)
                    .await
            }
            Some(Stage::Complete) => Ok(Progress::Complete),
        }
    }

    async fn deliver(
        &mut self,
        port: &impl Transport,
        lease: &impl super::DeliveryLease,
        piece: &Piece,
        serial: u64,
        footer: bool,
        bot: NonZeroU64,
    ) -> Result<Progress, Error> {
        let channel = self.plan.key.destination.get();
        let Some(_held) = lease.try_acquire(channel, serial) else {
            return Ok(Progress::Held("agy delivery lease busy".into()));
        };
        let message = match self.journal.state().attempt {
            None => return self.commit(|state| state.attempt = Some(Attempt::Prepared)),
            Some(Attempt::Prepared) => {
                let gate = self.gate;
                let epoch = self.epoch;
                let post = guarded(
                    gate,
                    epoch,
                    async { port.post(channel, piece).await },
                    || {
                        Ok(self
                            .journal
                            .commit(|state| state.attempt = Some(Attempt::HandedOff))?)
                    },
                );
                let outcome = tokio::time::timeout(super::deliver::POST_TIMEOUT, post)
                    .await
                    .unwrap_or(Ok(Post::Unknown))?;
                match outcome {
                    Post::Created(id) => id,
                    Post::NotSent(reason) => {
                        self.commit(|state| state.attempt = Some(Attempt::Prepared))?;
                        return Ok(Progress::Held(reason));
                    }
                    Post::Unknown => {
                        return Ok(Progress::Held("agy POST awaits exact confirmation".into()));
                    }
                }
            }
            Some(Attempt::HandedOff) => match self.confirm(port, piece, bot).await {
                Ok(id) => id,
                Err(reason) => return Ok(Progress::Held(reason)),
            },
        };
        let state = self.journal.state();
        if state
            .receipts
            .iter()
            .any(|receipt| receipt.message == message)
        {
            return Ok(Progress::Held(
                "agy receipt belongs to an earlier piece".into(),
            ));
        }
        self.commit(|state| {
            let receipt = Receipt { message, bot };
            state.attempt = None;
            if footer {
                state.footer = Some(receipt);
                state.ended = Some(Stage::Inflight);
            } else {
                state.receipts.push(receipt);
            }
        })
    }

    async fn confirm(
        &self,
        port: &impl Transport,
        piece: &Piece,
        bot: NonZeroU64,
    ) -> Result<NonZeroU64, String> {
        let channel = self.plan.key.destination.get();
        let after = self
            .journal
            .state()
            .receipts
            .last()
            .map_or(self.plan.key.id.get(), |receipt| receipt.message.get());
        let mut before = None;
        let mut candidate = None;
        for _ in 0..super::confirm::MAX_PAGES {
            let page = super::confirm::bounded(port.history_before(channel, before)).await?;
            for evidence in page.iter().filter(|evidence| evidence.id.get() > after) {
                if evidence.channel == channel
                    && evidence.bot == bot.get()
                    && evidence.correlation.as_deref() == Some(piece.correlation.as_str())
                    && (hash(&evidence.content) != piece.payload_hash
                        || candidate.replace(evidence.id).is_some())
                {
                    return Err("agy receipt correlation conflict".into());
                }
            }
            let oldest = page.iter().map(|evidence| evidence.id).min();
            if page.len() < super::confirm::HISTORY_PAGE
                || oldest.is_some_and(|id| id.get() <= after)
            {
                return candidate.ok_or_else(|| "agy POST remains unconfirmed".into());
            }
            before = Some(
                oldest
                    .filter(|&oldest| before.is_none_or(|before| oldest < before))
                    .ok_or("agy history did not advance")?,
            );
        }
        Err("agy confirmation history exceeded page limit".into())
    }

    async fn project(
        &mut self,
        target: &impl Projection,
        effect: Effect,
        next: Stage,
    ) -> Result<Progress, Error> {
        let result = guarded(
            self.gate,
            self.epoch,
            async { target.apply(self.lease, effect).await },
            || Ok(()),
        )
        .await?;
        match result {
            Ok(()) => self.commit(|state| state.ended = Some(next)),
            Err(reason) => Ok(Progress::Held(reason)),
        }
    }

    fn commit(&mut self, update: impl FnOnce(&mut State)) -> Result<Progress, Error> {
        self.gate
            .admit(|current| (current == self.epoch).then(|| self.journal.commit(update)))
            .flatten()
            .ok_or(Error::Authority)??;
        Ok(Progress::More)
    }
}

async fn guarded<T>(
    gate: &OwnershipGate,
    epoch: u64,
    request: impl Future<Output = T>,
    before: impl FnOnce() -> Result<(), Error>,
) -> Result<T, Error> {
    let mut updates = gate.subscribe();
    let mut before = Some(before);
    let mut request = std::pin::pin!(request);
    let run = poll_fn(|cx| {
        gate.admit(|current| {
            if current != epoch {
                return Poll::Ready(Err(Error::Authority));
            }
            if let Some(before) = before.take() {
                before()?;
            }
            request.as_mut().poll(cx).map(Ok)
        })
        .unwrap_or(Poll::Ready(Err(Error::Authority)))
    });
    tokio::select! {
        biased;
        _ = updates.changed() => Err(Error::Authority),
        result = run => result,
    }
}
