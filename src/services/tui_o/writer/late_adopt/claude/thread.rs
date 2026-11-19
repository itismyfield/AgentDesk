//! Takeover of the first source of a thread launch whose body was reserved before spawn.

use sha2::{Digest, Sha256};

use super::super::codex::prefix::{Prefix, PrefixClass, Verdict};
use super::prefix;
use crate::services::tui_o::cutover::claude_thread::protected_launch;
use crate::services::tui_o::shadow::{SourceId, binding_reader::source_id_for, capture::same_file};
use crate::services::tui_o::writer::{
    adoption::{self, ChildDebt, ReadVersion},
    binding::{BindingEvent, BindingRecord},
    rotation::bound_source,
};

pub(in crate::services::tui_o::writer) fn is_protected(events: &[BindingEvent]) -> bool {
    events
        .iter()
        .rev()
        .find(|event| !matches!(event.record, BindingRecord::Rejected { .. }))
        .is_some_and(|latest| protected_launch(latest).is_some())
}

pub(in crate::services::tui_o::writer) fn binding(
    events: &[BindingEvent],
    parent: u64,
) -> Option<(SourceId, u64)> {
    let (current, _) = adoption::current(events)?;
    let latest = events
        .iter()
        .rev()
        .find(|event| bound_source(event) == Some(&current))?;
    let verified = crate::services::tui_prompt_dedupe::binding_events::verifies_claude_source(
        latest.channel_id,
        latest.seq,
        &latest.execution_nonce,
        &current,
    )
    .unwrap_or(false);
    if !verified {
        return None;
    }
    let launch = protected_launch(latest)?;
    if launch.claude_thread_writer_parent() != Some(parent)
        || launch.expected_native_session_id.as_deref() != Some(current.session_id.as_str())
    {
        return None;
    }
    let root = events.iter().find(|event| {
        event.execution_nonce == latest.execution_nonce && bound_source(event).is_some()
    })?;
    super::takeover_binding(events, root.seq.checked_sub(1)?)
}

/// No legacy body was admitted for this launch. The writer can start at byte zero even if the
/// file grew while older deliveries were settling.
pub(super) fn source(source: &SourceId) -> Verdict {
    match source_id_for(&source.session_id, &source.path) {
        Ok(actual) if same_file(&actual, source) => Verdict::Eligible(Prefix {
            class: PrefixClass::ReservedClaude,
            len: 0,
            sha256: hex::encode(Sha256::digest(b"")),
            prompt_digest: None,
            prompt_end: None,
        }),
        Ok(_) => Verdict::Refuse("the reserved transcript was replaced".into()),
        Err(error) => Verdict::Wait(format!("the reserved transcript is unavailable: {error}")),
    }
}

pub(in crate::services::tui_o::writer) struct Earlier {
    events: Vec<BindingEvent>,
    files: Vec<(std::path::PathBuf, ReadVersion)>,
}

pub(in crate::services::tui_o::writer) fn pin_earlier(
    events: Vec<BindingEvent>,
    root: u64,
    debts: &ChildDebt,
    proofs: &[super::Owed],
    sends: (u64, u64),
) -> Result<Earlier, String> {
    let mut checked = std::collections::HashSet::new();
    let mut files = Vec::new();
    let mut pin = |source: &SourceId, seq: u64, nonce: &str| {
        let before = ReadVersion::of(&source.path).ok_or("earlier thread source is unavailable")?;
        let sampled = proofs.iter().any(|owed| {
            owed.sample.as_ref().is_some_and(|sample| {
                &sample.source == source
                    && sample.nonce == nonce
                    && super::old_source_done_with_sends(
                        Some(sample),
                        &(source.clone(), seq),
                        &events,
                        sends,
                    )
                    .is_ok()
            })
        });
        if !sampled {
            prefix::metadata_since(source, 0)?;
        }
        if ReadVersion::of(&source.path).as_ref() != Some(&before) {
            return Err("earlier thread source moved while being verified".into());
        }
        files.push((source.path.clone(), before));
        Ok::<(), String>(())
    };
    for event in events[..events.partition_point(|event| event.seq < root)]
        .iter()
        .rev()
    {
        let Some(source) = bound_source(event) else {
            continue;
        };
        if checked.insert((source, event.execution_nonce.as_str()))
            && !debts.settles(source, &event.execution_nonce)
        {
            pin(source, event.seq, &event.execution_nonce).map_err(|detail| {
                format!(
                    "earlier thread source at binding {} still owes delivery: {detail}",
                    event.seq
                )
            })?;
        }
    }
    let (_, named) = adoption::logged(&events).map_err(|error| error.to_string())?;
    for source in named {
        pin(source, 0, "")?;
    }
    let prefix = events.partition_point(|event| event.seq < root);
    Ok(Earlier {
        events: events[..prefix].to_vec(),
        files,
    })
}

impl Earlier {
    pub(in crate::services::tui_o::writer) fn recheck(
        &self,
        events: &[BindingEvent],
        root: u64,
    ) -> Result<(), String> {
        let prefix = events.partition_point(|event| event.seq < root);
        if events[..prefix] != self.events
            || self
                .files
                .iter()
                .any(|(path, before)| ReadVersion::of(path).as_ref() != Some(before))
        {
            return Err("earlier thread sources changed before writer activation".into());
        }
        Ok(())
    }
}
