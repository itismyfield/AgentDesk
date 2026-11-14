//! What the old process left of its turn once it is gone, read from its own transcript.
use std::path::Path;

use sha2::{Digest, Sha256};

use crate::services::provider::ProviderKind;
use crate::services::tui_o::shadow::{ShadowProvider, SourceBinding};
use crate::services::tui_o::store::restart_end::{self, RestartEnd};
use crate::services::tui_o::writer::binding::{
    BindingEvents, BindingLog, BindingRecord, BindingTarget,
};
use crate::services::tui_o::writer::input_facts::InputFacts;

const READ_BYTES: u64 = 8 * 1024 * 1024;

/// The transcript the pane last bound, and the bind that names the process reading it.
pub(super) struct Bound {
    binding: SourceBinding,
    tmux_session: String,
    execution_nonce: String,
    seq: u64,
}

/// What `tmux` last bound on the channel, read from the binding log.
pub(super) fn bound(provider: &ProviderKind, channel: u64, tmux: &str) -> Result<Bound, String> {
    let shadow = match provider {
        ProviderKind::Claude => ShadowProvider::Claude,
        ProviderKind::Codex => ShadowProvider::Codex,
        _ => return Err("provider has no transcript source".into()),
    };
    let history = BindingLog.binding_history(channel)?;
    let found = history.iter().rev().find_map(|event| {
        let source = match &event.record {
            BindingRecord::Bound {
                new: BindingTarget::Source(source),
                ..
            }
            | BindingRecord::Resolved { source, .. } => source,
            _ => return None,
        };
        (event.tmux_session == tmux && event.provider == shadow).then_some((event, source))
    });
    let (event, source) = found.ok_or("the pane has no bound transcript")?;
    Ok(Bound {
        binding: SourceBinding {
            channel_id: channel,
            provider: shadow,
            source: source.clone(),
        },
        tmux_session: event.tmux_session.clone(),
        execution_nonce: event.execution_nonce.clone(),
        seq: event.seq,
    })
}

/// Records that the bound process ended where its transcript ends, when that leaves a turn open;
/// `false` when the provider closed it. An end that cannot be proven is an error, not a record.
pub(super) fn record(root: &Path, bound: Bound, operation: &str) -> Result<bool, String> {
    let Bound {
        binding,
        tmux_session,
        execution_nonce,
        seq,
    } = bound;
    let (channel, shadow, source) = (binding.channel_id, binding.provider, binding.source.clone());
    let mut facts = InputFacts::open(binding)?;
    let mut through = facts.poll(READ_BYTES)?.through;
    while !facts.caught_up()? {
        let next = facts.poll(READ_BYTES)?.through;
        if next == through {
            return Err("the transcript does not end on a complete record".into());
        }
        through = next;
    }
    let Some((open_after, native_turn_id)) = facts.open_window() else {
        return Ok(false);
    };
    let bytes = std::fs::read(&source.path).map_err(|error| error.to_string())?;
    let prefix = bytes
        .get(..through as usize)
        .ok_or("the transcript shrank")?;
    let end = RestartEnd {
        end_id: format!("effort-{operation}"),
        channel,
        provider: shadow,
        tmux_session,
        execution_nonce,
        binding_seq: seq,
        source,
        cutoff: through,
        prefix_hash: hex::encode(Sha256::digest(prefix)),
        open_after,
        native_turn_id,
    };
    restart_end::append(root, &end).map(|_| true)
}
