//! A pane's settled source and channel watermark from one strict ownership snapshot.

use super::*;

pub(crate) struct SettledSource {
    pub source: SourceId,
    pub execution_nonce: Option<String>,
    pub binding_seq: u64,
}

pub(crate) fn read(channel: u64, pane: &str) -> io::Result<Option<SettledSource>> {
    let _logs = lock_logs();
    let records = ownership_records(channel)?;
    let mut checked = std::collections::HashSet::new();
    for logged in records.iter().filter(|l| l.event.tmux_session == pane) {
        if logged.event.provider != "codex" {
            return Ok(None);
        }
        let Some(ownership) = &logged.codex_ownership else {
            // Bare observations cannot prove an exact terminal link; Rejected remains audit-only.
            if matches!(
                logged.event.new,
                BindingTarget::Pending { .. } | BindingTarget::Resolved { .. }
            ) {
                return Ok(None);
            }
            continue;
        };
        if checked.insert(&ownership.context.execution_nonce) {
            let fold = fold_records(&ownership.context, &records)?;
            if !fold.pending.is_empty() || fold.conflicted || !neutral_after_proof(&fold) {
                return Ok(None);
            }
        }
    }
    let binding_seq = records.last().map_or(0, |logged| logged.event.seq);
    Ok(records.into_iter().rev().find_map(|logged| {
        if logged.event.tmux_session != pane {
            return None;
        }
        match logged.event.new {
            BindingTarget::Source(source) | BindingTarget::Resolved { source, .. } => {
                Some(SettledSource {
                    source,
                    execution_nonce: logged.event.execution_nonce,
                    binding_seq,
                })
            }
            _ => None,
        }
    }))
}

fn neutral_after_proof(fold: &Fold) -> bool {
    let Some(proof) = &fold.verified else {
        return true;
    };
    fold.legacy_unverified
        .iter()
        .filter(|event| event.seq > proof.seq)
        .all(|event| {
            let same = |source: &SourceId| {
                recorded_native(
                    event.seq,
                    source,
                    &proof.source,
                    &proof.ownership.canonical_witnesses,
                )
            };
            match &event.new {
                BindingTarget::Source(source) => {
                    same(source) && event.old.as_ref().is_none_or(same)
                }
                BindingTarget::Rejected { .. } => true,
                _ => false,
            }
        })
}
