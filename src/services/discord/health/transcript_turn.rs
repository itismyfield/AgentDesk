//! Health projection for channels whose input moved to the ledger: the input supervisor's
//! published view, its drain-eligibility clock, and three-valued Legacy residue observations.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use poise::serenity_prelude::ChannelId;
use serde::Serialize;

use super::ProviderEntry;
use super::legacy_supervision;
use crate::services::discord::{
    abandon_request_store, busy_followup_retry_store, status_panel_orphan_store,
    tui_direct_abort_marker, turn_view_reconciler,
};
use crate::services::provider::ProviderKind;
use crate::services::turn_orchestrator::ChannelMailboxSnapshot;

// Policy values; the sweep freshness window is three placeholder-sweeper intervals.
const NOT_DRAINING_AFTER: Duration = Duration::from_secs(300);
const VIEW_STALE_AFTER: Duration = Duration::from_secs(120);
const UNKNOWN_DEGRADES_AFTER: Duration = Duration::from_secs(600);
const SWEEP_FRESHNESS: Duration = Duration::from_secs(90);

/// One observation of a residue kind; `Absent` only when the observer read and found none.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::services::discord) enum Presence {
    Present(usize),
    Absent,
    Unknown(&'static str),
}

impl Presence {
    fn counted(count: usize) -> Self {
        match count {
            0 => Self::Absent,
            n => Self::Present(n),
        }
    }

    /// Counts the records of one file; a missing file is a confirmed absence.
    pub(in crate::services::discord) fn of_file(
        path: Option<PathBuf>,
        count: impl FnOnce(&str) -> usize,
    ) -> Self {
        match path.map(std::fs::read_to_string) {
            None => Self::Unknown("no_runtime_root"),
            Some(Ok(raw)) => Self::counted(count(&raw)),
            Some(Err(error)) if error.kind() == std::io::ErrorKind::NotFound => Self::Absent,
            Some(Err(_)) => Self::Unknown("read_failed"),
        }
    }

    /// Sums `count` over a directory's entries; a missing directory is a confirmed absence.
    pub(in crate::services::discord) fn of_dir(
        dir: Option<PathBuf>,
        mut count: impl FnMut(&Path) -> std::io::Result<usize>,
    ) -> Self {
        let entries = match dir.map(std::fs::read_dir) {
            None => return Self::Unknown("no_runtime_root"),
            Some(Ok(entries)) => entries,
            Some(Err(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                return Self::Absent;
            }
            Some(Err(_)) => return Self::Unknown("read_failed"),
        };
        let mut total = 0;
        for entry in entries {
            match entry.and_then(|entry| count(&entry.path())) {
                Ok(n) => total += n,
                Err(_) => return Self::Unknown("read_failed"),
            }
        }
        Self::counted(total)
    }
}

/// Kinds up to `AbortMarker` degrade health (retirement should have cleared them); the rest
/// is display-cleanup detail. Kinds from `AbortMarker` on are observed by the sweeper.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub(in crate::services::discord) enum ResidueKind {
    Row,
    MailboxToken,
    MailboxActiveId,
    MailboxQueue,
    AbortMarker,
    BusyRetry,
    OrphanPanel,
    AbandonRequest,
    PendingAnchor,
}

impl ResidueKind {
    const ALL: [Self; 9] = [
        Self::Row,
        Self::MailboxToken,
        Self::MailboxActiveId,
        Self::MailboxQueue,
        Self::AbortMarker,
        Self::BusyRetry,
        Self::OrphanPanel,
        Self::AbandonRequest,
        Self::PendingAnchor,
    ];

    fn degrades(self) -> bool {
        self <= Self::AbortMarker
    }

    /// Stores scoped by bot token: every registered runtime must confirm the absence.
    fn per_token(self) -> bool {
        matches!(self, Self::OrphanPanel | Self::AbandonRequest)
    }

    fn freshness(self) -> Option<Duration> {
        (self >= Self::AbortMarker).then_some(SWEEP_FRESHNESS)
    }
}

/// The latest observation of one kind by one runtime: a count stays until that observer
/// confirms the absence, and an unknown read only marks the slot unconfirmed.
#[derive(Debug, Default)]
struct Slot {
    present: usize,
    confirmed_at: Option<Instant>,
    unknown: bool,
}

impl Slot {
    fn observe(&mut self, presence: Presence, now: Instant) {
        let present = match presence {
            Presence::Present(n) => n,
            Presence::Absent => 0,
            Presence::Unknown(_) => return self.unknown = true,
        };
        *self = Self {
            present,
            confirmed_at: Some(now),
            unknown: false,
        };
    }

    fn is_confirmed(&self, now: Instant, freshness: Option<Duration>) -> bool {
        let fresh = |at: Instant| freshness.is_none_or(|window| age(now, at) <= window);
        !self.unknown && self.confirmed_at.is_some_and(fresh)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(in crate::services::discord) enum OFact {
    Open,
    Idle,
    Unknown,
}

/// Why the input actor last declined to offer the head row; an unknown kind is still a veto.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(in crate::services::discord) enum VetoKind {
    ThreadActive,
    ThreadUnobserved,
    DispatchPolicyUnknown,
    ComposerOccupied,
    ModalPresent,
    BindingNotCurrent,
    AdmissionBarrier,
    Other(String),
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub(in crate::services::discord) struct InputCounts {
    pub received: usize,
    pub ready: usize,
    pub in_flight: usize,
    pub running: usize,
    pub held_modal: usize,
    pub held_not_ready: usize,
    pub held_ambiguous: usize,
    pub unaccepted: usize,
}

impl InputCounts {
    fn waiting(&self) -> usize {
        let held = self.held_modal + self.held_not_ready + self.held_ambiguous;
        self.received + self.ready + held + self.unaccepted
    }
}

/// What the input supervisor publishes for one ledger channel; health never opens the ledger.
#[derive(Clone, Debug, Serialize)]
pub(in crate::services::discord) struct TranscriptTurnView {
    pub o_fact: OFact,
    pub writer_ready: bool,
    pub binding_generation: u64,
    #[serde(flatten)]
    pub counts: InputCounts,
    pub head_key: Option<String>,
    /// The head row is Held or Unaccepted; that backlog has its own reconcile reason.
    pub head_held: bool,
    pub oldest_waiting_age_secs: Option<u64>,
    pub hold_reasons: Vec<String>,
    #[serde(skip)]
    pub head_offer_veto: Option<(VetoKind, Instant)>,
    #[serde(skip)]
    pub drain_eligible_since: Option<Instant>,
}

/// Measures how long the head row has been offerable without a veto; a change of binding or
/// head restarts it, and a re-push of the same state keeps the start.
#[derive(Debug, Default)]
pub(in crate::services::discord) struct EligibilityClock {
    since: Option<Instant>,
    context: Option<(u64, Option<String>)>,
}

impl EligibilityClock {
    pub(in crate::services::discord) fn observe(
        &mut self,
        view: &TranscriptTurnView,
        now: Instant,
    ) -> Option<Instant> {
        let eligible = view.o_fact == OFact::Idle
            && view.writer_ready
            && view.hold_reasons.is_empty()
            && !view.head_held
            && view.counts.waiting() > 0
            && view.head_offer_veto.is_none();
        let context = Some((view.binding_generation, view.head_key.clone()));
        if !eligible {
            self.since = None;
        } else if self.since.is_none() || self.context != context {
            self.since = Some(now);
        }
        self.context = context;
        self.since
    }
}

#[derive(Debug, Default, Serialize)]
struct TranscriptTurnHealth {
    provider: String,
    channel_id: u64,
    view: Option<TranscriptTurnView>,
    view_age_secs: Option<u64>,
    held_total: Option<usize>,
    head_veto: Option<HeadVeto>,
    drain_eligible_secs: Option<u64>,
    residue: BTreeMap<ResidueKind, usize>,
    unobserved: Vec<ResidueKind>,
    legacy_cleanup_pending: BTreeMap<ResidueKind, usize>,
}

#[derive(Debug, Serialize)]
struct HeadVeto {
    kind: VetoKind,
    since_secs: u64,
}

#[derive(Debug, Serialize)]
pub(in crate::services::discord) struct TranscriptTurnsHealth {
    channels: Vec<TranscriptTurnHealth>,
    skipped_effects: BTreeMap<&'static str, u64>,
}

#[derive(Debug, Default)]
struct ChannelState {
    slots: BTreeMap<(ResidueKind, String), Slot>,
    unknown_since: BTreeMap<ResidueKind, Instant>,
    view: Option<(TranscriptTurnView, Instant)>,
    first_seen: Option<Instant>,
}

static CHANNELS: OnceLock<Mutex<BTreeMap<(String, u64), ChannelState>>> = OnceLock::new();

fn with_channel<R>(provider: &str, channel_id: u64, f: impl FnOnce(&mut ChannelState) -> R) -> R {
    let mut channels = CHANNELS
        .get_or_init(Mutex::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    f(channels
        .entry((provider.to_ascii_lowercase(), channel_id))
        .or_default())
}

impl ChannelState {
    fn record(&mut self, kind: ResidueKind, token_hash: &str, presence: Presence, now: Instant) {
        let token = if kind.per_token() { token_hash } else { "" };
        let slot = self.slots.entry((kind, token.to_string())).or_default();
        slot.observe(presence, now);
    }

    /// Present count over every runtime, and whether each required observer confirmed it
    /// freshly; one runtime's absence never hides another runtime's records.
    fn aggregate(&self, kind: ResidueKind, tokens: &[String], now: Instant) -> (usize, bool) {
        let slots = || self.slots.iter().filter(move |((k, _), _)| *k == kind);
        let present = slots().map(|(_, slot)| slot.present).sum();
        let confirmed_by = |token: &str| {
            slots().any(|((_, t), slot)| t == token && slot.is_confirmed(now, kind.freshness()))
        };
        let confirmed = if kind.per_token() {
            !tokens.is_empty() && tokens.iter().all(|token| confirmed_by(token))
        } else {
            confirmed_by("")
        };
        (present, confirmed)
    }

    fn project(
        &mut self,
        provider: &str,
        channel_id: u64,
        tokens: &[String],
        now: Instant,
    ) -> (Vec<String>, TranscriptTurnHealth) {
        let first_seen = *self.first_seen.get_or_insert(now);
        let mut health = TranscriptTurnHealth {
            provider: provider.to_string(),
            channel_id,
            ..TranscriptTurnHealth::default()
        };
        let mut unobservable = false;
        for kind in ResidueKind::ALL {
            let (present, confirmed) = self.aggregate(kind, tokens, now);
            if present > 0 && kind.degrades() {
                health.residue.insert(kind, present);
            } else if present > 0 {
                health.legacy_cleanup_pending.insert(kind, present);
            }
            if confirmed {
                self.unknown_since.remove(&kind);
                continue;
            }
            health.unobserved.push(kind);
            let since = *self.unknown_since.entry(kind).or_insert(now);
            unobservable |= kind.degrades() && age(now, since) > UNKNOWN_DEGRADES_AFTER;
        }
        let mut reasons = Vec::new();
        if !health.residue.is_empty() {
            reasons.push(format!("tui_o:legacy_residue:{channel_id}"));
        }
        if unobservable {
            reasons.push(format!("tui_o:legacy_residue_unknown:{channel_id}"));
        }
        let published_at = self.view.as_ref().map_or(first_seen, |(_, at)| *at);
        if age(now, published_at) > VIEW_STALE_AFTER {
            reasons.push(format!("tui_o:input_view_stale:{channel_id}"));
        }
        if let Some((view, at)) = &self.view {
            let eligible_for = view.drain_eligible_since.map(|since| age(now, since));
            if eligible_for.is_some_and(|eligible| eligible >= NOT_DRAINING_AFTER) {
                reasons.push(format!("tui_o:input_not_draining:{channel_id}"));
            }
            health.view_age_secs = Some(age(now, *at).as_secs());
            health.drain_eligible_secs = eligible_for.map(|eligible| eligible.as_secs());
            let held = view.counts.held_modal + view.counts.held_not_ready;
            health.held_total = Some(held + view.counts.held_ambiguous + view.counts.unaccepted);
            health.head_veto = view.head_offer_veto.as_ref().map(|(kind, since)| HeadVeto {
                kind: kind.clone(),
                since_secs: age(now, *since).as_secs(),
            });
            health.view = Some(view.clone());
        }
        (reasons, health)
    }
}

/// Stores the supervisor's latest view for a retired channel, stamped with its push time.
pub(in crate::services::discord) fn publish_view(
    provider: &str,
    channel_id: u64,
    view: TranscriptTurnView,
    now: Instant,
) {
    with_channel(provider, channel_id, |state| state.view = Some((view, now)));
}

/// Observes one runtime's durable Legacy records for its retired channels; runs at the end
/// of that runtime's placeholder-sweeper tick.
pub(in crate::services::discord) fn observe_durable_residue(
    provider: &ProviderKind,
    token_hash: &str,
) {
    use ResidueKind as K;
    let now = Instant::now();
    for (retired_provider, c) in legacy_supervision::retired_channels() {
        if !provider.as_str().eq_ignore_ascii_case(&retired_provider) {
            continue;
        }
        let (p, t) = (provider, token_hash);
        let observed = [
            (
                K::AbortMarker,
                tui_direct_abort_marker::channel_presence(p.as_str(), c),
            ),
            (
                K::BusyRetry,
                busy_followup_retry_store::channel_presence(p, c),
            ),
            (
                K::OrphanPanel,
                status_panel_orphan_store::channel_presence(p, t, c),
            ),
            (
                K::AbandonRequest,
                abandon_request_store::channel_presence(p, t, c),
            ),
            (
                K::PendingAnchor,
                turn_view_reconciler::pending_anchor_presence(p, c),
            ),
        ];
        with_channel(&retired_provider, c, |state| {
            for (kind, presence) in observed {
                state.record(kind, t, presence, now);
            }
        });
    }
}

type RuntimeMailboxes = Vec<(String, HashMap<ChannelId, ChannelMailboxSnapshot>)>;

/// Mailbox residue across every runtime of the provider; with no runtime it is unknown.
fn mailbox_presence(runtimes: &RuntimeMailboxes, channel_id: u64) -> [(ResidueKind, Presence); 3] {
    let channel = ChannelId::new(channel_id);
    let count = |measure: fn(&ChannelMailboxSnapshot) -> usize| match runtimes.is_empty() {
        true => Presence::Unknown("no_runtime"),
        false => Presence::counted(
            runtimes
                .iter()
                .filter_map(|(_, mailboxes)| mailboxes.get(&channel))
                .map(measure)
                .sum(),
        ),
    };
    [
        (
            ResidueKind::MailboxToken,
            count(|s| usize::from(s.cancel_token.is_some())),
        ),
        (
            ResidueKind::MailboxActiveId,
            count(|s| usize::from(s.active_user_message_id.is_some())),
        ),
        (
            ResidueKind::MailboxQueue,
            count(|s| s.intervention_queue.len()),
        ),
    ]
}

/// Degraded reasons for the retired channels plus their detail block (detail builds only).
/// With no retired channel it returns at once, before any IO.
pub(super) async fn project_retired(
    providers: &[ProviderEntry],
    detailed: bool,
) -> (Vec<String>, Option<TranscriptTurnsHealth>) {
    let retired = legacy_supervision::retired_channels();
    if retired.is_empty() {
        return (Vec::new(), None);
    }
    let rows = legacy_supervision::observe_rows(&retired).await;
    let mut runtimes: BTreeMap<String, RuntimeMailboxes> = BTreeMap::new();
    for entry in providers {
        let name = entry.name.to_ascii_lowercase();
        if retired.iter().any(|(provider, _)| *provider == name) {
            let mailboxes = entry.shared.mailboxes.snapshot_all().await;
            let token = entry.shared.token_hash.clone();
            runtimes.entry(name).or_default().push((token, mailboxes));
        }
    }
    let now = Instant::now();
    let (mut reasons, mut channels) = (Vec::new(), Vec::new());
    let unregistered = RuntimeMailboxes::new();
    for ((provider, channel_id), row) in retired.iter().zip(rows) {
        let runtimes = runtimes.get(provider).unwrap_or(&unregistered);
        let tokens: Vec<String> = runtimes.iter().map(|(token, _)| token.clone()).collect();
        let (channel_reasons, health) = with_channel(provider, *channel_id, |state| {
            state.record(ResidueKind::Row, "", row, now);
            for (kind, presence) in mailbox_presence(runtimes, *channel_id) {
                state.record(kind, "", presence, now);
            }
            state.project(provider, *channel_id, &tokens, now)
        });
        reasons.extend(channel_reasons);
        channels.push(health);
    }
    let detail = detailed.then(|| TranscriptTurnsHealth {
        channels,
        skipped_effects: legacy_supervision::skipped_effects(),
    });
    (reasons, detail)
}

/// One diagnostics line for a retired channel; `None` leaves the report unchanged.
pub(in crate::services::discord) fn report_line(
    provider: &ProviderKind,
    channel_id: u64,
) -> Option<String> {
    if !legacy_supervision::is_retired(provider.as_str(), channel_id) {
        return None;
    }
    let now = Instant::now();
    Some(with_channel(provider.as_str(), channel_id, |state| {
        let mut listed = [Vec::new(), Vec::new()];
        for kind in ResidueKind::ALL {
            let present = state.aggregate(kind, &[], now).0;
            if present > 0 {
                let entry = format!("{}={present}", serialized(&kind));
                listed[usize::from(!kind.degrades())].push(entry);
            }
        }
        let [residue, cleanup] = listed.map(|kinds| match kinds.is_empty() {
            true => "none".to_string(),
            false => kinds.join(","),
        });
        let view = state.view.as_ref().map_or_else(
            || "view `unpublished`".to_string(),
            |(view, at)| {
                let veto = view.head_offer_veto.as_ref().map_or_else(
                    || "none".to_string(),
                    |(kind, since)| format!("{} {}s", serialized(kind), age(now, *since).as_secs()),
                );
                let (o_fact, waiting) = (serialized(&view.o_fact), view.counts.waiting());
                let pushed = age(now, *at).as_secs();
                format!("o `{o_fact}`, waiting `{waiting}`, veto `{veto}`, pushed `{pushed}s` ago")
            },
        );
        format!(
            "- transcript turns: input `ledger`, {view}, residue `{residue}`, cleanup `{cleanup}`\n"
        )
    }))
}

fn age(now: Instant, since: Instant) -> Duration {
    now.saturating_duration_since(since)
}

fn serialized(value: &impl Serialize) -> String {
    match serde_json::to_value(value) {
        Ok(serde_json::Value::String(name)) => name,
        Ok(other) => other.to_string(),
        Err(_) => "?".to_string(),
    }
}

#[cfg(test)]
#[path = "transcript_turn_tests.rs"]
pub(in crate::services::discord) mod tests;
