//! Final notice admission stays synchronous through prepare and the first POST poll.

use super::super::pieces::PieceWork;
use super::NoticeOrigin;
use crate::services::tui_input::human_frame::history::{self, ObservedAttempt};
use crate::services::tui_o::shadow::{SourceId, UnitKind};
use crate::services::tui_o::store::{ChannelStore, ledger::LedgerEntry};
use crate::services::tui_prompt_dedupe::binding_events::{self as p5, BindingEvent, BindingTarget};
use crate::services::{discord, session_host, tmux_common as tc, tui_prompt_dedupe};

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum NoticeRefusal {
    Busy,
    /// A registered injection may still own the row; a receipt or withdrawal decides it later.
    OwnershipPending,
    Withheld(String),
    Store(String),
    Violation(String),
}

fn withheld(reason: impl Into<String>) -> NoticeRefusal {
    NoticeRefusal::Withheld(reason.into())
}

pub(crate) fn admit_notice<R>(
    piece: &PieceWork,
    store: &mut ChannelStore,
    start: impl FnOnce(&mut ChannelStore) -> R,
) -> Result<R, NoticeRefusal> {
    if piece.unit_key.kind != UnitKind::InputNotice {
        return Ok(start(store));
    }
    if let Some(reason) = store.ledger().violation() {
        return Err(NoticeRefusal::Violation(reason.into()));
    }
    if store.ledger().excluded(&piece.unit_key).is_some() {
        return Err(withheld("input notice already acknowledged"));
    }
    let origin = piece
        .notice_origin
        .as_deref()
        .ok_or_else(|| withheld("input notice origin missing"))?;
    let record = &origin.record;
    let notices = store.ledger().input_notices();
    let evidence = notices
        .evidence
        .get(&(record.source.clone(), record.start, record.end))
        .ok_or_else(|| withheld("input notice evidence missing"))?;
    if piece.unit_key.channel_id != store.init().channel
        || piece.index != 0
        || evidence.unit_key.as_ref() != Some(&piece.unit_key)
        || evidence.payload.as_ref() != Some(&piece.payload)
        || evidence.origin.as_deref() != Some(origin)
        || notices
            .unresolved
            .contains_key(&(record.source.clone(), record.start, record.end))
    {
        return Err(withheld(
            "input notice provenance is incomplete or unresolved",
        ));
    }
    let prompt = evidence
        .prompt
        .clone()
        .ok_or_else(|| withheld("input notice prompt missing"))?;
    tc::try_with_tmux_source_authority(&origin.logical_session, |_| {
        let events = p5::records_strict(store.init().channel)
            .map_err(|error| withheld(error.to_string()))?
            .map_err(|error| withheld(format!("input notice binding log: {error:?}")))?;
        if cleared(&events, origin, store)? {
            return exclude(piece, store, "input_notice_clear");
        }
        if tui_prompt_dedupe::is_discord_relayed_user_prompt(&prompt)
            || discord::input_notice_content(&prompt).is_none()
        {
            return exclude(piece, store, "input_notice_source");
        }
        match history::observed_attempt_checked(
            "claude",
            &origin.logical_session,
            std::slice::from_ref(&prompt),
            Some(&piece.unit_key.native_key),
        )
        .map_err(withheld)?
        {
            ObservedAttempt::Known(_) => return exclude(piece, store, "input_notice_source"),
            ObservedAttempt::Pending => return Err(NoticeRefusal::OwnershipPending),
            ObservedAttempt::None => {}
        }
        match crate::config::session_hosts::herdr_endpoint(store.init().channel) {
            Some(endpoint) => {
                session_host::try_notice_fence(&endpoint, &origin.execution_nonce, || start(store))
                    .map_err(|reason| match reason {
                        session_host::HerdrGateRefusal::MutationBusy => NoticeRefusal::Busy,
                        _ => withheld(format!("input notice execution fence: {reason:?}")),
                    })
            }
            None => Ok(start(store)),
        }
    })
    .ok_or(NoticeRefusal::Busy)?
}

fn bound_source<'a>(
    events: &'a [BindingEvent],
    event: &'a BindingEvent,
) -> Result<&'a SourceId, NoticeRefusal> {
    if let BindingTarget::Source(source) = &event.new {
        return Ok(source);
    }
    let resolved = if matches!(event.new, BindingTarget::Pending { .. }) {
        let mut matching = events.iter().filter(|row| {
            matches!(row.new, BindingTarget::Resolved { pending_seq, .. } if pending_seq == event.seq)
        });
        let resolved = matching
            .next()
            .ok_or_else(|| withheld("input notice binding pending"))?;
        if matching.next().is_some() {
            return Err(withheld("input notice resolution ambiguous"));
        }
        resolved
    } else {
        event
    };
    let BindingTarget::Resolved {
        pending_seq,
        source,
    } = &resolved.new
    else {
        return Err(withheld("input notice binding is not a source"));
    };
    let pending = events.iter().find(|row| row.seq == *pending_seq);
    let pending = pending.ok_or_else(|| withheld("input notice pending binding missing"))?;
    let BindingTarget::Pending {
        payload_session_id,
        payload_transcript_path,
    } = &pending.new
    else {
        return Err(withheld("input notice resolution has no Pending"));
    };
    if resolved.seq <= pending.seq
        || resolved.tmux_session != pending.tmux_session
        || resolved.execution_nonce != pending.execution_nonce
        || resolved.old != pending.old
        || source.session_id != *payload_session_id
        || payload_transcript_path
            .as_ref()
            .is_some_and(|path| std::path::Path::new(path) != source.path)
        || events.iter().any(|row| {
            pending.seq < row.seq
                && row.seq < resolved.seq
                && row.tmux_session == pending.tmux_session
                && !matches!(row.new, BindingTarget::Rejected { .. })
        })
    {
        return Err(withheld(
            "input notice resolution changed its owner or source",
        ));
    }
    Ok(source)
}

fn cleared(
    events: &[BindingEvent],
    origin: &NoticeOrigin,
    store: &ChannelStore,
) -> Result<bool, NoticeRefusal> {
    let channel = store.init().channel;
    if events
        .iter()
        .any(|event| event.channel_id != channel || event.provider != "claude")
    {
        return Err(withheld("input notice binding log has a foreign owner"));
    }
    let original = events
        .iter()
        .find(|event| event.seq == origin.binding_seq)
        .ok_or_else(|| withheld("input notice original binding missing"))?;
    if original.tmux_session != origin.logical_session
        || original.execution_nonce.as_deref() != Some(&origin.execution_nonce)
        || bound_source(events, original)? != &origin.record.source
    {
        return Err(withheld("input notice original binding changed"));
    }
    let mut lineage = vec![&origin.record.source];
    for event in events
        .iter()
        .filter(|event| event.seq > original.seq && event.tmux_session == origin.logical_session)
    {
        if event.old.as_ref().is_some_and(|old| lineage.contains(&old))
            && !matches!(event.new, BindingTarget::Rejected { .. })
        {
            if event.cause == p5::BindingCause::Clear {
                return Ok(true);
            }
            let source = bound_source(events, event)?;
            if !lineage.contains(&source) {
                lineage.push(source);
            }
        }
    }
    for end in store.clear_ends().map_err(withheld)? {
        if end.channel != channel
            || end.provider != "claude"
            || end.tmux_session != origin.logical_session
            || end.basis != "confirmed_clear"
            || end.binding_seq < origin.binding_seq
            || !lineage.contains(&&end.source)
            || !events.iter().any(|event| event.seq == end.binding_seq)
        {
            continue;
        }
        // Clear captures the channel checkpoint, which can include another pane's latest row.
        if let Some(last) = events.iter().rev().find(|event| {
            event.seq <= end.binding_seq
                && event.tmux_session == origin.logical_session
                && !matches!(event.new, BindingTarget::Rejected { .. })
        }) && !matches!(last.new, BindingTarget::Pending { .. })
            && last.execution_nonce.as_deref() == Some(&end.execution_nonce)
            && bound_source(events, last)? == &end.source
        {
            return Ok(true);
        }
    }
    Ok(false)
}

fn exclude<R>(
    piece: &PieceWork,
    store: &mut ChannelStore,
    reason: &str,
) -> Result<R, NoticeRefusal> {
    store
        .append_ledger(LedgerEntry::Excluded {
            unit_key: piece.unit_key.clone(),
            reason: reason.into(),
        })
        .map_err(|error| NoticeRefusal::Store(format!("{error:?}")))?;
    match store.ledger().violation() {
        Some(reason) => Err(NoticeRefusal::Violation(reason.into())),
        None => Err(withheld(reason)),
    }
}

#[cfg(all(test, unix))]
#[path = "admission_tests.rs"]
pub(crate) mod tests;
