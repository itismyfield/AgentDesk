//! Whether this process may still adopt a selected channel into O. A state changes only under the
//! channel's own lock and is decided once from Pending or Deferred; it cancels or records an
//! adoption, never a delivery.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};

#[cfg(test)]
pub(crate) mod body_check;

use crate::services::tui_o::store::OStore;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Adoption {
    /// Undecided: a Legacy body or placement releases it, a first `init` commits it.
    Pending,
    /// The channel's store exists, so O owns its output.
    Committed,
    /// Store state is damaged or may be public unconfirmed: O keeps the output and holds it.
    Held,
    /// Legacy keeps the channel for the rest of this process.
    Released,
    /// Legacy keeps the channel while its writer host waits for a turn to close and retries;
    /// only that host sets or leaves it, and a Legacy body does not.
    Deferred,
}

impl Adoption {
    pub(crate) fn owned(self) -> bool {
        matches!(self, Self::Committed | Self::Held)
    }
}

/// Legacy body sends a candidate let through: `started` moves under the candidate lock as a body
/// claim passes, `finished` as that send's guard drops.
#[derive(Debug, Default)]
struct LegacySends {
    started: AtomicU64,
    finished: AtomicU64,
}

/// A Legacy body send under way on its channel, held until its transport is done.
#[derive(Debug)]
pub(crate) struct LegacySend(Arc<LegacySends>);

impl Drop for LegacySend {
    fn drop(&mut self) {
        self.0.finished.fetch_add(1, Ordering::SeqCst);
    }
}

/// A body claim's answer: whether O owns the channel and, when Legacy sends, that send.
#[derive(Debug)]
pub(crate) struct BodyClaimed {
    pub(crate) owned: bool,
    pub(crate) send: Option<LegacySend>,
}

/// A sends snapshot taken before reading the binding log; order is local to this candidate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct TurnMark {
    pub(crate) sends: (u64, u64),
    pub(crate) seq: u64,
    pub(crate) order: u64,
}

/// One selected channel's adoption and the lock every transition of it takes.
#[derive(Clone, Debug)]
pub(crate) struct Candidate {
    state: Arc<Mutex<Adoption>>,
    sends: Arc<LegacySends>,
    /// Immutable boot origin, shared by every clone, distinct from ordinary Deferred retries.
    late_origin: bool,
    /// The binding seq a late reservation baselines on: fixed by its first host's first read and
    /// kept for every host that takes the reservation over.
    late_boundary: Arc<OnceLock<u64>>,
    turn_mark: Arc<Mutex<Option<TurnMark>>>,
    baseline_order: Arc<AtomicU64>,
    /// Test builds: set when a claim released a pending adoption, cleared once a sink saw a body.
    #[cfg(test)]
    bodiless: Arc<std::sync::atomic::AtomicBool>,
}

impl Candidate {
    pub(crate) fn new(state: Adoption) -> Self {
        Self {
            state: Arc::new(Mutex::new(state)),
            sends: Arc::default(),
            late_origin: false,
            late_boundary: Arc::default(),
            turn_mark: Arc::default(),
            baseline_order: Arc::default(),
            #[cfg(test)]
            bodiless: Arc::default(),
        }
    }

    pub(crate) fn reserved_late() -> Self {
        Self {
            late_origin: true,
            ..Self::new(Adoption::Deferred)
        }
    }

    pub(crate) fn late_adopt_origin(&self) -> bool {
        self.late_origin
    }

    /// Callers hold the adoption lock, so a body claim and its baseline have a fixed order.
    pub(crate) fn baseline_sends(&self) -> ((u64, u64), u64) {
        let order = self.baseline_order.fetch_add(1, Ordering::SeqCst) + 1;
        (self.sends(), order)
    }

    pub(crate) fn turn_mark(&self) -> Option<TurnMark> {
        *self
            .turn_mark
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Admission never waits for adoption; a failed log read leaves the host's baseline in use.
    pub(crate) fn mark_turn_admitted(
        &self,
        channel: u64,
        read_seq: impl FnOnce() -> Result<u64, String>,
    ) -> bool {
        let (sends, order) = {
            let state = self.lock();
            if *state != Adoption::Deferred || !self.late_origin {
                return false;
            }
            *self
                .turn_mark
                .lock()
                .unwrap_or_else(PoisonError::into_inner) = None;
            self.baseline_sends()
        };
        // The log may bind a rollout during this read; that rollout is already behind the mark.
        let seq = match read_seq() {
            Ok(seq) => seq,
            Err(detail) => {
                tracing::warn!(channel, order, detail, "[tui_o] Codex turn mark discarded");
                return false;
            }
        };
        let state = self.lock();
        if *state != Adoption::Deferred || self.baseline_order.load(Ordering::SeqCst) != order {
            return false;
        }
        let mark = TurnMark { sends, seq, order };
        *self
            .turn_mark
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(mark);
        tracing::info!(channel, ?mark, "[tui_o] Codex fresh turn admitted");
        true
    }

    /// The late reservation's binding boundary, once fixed.
    pub(crate) fn late_boundary(&self) -> Option<u64> {
        self.late_boundary.get().copied()
    }

    /// Fixes the boundary `judge` returns unless one is fixed already, and returns the fixed one.
    pub(crate) fn fix_late_boundary(&self, judge: impl FnOnce() -> u64) -> u64 {
        *self.late_boundary.get_or_init(judge)
    }

    pub(crate) fn late_cleanup_guard(
        &self,
        channel: u64,
        runtime_root: Option<PathBuf>,
    ) -> Option<LateReservationGuard> {
        self.late_origin.then(|| LateReservationGuard {
            candidate: self.clone(),
            channel,
            runtime_root,
            keep: Arc::default(),
        })
    }

    pub(crate) fn lock(&self) -> MutexGuard<'_, Adoption> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    #[cfg(test)]
    pub(crate) fn locked_for_test(&self) -> bool {
        matches!(
            self.state.try_lock(),
            Err(std::sync::TryLockError::WouldBlock)
        )
    }

    /// Read without deciding anything, for callers that carry no body.
    pub(crate) fn peek(&self) -> Adoption {
        *self.lock()
    }

    /// Whether O owns the channel for a placement, which carries no body; a pending adoption is
    /// released first. A placement is not a Legacy send, so it is not counted.
    pub(in crate::services::tui_o) fn claim(&self, channel: u64) -> bool {
        self.take(channel).owned()
    }

    /// Whether O owns the channel for a body Legacy would otherwise send, releasing a pending
    /// adoption first; a Legacy send is counted under this lock until its guard drops.
    #[track_caller]
    pub(in crate::services::tui_o) fn claim_body(&self, channel: u64) -> BodyClaimed {
        self.claim_body_from(channel, std::panic::Location::caller())
    }

    pub(in crate::services::tui_o) fn claim_body_from(
        &self,
        channel: u64,
        origin: &'static std::panic::Location<'static>,
    ) -> BodyClaimed {
        let state = self.take(channel);
        let owned = state.owned();
        #[cfg(test)]
        if !owned {
            crate::services::tui_o::writer::activation::test_hook::run(
                channel,
                crate::services::tui_o::writer::activation::test_hook::Step::BeforeLegacyStarted,
            )
            .expect("Legacy start hook");
        }
        let send = (!owned).then(|| {
            self.sends.started.fetch_add(1, Ordering::SeqCst);
            LegacySend(Arc::clone(&self.sends))
        });
        let (started, finished) = self.sends();
        let mark = self.turn_mark();
        tracing::info!(channel, owned, started, finished, %origin,
            mark_present = mark.is_some(), mark_order = mark.map(|m| m.order),
            mark_used = mark.is_some_and(|m| m.order == self.baseline_order.load(Ordering::SeqCst)),
            mark_seq = mark.map(|m| m.seq), mark_sends = ?mark.map(|m| m.sends),
            "[tui_o] body claim");
        drop(state);
        BodyClaimed { owned, send }
    }

    fn take(&self, channel: u64) -> MutexGuard<'_, Adoption> {
        let mut state = self.lock();
        if *state == Adoption::Pending {
            *state = Adoption::Released;
            #[cfg(test)]
            body_check::note_release(self);
            tracing::info!(channel, "[tui_o] Legacy took the channel before O adoption");
        }
        state
    }

    /// Legacy body sends this channel started and finished so far; callers hold the state lock.
    pub(crate) fn sends(&self) -> (u64, u64) {
        let sends = &self.sends;
        let started = sends.started.load(Ordering::SeqCst);
        (started, sends.finished.load(Ordering::SeqCst))
    }

    /// Leaves a pending or deferred adoption to Legacy for the rest of this process; a decided
    /// one is kept.
    pub(crate) fn release(&self, channel: u64) {
        let mut state = self.lock();
        if matches!(*state, Adoption::Pending | Adoption::Deferred) {
            *state = Adoption::Released;
            tracing::info!(channel, "[tui_o] adoption released before the first init");
        }
    }

    /// Leaves a pending adoption to Legacy until its host retries; true while it is deferred.
    pub(crate) fn defer(&self, channel: u64) -> bool {
        let mut state = self.lock();
        match *state {
            Adoption::Pending => {
                *state = Adoption::Deferred;
                tracing::info!(
                    channel,
                    "[tui_o] adoption deferred until Legacy's turn closes"
                );
                true
            }
            Adoption::Deferred => true,
            Adoption::Committed | Adoption::Held | Adoption::Released => false,
        }
    }

    /// A recovered store commits the adoption, unless Legacy already took the channel.
    pub(crate) fn confirm_store(&self) -> bool {
        let mut state = self.lock();
        if *state == Adoption::Released {
            return false;
        }
        *state = Adoption::Committed;
        true
    }
}

/// Owned by the sole claimed host before its future is spawned.
pub(crate) struct LateReservationGuard {
    candidate: Candidate,
    channel: u64,
    runtime_root: Option<PathBuf>,
    /// Set before a stop whose successor host takes the reservation over, so this one keeps it.
    keep: Arc<AtomicBool>,
}

impl LateReservationGuard {
    /// Leaves the reservation as it is when `keep` was set by the time the host ends.
    pub(crate) fn kept_by(mut self, keep: Arc<AtomicBool>) -> Self {
        self.keep = keep;
        self
    }
}

impl Drop for LateReservationGuard {
    fn drop(&mut self) {
        #[cfg(test)]
        let kept = self.keep.load(Ordering::SeqCst)
            && std::env::var("ADK_S3B_MUTANT").ok().as_deref() != Some("stop_settles_reservation");
        #[cfg(not(test))]
        let kept = self.keep.load(Ordering::SeqCst);
        if kept {
            return;
        }
        let mut state = self.candidate.lock();
        if !matches!(*state, Adoption::Pending | Adoption::Deferred) {
            return;
        }
        let settled = self
            .runtime_root
            .as_deref()
            .and_then(OStore::existing)
            .map_or(Adoption::Released, |store| {
                crate::services::tui_o::writer::activation::settled(&store, self.channel)
            });
        *state = settled;
        tracing::info!(channel = self.channel, adoption = ?*state, "[tui_o] late reservation settled after host exit");
    }
}

/// Where this node stands for the selected channels: only the O home hosts or adopts them.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) enum Site {
    #[default]
    Home,
    Foreign {
        home: String,
    },
}

/// `stored` over the selection and every channel the local store committed: an `init` entry or
/// an era member. The whole store is searched before the selection joins it.
pub(crate) fn stored_with_committed(
    runtime_root: Option<&Path>,
    selected: &BTreeSet<u64>,
) -> std::io::Result<BTreeMap<u64, Adoption>> {
    let committed = match runtime_root.and_then(OStore::existing) {
        Some(store) => {
            let mut committed = store.channels_with_init()?;
            if let Ok(Some(era)) = store.read_era() {
                committed.extend(era.initial_channels);
            }
            committed
        }
        None => BTreeSet::new(),
    };
    let channels = selected.union(&committed).copied().collect();
    Ok(stored(runtime_root, &channels))
}

/// The adoption each selected channel starts with, from the local store as it is on disk.
/// A readable `init` commits; unreadable, orphaned or era-only state holds; absent is pending.
pub(crate) fn stored(
    runtime_root: Option<&Path>,
    channels: &BTreeSet<u64>,
) -> BTreeMap<u64, Adoption> {
    let all = |state| channels.iter().map(|&channel| (channel, state)).collect();
    let Some(root) = runtime_root else {
        return all(Adoption::Held);
    };
    let Some(store) = OStore::existing(root) else {
        return all(Adoption::Pending);
    };
    let era = store.read_era();
    let judged = |&channel: &u64| {
        let state = match store.read_init(channel) {
            Ok(Some(_)) => Adoption::Committed,
            Err(_) => Adoption::Held,
            Ok(None) => match &era {
                Ok(Some(era)) if era.initial_channels.contains(&channel) => Adoption::Held,
                Ok(_) if !store.has_channel_dir(channel) => Adoption::Pending,
                _ => Adoption::Held,
            },
        };
        (channel, state)
    };
    channels.iter().map(judged).collect()
}
