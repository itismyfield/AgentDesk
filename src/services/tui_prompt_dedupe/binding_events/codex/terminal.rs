//! Historical terminal sequences close observations without granting source ownership.

use super::*;
use std::collections::{BTreeMap, BTreeSet};

/// Redundant observations closed by exact typed rejection and a matching resolved anchor.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct SettledCompanions(BTreeSet<u64>);

impl SettledCompanions {
    pub(crate) fn contains(&self, seq: u64) -> bool {
        self.0.contains(&seq)
    }
}

/// Called only after the ownership fold validates all terminal links in the whole channel log.
pub(super) fn settled_companions(records: &[Logged]) -> SettledCompanions {
    let rejected: BTreeMap<_, _> = records
        .iter()
        .filter(|logged| matches!(logged.event.new, BindingTarget::Rejected { .. }))
        .filter_map(|logged| {
            Some((
                logged.codex_ownership.as_ref()?.pending_seq?,
                logged.event.seq,
            ))
        })
        .collect();
    let mut settled = SettledCompanions::default();
    for logged in records {
        let resolution = &logged.event;
        let BindingTarget::Resolved {
            pending_seq,
            source,
        } = &resolution.new
        else {
            continue;
        };
        let Some(anchor) = pending_seq
            .checked_sub(1)
            .and_then(|index| usize::try_from(index).ok())
            .and_then(|index| records.get(index))
            .map(|logged| &logged.event)
        else {
            continue;
        };
        let BindingTarget::Pending {
            payload_session_id,
            payload_transcript_path,
        } = &anchor.new
        else {
            continue;
        };
        if anchor.seq >= resolution.seq
            || anchor.provider != "codex"
            || resolution.provider != anchor.provider
            || resolution.tmux_session != anchor.tmux_session
            || resolution.execution_nonce != anchor.execution_nonce
            || !anchor
                .execution_nonce
                .as_deref()
                .is_some_and(|nonce| !nonce.is_empty())
            || source.session_id != *payload_session_id
            || payload_transcript_path
                .as_deref()
                .is_some_and(|path| Path::new(path) != source.path)
        {
            continue;
        }
        for candidate in records.iter().map(|logged| &logged.event).filter(|event| {
            event.seq > anchor.seq
                && event.seq < resolution.seq
                && event.provider == anchor.provider
                && event.tmux_session == anchor.tmux_session
                && matches!(event.new, BindingTarget::Pending { .. })
        }) {
            let same_nonce = candidate.execution_nonce == anchor.execution_nonce;
            let same_payload = candidate.new == anchor.new;
            let closed = rejected
                .get(&candidate.seq)
                .is_some_and(|seq| *seq > candidate.seq && *seq < resolution.seq);
            #[cfg(test)]
            let same_nonce = same_nonce || mutant("companion_payload_only");
            #[cfg(test)]
            let same_payload = same_payload || mutant("companion_nonce_only");
            #[cfg(test)]
            let closed = closed || mutant("companion_without_terminal");
            if same_nonce && same_payload && closed {
                settled.0.insert(candidate.seq);
            }
        }
    }
    settled
}

#[cfg(test)]
fn mutant(name: &str) -> bool {
    std::env::var("AGENTDESK_CODEX_SNAPSHOT_MUTANT").is_ok_and(|value| value == name)
}

pub(crate) fn terminal_pending_at_seq(channel: u64, seq: u64) -> io::Result<Option<u64>> {
    let _logs = lock_logs();
    Ok(ownership_records(channel)?.into_iter().find_map(|logged| {
        if logged.event.seq != seq
            || logged.event.provider != "codex"
            || !matches!(logged.event.new, BindingTarget::Rejected { .. })
        {
            return None;
        }
        logged.codex_ownership?.pending_seq
    }))
}
