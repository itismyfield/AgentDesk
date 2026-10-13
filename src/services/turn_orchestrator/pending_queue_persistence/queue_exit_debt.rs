//! Queue-exit events not yet acknowledged, kept in the channel's canonical queue snapshot so an
//! item a claim removed can be reported again after a restart. Nothing here applies or acks one.

use super::PendingQueueItem;
#[cfg(test)]
use super::{
    ChannelId, ChannelMailboxState, Instant, Intervention, MessageId, Path,
    PendingQueueSourceTextSegment, ProviderKind, QueuePersistenceContext, SystemTime, fs,
    parse_snapshot, pending_queue_file_path, pending_queue_item_from_intervention,
    pending_queue_item_to_intervention, pending_queue_root, persist_queue,
};
#[cfg(test)]
use crate::services::turn_orchestrator::{
    QueueExitEvent, QueueExitKind,
    active_source_dedup::purge_active_source_from_queue,
    queued_head::{SnapshotMeta, adopt, meta_value, record_write},
};

/// One removed intervention whose exit feedback is still owed, under the event id minted when it
/// left the queue; the provider, token and channel are the file's namespace.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct QueueExitDebt {
    pub(crate) event_id: String,
    pub(crate) episode: String,
    pub(crate) kind: QueueExitDebtKind,
    pub(crate) intervention: PendingQueueItem,
    pub(crate) state: QueueExitDebtState,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum QueueExitDebtKind {
    Cancelled,
    Expired,
    Superseded,
    Overflow,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum QueueExitDebtState {
    Unacked,
}

/// Refuses debts no reader could attribute: a blank id or episode, a payload without a message
/// or channel, or an event id recorded twice.
pub(in crate::services::turn_orchestrator) fn check_queue_exit_debts(
    debts: &[QueueExitDebt],
) -> Result<(), String> {
    for (index, debt) in debts.iter().enumerate() {
        let item = &debt.intervention;
        if debt.event_id.trim().is_empty()
            || debt.episode.trim().is_empty()
            || item.message_id == 0
            || item.channel_id.is_none_or(|channel| channel == 0)
        {
            return Err("queue-exit debt has a blank event id, episode or identity".into());
        }
        if debts[..index].iter().any(|d| d.event_id == debt.event_id) {
            return Err(format!(
                "queue-exit debt {} is recorded twice",
                debt.event_id
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
impl From<QueueExitKind> for QueueExitDebtKind {
    fn from(kind: QueueExitKind) -> Self {
        match kind {
            QueueExitKind::Cancelled => Self::Cancelled,
            QueueExitKind::Expired => Self::Expired,
            QueueExitKind::Superseded => Self::Superseded,
            QueueExitKind::Overflow => Self::Overflow,
        }
    }
}

#[cfg(test)]
impl QueueExitDebt {
    /// The debt for `event`, minting its event id; called once, by the transition that removed it.
    pub(crate) fn new(episode: &str, event: &QueueExitEvent, channel_id: ChannelId) -> Self {
        let source = &event.intervention;
        let mut intervention = pending_queue_item_from_intervention(
            source,
            channel_id,
            None,
            SystemTime::now(),
            Instant::now(),
        );
        // Every segment as held, not only a split one, so the restored payload is the original.
        let segments = source.source_text_segments.iter();
        intervention.source_text_segments = (segments)
            .map(|segment| PendingQueueSourceTextSegment {
                message_id: segment.message_id.get(),
                text: segment.text.clone(),
            })
            .collect();
        Self {
            event_id: uuid::Uuid::new_v4().to_string(),
            episode: episode.to_string(),
            kind: event.kind.into(),
            intervention,
            state: QueueExitDebtState::Unacked,
        }
    }

    /// The event this debt reports, rebuilt from the stored payload.
    pub(crate) fn event(&self) -> QueueExitEvent {
        let intervention = pending_queue_item_to_intervention(
            self.intervention.clone(),
            SystemTime::now(),
            Instant::now(),
        );
        let kind = match self.kind {
            QueueExitDebtKind::Cancelled => QueueExitKind::Cancelled,
            QueueExitDebtKind::Expired => QueueExitKind::Expired,
            QueueExitDebtKind::Superseded => QueueExitKind::Superseded,
            QueueExitDebtKind::Overflow => QueueExitKind::Overflow,
        };
        QueueExitEvent { intervention, kind }
    }
}

/// Lets a metadata-free test write replace only a missing file or a legacy array.
#[cfg(test)]
pub(crate) fn refuse_metadata_loss(path: &Path) -> Result<(), String> {
    match fs::read_to_string(path) {
        Ok(content) if !matches!(parse_snapshot(&content), Ok((_, None))) => {
            Err(format!("refusing to overwrite {}", path.display()))
        }
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
            Err(format!("read {}: {error}", path.display()))
        }
        _ => Ok(()),
    }
}

/// The debts in one queue file: none for a missing file or legacy array, an error for a file that
/// cannot be read or parsed or a debt whose payload names another channel.
#[cfg(test)]
fn read_file_debts(path: &Path, channel_id: ChannelId) -> Result<Vec<QueueExitDebt>, String> {
    let content = match fs::read_to_string(path) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(format!("read {}: {error}", path.display())),
    };
    let (_, meta) =
        parse_snapshot(&content).map_err(|e| format!("parse {}: {e}", path.display()))?;
    let debts = meta.map(|meta| meta.queue_exit_debts).unwrap_or_default();
    let foreign = debts
        .iter()
        .find(|debt| debt.intervention.channel_id != Some(channel_id.get()));
    if let Some(debt) = foreign {
        return Err(format!(
            "queue-exit debt {} in {} names another channel",
            debt.event_id,
            path.display()
        ));
    }
    Ok(debts)
}

/// One channel's unacknowledged debts; only a missing file reads as none.
#[cfg(test)]
pub(crate) fn read_queue_exit_debts_checked(
    provider: &ProviderKind,
    token_hash: &str,
    channel_id: ChannelId,
) -> Result<Vec<QueueExitDebt>, String> {
    let path = pending_queue_file_path(provider, token_hash, channel_id)
        .ok_or_else(|| format!("pending queue root unavailable for channel {channel_id}"))?;
    read_file_debts(&path, channel_id)
}

/// A scan that met errors: the debts it could read, and what it could not.
#[cfg(test)]
pub(crate) struct DebtScanError {
    pub(crate) found: Vec<(ChannelId, Vec<QueueExitDebt>)>,
    pub(crate) errors: Vec<String>,
}

/// Every channel of the namespace holding a debt, an emptied queue included; a listing, entry or
/// file that cannot be read is an error, never an absent debt.
#[cfg(test)]
pub(crate) fn scan_queue_exit_debts_checked(
    provider: &ProviderKind,
    token_hash: &str,
) -> Result<Vec<(ChannelId, Vec<QueueExitDebt>)>, DebtScanError> {
    let failed = |error: String| DebtScanError {
        found: Vec::new(),
        errors: vec![error],
    };
    let root =
        pending_queue_root().ok_or_else(|| failed("pending queue root unavailable".into()))?;
    let dir = root.join(provider.as_str()).join(token_hash);
    match fs::read_dir(&dir) {
        Ok(entries) => scan_entries(entries.map(|entry| entry.map(|entry| entry.path()))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(failed(format!("read_dir {}: {error}", dir.display()))),
    }
}

#[cfg(test)]
fn scan_entries(
    entries: impl Iterator<Item = std::io::Result<std::path::PathBuf>>,
) -> Result<Vec<(ChannelId, Vec<QueueExitDebt>)>, DebtScanError> {
    let (mut found, mut errors) = (Vec::new(), Vec::new());
    for entry in entries {
        let path = match entry {
            Ok(path) => path,
            Err(error) => {
                errors.push(format!("scan entry: {error}"));
                continue;
            }
        };
        if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
            continue;
        }
        let stem = path.file_stem().and_then(|stem| stem.to_str());
        let Some(channel) = stem
            .and_then(|stem| stem.parse::<u64>().ok())
            .filter(|id| *id != 0)
        else {
            continue;
        };
        let channel = ChannelId::new(channel);
        match read_file_debts(&path, channel) {
            Ok(debts) if debts.is_empty() => {}
            Ok(debts) => found.push((channel, debts)),
            Err(error) => errors.push(error),
        }
    }
    found.sort_by_key(|(channel, _)| channel.get());
    if errors.is_empty() {
        Ok(found)
    } else {
        Err(DebtScanError { found, errors })
    }
}

#[cfg(test)]
#[derive(Debug)]
pub(crate) enum QueueExitCommitError {
    /// The file still holds the previous queue; memory was restored to it.
    NotCommitted(String),
    /// The file holds neither snapshot or cannot be read; memory was restored and the owner's
    /// later writes are refused until the file is adopted again.
    Indeterminate(String),
}

/// One event a transition removed and the single debt minted for it.
#[cfg(test)]
#[derive(Clone)]
pub(crate) struct ExitWithDebt {
    pub(crate) event: QueueExitEvent,
    pub(crate) debt: QueueExitDebt,
}

/// The queue one step leaves and one debt per item it removed, checked against the queue it was
/// prepared from; the event ids are minted here once, so a retry writes the same ids.
#[cfg(test)]
pub(crate) struct PreparedQueueExitTransition {
    original: QueueIdentities,
    remaining: Vec<Intervention>,
    exits: Vec<ExitWithDebt>,
}

#[cfg(test)]
#[derive(Debug)]
pub(crate) struct QueueExitPrepareError(pub(crate) String);

#[cfg(test)]
impl PreparedQueueExitTransition {
    /// Removes `source` from `original` as a claim of that message does, with a debt per exit.
    pub(crate) fn from_scoped_purge(
        original: &[Intervention],
        source: MessageId,
        episode: &str,
        channel_id: ChannelId,
    ) -> Result<Self, QueueExitPrepareError> {
        let mut remaining = original.to_vec();
        let events = purge_active_source_from_queue(&mut remaining, source);
        let foreign = events.iter().any(|event| {
            let (_, _, sources) = &identities(std::slice::from_ref(&event.intervention))[0];
            event.kind != QueueExitKind::Superseded || sources.iter().any(|s| *s != source.get())
        });
        if foreign {
            return Err(QueueExitPrepareError(format!(
                "an exit of the purge of {source} is not that source superseded"
            )));
        }
        Self::from_exits(original, remaining, events, episode, channel_id)
    }

    fn from_exits(
        original: &[Intervention],
        remaining: Vec<Intervention>,
        events: Vec<QueueExitEvent>,
        episode: &str,
        channel_id: ChannelId,
    ) -> Result<Self, QueueExitPrepareError> {
        let exits = events.into_iter().map(|event| ExitWithDebt {
            debt: QueueExitDebt::new(episode, &event, channel_id),
            event,
        });
        let transition = Self {
            original: identities(original),
            remaining,
            exits: exits.collect(),
        };
        transition.check(episode)?;
        Ok(transition)
    }

    /// Each debt reports its own event under `episode`, and the sources the queue lost are
    /// exactly the sources of the exits.
    fn check(&self, episode: &str) -> Result<(), QueueExitPrepareError> {
        let refuse = |why: &str| Err(QueueExitPrepareError(why.into()));
        let debts: Vec<QueueExitDebt> = self.exits.iter().map(|e| e.debt.clone()).collect();
        check_queue_exit_debts(&debts).map_err(QueueExitPrepareError)?;
        for ExitWithDebt { event, debt } in &self.exits {
            let stored = debt.event().intervention;
            let item = &event.intervention;
            let one = |item: &Intervention| identities(std::slice::from_ref(item));
            if debt.episode != episode
                || debt.kind != QueueExitDebtKind::from(event.kind)
                || one(&stored) != one(item)
                || (stored.author_id, &stored.text) != (item.author_id, &item.text)
            {
                return refuse("a debt does not report its own exit");
            }
        }
        let sources = |rows: &QueueIdentities| -> Vec<u64> {
            rows.iter()
                .flat_map(|(_, _, s)| s.iter().copied())
                .collect()
        };
        let mut removed = sources(&self.original);
        for kept in sources(&identities(&self.remaining)) {
            let Some(index) = removed.iter().position(|s| *s == kept) else {
                return refuse("the remaining queue holds a source the original did not");
            };
            removed.swap_remove(index);
        }
        let exits = self.exits.iter().map(|e| e.event.intervention.clone());
        let mut exited = sources(&identities(&exits.collect::<Vec<_>>()));
        removed.sort();
        exited.sort();
        if removed != exited {
            return refuse("the removed sources and the exit events differ");
        }
        Ok(())
    }

    pub(crate) fn exits(&self) -> &[ExitWithDebt] {
        &self.exits
    }
}

/// Writes the prepared queue and its debts as a single snapshot. A failed write is settled by
/// re-reading the file, so a published debt is kept and never rolled back.
#[cfg(test)]
pub(crate) fn commit_queue_exit(
    state: &mut ChannelMailboxState,
    channel_id: ChannelId,
    persistence: &QueuePersistenceContext,
    transition: &PreparedQueueExitTransition,
) -> Result<(), QueueExitCommitError> {
    // Adoption refuses a ledger already holding metadata, so it runs before the debts enter it.
    adopt(state, channel_id, persistence).map_err(QueueExitCommitError::NotCommitted)?;
    if identities(&state.intervention_queue) != transition.original {
        return Err(QueueExitCommitError::NotCommitted(
            "the queue changed after the transition was prepared".into(),
        ));
    }
    let previous_queue =
        std::mem::replace(&mut state.intervention_queue, transition.remaining.clone());
    let previous_meta = state.queued_head.meta.clone();
    let debts = transition.exits.iter().map(|exit| exit.debt.clone());
    state.queued_head.meta.queue_exit_debts.extend(debts);
    let restore = |state: &mut ChannelMailboxState| {
        state.intervention_queue = previous_queue.clone();
        state.queued_head.meta = previous_meta.clone();
    };
    let written = check_queue_exit_debts(&state.queued_head.meta.queue_exit_debts)
        .map_err(QueueExitCommitError::NotCommitted)
        .and_then(|()| {
            persist_queue(state, channel_id, persistence)
                .map_err(QueueExitCommitError::Indeterminate)
        });
    let error = match written {
        Ok(()) => return Ok(()),
        Err(QueueExitCommitError::NotCommitted(error)) => {
            restore(state);
            return Err(QueueExitCommitError::NotCommitted(error));
        }
        Err(QueueExitCommitError::Indeterminate(error)) => error,
    };
    let new = (
        identities(&state.intervention_queue),
        meta_value(&state.queued_head.meta),
    );
    let old = (
        identities(&previous_queue),
        state.queued_head.persisted_meta.clone(),
    );
    read_back_fault::run();
    match read_back(persistence, channel_id) {
        Ok(disk) if disk == new => {
            tracing::warn!(channel_id = channel_id.get(), %error, "queue exit commit was published");
            record_write(state);
            Ok(())
        }
        Ok(disk) if disk == old => {
            restore(state);
            Err(QueueExitCommitError::NotCommitted(error))
        }
        outcome => {
            restore(state);
            let found = outcome.err().unwrap_or_else(|| "neither snapshot".into());
            Err(QueueExitCommitError::Indeterminate(format!(
                "{error}; file holds {found}"
            )))
        }
    }
}

/// Runs a test's file change between a failed save and the commit's re-read of the file.
#[cfg(test)]
pub(crate) mod read_back_fault {
    use std::cell::RefCell;

    thread_local! {
        static NEXT: RefCell<Option<Box<dyn FnOnce()>>> = RefCell::new(None);
    }

    pub(crate) fn before_next(change: impl FnOnce() + 'static) {
        NEXT.with(|next| *next.borrow_mut() = Some(Box::new(change)));
    }

    pub(super) fn run() {
        if let Some(change) = NEXT.with(|next| next.borrow_mut().take()) {
            change();
        }
    }
}

#[cfg(test)]
type QueueIdentities = Vec<(u64, u64, Vec<u64>)>;

#[cfg(test)]
fn identities(queue: &[Intervention]) -> QueueIdentities {
    let row = |item: &Intervention| {
        let sources = item.source_message_ids.iter().map(|id| id.get());
        let sources: Vec<u64> = sources.collect();
        let primary = item.message_id.get();
        let sources = if sources.is_empty() {
            vec![primary]
        } else {
            sources
        };
        (primary, item.queued_generation, sources)
    };
    queue.iter().map(row).collect()
}

/// The queue identities and set metadata now on disk.
#[cfg(test)]
fn read_back(
    persistence: &QueuePersistenceContext,
    channel_id: ChannelId,
) -> Result<(QueueIdentities, Option<serde_json::Value>), String> {
    let (provider, token_hash) = (&persistence.provider, &persistence.token_hash);
    let path = pending_queue_file_path(provider, token_hash, channel_id)
        .ok_or("pending queue root unavailable")?;
    let content = match fs::read_to_string(&path) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok((Vec::new(), None));
        }
        Err(error) => return Err(format!("unreadable {}: {error}", path.display())),
    };
    let (items, meta) = parse_snapshot(&content)?;
    let row = |item: PendingQueueItem| {
        let primary = item.message_id;
        let mut sources = item.source_message_ids;
        if sources.is_empty() {
            sources.push(primary);
        }
        (primary, item.queued_generation, sources)
    };
    let meta = meta.filter(SnapshotMeta::is_set);
    Ok((
        items.into_iter().map(row).collect(),
        meta.as_ref().and_then(meta_value),
    ))
}

#[cfg(test)]
#[path = "queue_exit_debt_tests.rs"]
mod tests;
