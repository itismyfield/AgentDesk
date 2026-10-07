//! Herdr's side of the native `/clear` helper: every check before the first change, one gated
//! `/clear` line, and Hold wherever tmux would reset; nothing here resets or kills a pane.

use std::future::Future;
use std::pin::Pin;

use tokio::sync::watch;
use tokio::time::Instant;

use super::herdr_gate::{HerdrGateRefusal, HerdrTarget, Mutation};
use super::model::HostMutation;
use crate::db::dispatched_sessions::hosted_execution::{
    HostedLookup, HostedLookupKey, HostedRecord, HostedState, load_hosted_execution_pg,
};
use crate::services::claude::herdr_turn::input_held;
use crate::services::claude_tui::host_input::{
    MutationGate, NativeClearSubmission, native_clear_composer_empty,
};
use crate::services::tui_prompt_dedupe::native_clear::{
    CanonicalClearWaiter, ClearCommit, ClearDecision, ClearTicket, NativeClearHost,
    capture_live_clear,
};

type Effect<'a> = Pin<Box<dyn Future<Output = bool> + Send + 'a>>;

/// Why a Herdr channel's `!clear` changed nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum HerdrClearRefusal {
    /// The row holds no Bound execution: its state, `None` without a readable one.
    NotBound(Option<HostedState>),
    /// O still reads this many sources an earlier rotation left.
    RotationUnsettled(usize),
    /// No running O actor reported the channel's rotation.
    RotationUnread,
    /// An earlier prompt may sit unsubmitted in the composer; only a retire ends that hold.
    InputHeld,
    HoldUnreadable(String),
    /// The execution is not on an endpoint this node registered.
    NotRegistered,
    Gate(HerdrGateRefusal),
    /// The running execution, its launch context or its pinned source was not identified.
    SourceUnidentified,
    /// No database holds the clear boundary and the cleared session.
    NoDatabase,
    NoSessionKey,
    /// A turn still runs: no Herdr stop reaches its pane, so the clear would race its output.
    TurnInProgress,
}

impl std::fmt::Display for HerdrClearRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("herdr clear refused: ")?;
        match self {
            Self::NotBound(state) => write!(f, "not_bound(state={state:?})"),
            Self::RotationUnsettled(count) => write!(f, "rotation_unsettled({count})"),
            Self::RotationUnread => f.write_str("rotation_unread"),
            Self::InputHeld => f.write_str("input_held"),
            Self::HoldUnreadable(detail) => write!(f, "hold_unreadable({detail})"),
            Self::NotRegistered => f.write_str("not_registered"),
            Self::Gate(refusal) => write!(f, "gate({refusal:?})"),
            Self::SourceUnidentified => f.write_str("source_unidentified"),
            Self::NoDatabase => f.write_str("no_database"),
            Self::NoSessionKey => f.write_str("no_session_key"),
            Self::TurnInProgress => f.write_str("turn_in_progress"),
        }
    }
}

/// The channel session's part of a clear, owned by the caller: the provider selector cleared
/// durably before `/clear`, then the cleared session saved and its boundary resolved.
pub(crate) trait ClearSession: Send + 'static {
    fn clear_selector(&mut self) -> Effect<'_>;
    /// `false` keeps the clear on Hold.
    fn save(&mut self, commit: ClearCommit) -> Effect<'_>;
    fn finish(&mut self, commit: ClearCommit) -> Effect<'_>;
}

/// A Bound execution's clear, judged before anything changed.
pub(crate) struct HerdrClearPlan {
    target: HerdrTarget,
    nonce: String,
    waiter: CanonicalClearWaiter,
}

impl HerdrClearPlan {
    pub(crate) fn waiter(&self) -> &CanonicalClearWaiter {
        &self.waiter
    }
}

/// Judges `!clear` on a Bound row: row, O's rotation and input hold before any I/O, then E7 and the
/// pane on one server, then the canonical baseline. Blocks on the socket, off the async runtime.
pub(crate) fn plan_clear(
    channel: u64,
    row: Option<&HostedRecord>,
    unsettled: Option<usize>,
) -> Result<HerdrClearPlan, HerdrClearRefusal> {
    let record = match row {
        Some(HostedRecord::Known(record)) if record.state == HostedState::Bound => record,
        Some(HostedRecord::Known(record)) => {
            return Err(HerdrClearRefusal::NotBound(Some(record.state)));
        }
        _ => return Err(HerdrClearRefusal::NotBound(None)),
    };
    match unsettled {
        Some(0) => {}
        Some(count) => return Err(HerdrClearRefusal::RotationUnsettled(count)),
        None => return Err(HerdrClearRefusal::RotationUnread),
    }
    not_held(&record.execution_nonce)?;
    let registry = super::herdr_registry::registry();
    let target = registry
        .target(record)
        .ok_or(HerdrClearRefusal::NotRegistered)?;
    target
        .pin(Mutation::Input)
        .map_err(HerdrClearRefusal::Gate)?;
    target.discard_pin();
    let logical = &record.owner.logical_key;
    let waiter =
        capture_live_clear(channel, logical).ok_or(HerdrClearRefusal::SourceUnidentified)?;
    if waiter.ticket.context.execution_nonce != record.execution_nonce {
        return Err(HerdrClearRefusal::SourceUnidentified);
    }
    let nonce = record.execution_nonce.clone();
    Ok(HerdrClearPlan {
        target,
        nonce,
        waiter,
    })
}

fn not_held(nonce: &str) -> Result<(), HerdrClearRefusal> {
    match input_held(nonce) {
        Ok(false) => Ok(()),
        Ok(true) => Err(HerdrClearRefusal::InputHeld),
        Err(detail) => Err(HerdrClearRefusal::HoldUnreadable(detail)),
    }
}

fn composer_empty_now(target: &HerdrTarget) -> bool {
    target
        .capture(-80)
        .is_some_and(|capture| native_clear_composer_empty(&capture))
}

/// Reads the committed clear's recorded execution without planning or sending another clear.
pub(crate) async fn recovery_composer_empty(
    pool: &sqlx::PgPool,
    session_key: &str,
    channel: u64,
    ticket: &ClearTicket,
    deadline: Instant,
) -> bool {
    tokio::time::timeout_at(deadline, async {
        let lookup = load_hosted_execution_pg(pool, HostedLookupKey::SessionKey(session_key)).await;
        let HostedLookup::Found(observation) = lookup else {
            return false;
        };
        let HostedRecord::Known(record) = observation.record else {
            return false;
        };
        let context = &ticket.context;
        if record.state != HostedState::Bound
            || context.channel_id != Some(channel)
            || record.owner.channel_id != channel.to_string()
            || record.owner.provider != context.provider
            || record.owner.logical_key != context.tmux_session
            || record.execution_nonce != context.execution_nonce
        {
            return false;
        }
        let Some(target) = super::herdr_registry::registry().target(&record) else {
            return false;
        };
        tokio::task::spawn_blocking(move || target.execution_alive() && composer_empty_now(&target))
            .await
            .unwrap_or(false)
    })
    .await
    .unwrap_or(false)
}

/// The helper's host for one planned Herdr clear.
pub(crate) struct HerdrClear<S> {
    plan: HerdrClearPlan,
    session: S,
}

impl<S: ClearSession> HerdrClear<S> {
    pub(crate) fn new(plan: HerdrClearPlan, session: S) -> Self {
        Self { plan, session }
    }

    fn composer_empty_now(&self) -> bool {
        composer_empty_now(&self.plan.target)
    }
}

impl<S: ClearSession> NativeClearHost for HerdrClear<S> {
    fn changes(&self) -> watch::Receiver<u64> {
        self.plan.waiter.changed.clone()
    }

    fn prepare(&mut self, _deadline: Instant) -> Effect<'_> {
        self.session.clear_selector()
    }

    /// One `/clear` line through the gate, only into an empty composer of the ticket's execution
    /// with no hold; never sent again.
    fn submit(&mut self, deadline: Instant) -> NativeClearSubmission {
        if not_held(&self.plan.nonce).is_err() || !self.composer_empty_now() {
            return NativeClearSubmission::NotSent;
        }
        let ticket = &self.plan.waiter.ticket;
        let logical = ticket.context.tmux_session.as_str();
        if Instant::now() >= deadline || ticket.admit(logical).is_err() {
            return NativeClearSubmission::NotSent;
        }
        if self.plan.target.pin(Mutation::Input).is_err() {
            return NativeClearSubmission::NotSent;
        }
        crate::services::tui_prompt_dedupe::record_discord_originated_prompt(
            "claude", logical, "/clear",
        );
        match self.plan.target.send_line("/clear") {
            Ok(HostMutation::Confirmed) if self.composer_empty_now() => {
                NativeClearSubmission::Confirmed
            }
            Ok(HostMutation::Confirmed | HostMutation::Indeterminate(_)) => {
                NativeClearSubmission::Indeterminate
            }
            Ok(HostMutation::Refused(_)) | Err(_) => NativeClearSubmission::NotSent,
        }
    }

    /// Only the canonical log decides; Herdr has no fallback cut, so a missing commit is a Hold.
    fn decide(&mut self, _allow_fallback: bool) -> ClearDecision {
        match self.plan.waiter.probe() {
            ClearDecision::Fallback => ClearDecision::Hold(None),
            decision => decision,
        }
    }

    fn composer_empty(&mut self, _deadline: Instant) -> bool {
        self.composer_empty_now()
    }

    fn save(&mut self, commit: ClearCommit, _deadline: Instant) -> Effect<'_> {
        self.session.save(commit)
    }

    fn finish(&mut self, commit: ClearCommit, _deadline: Instant) -> Effect<'_> {
        self.session.finish(commit)
    }

    /// The bounded Hold: no reset, kill or tmux call, and no second `/clear`.
    fn fallback(&mut self, _deadline: Instant) -> Effect<'_> {
        Box::pin(async { false })
    }
}

#[cfg(test)]
#[path = "herdr_clear_adapter_tests.rs"]
mod tests;
