//! A fresh Claude thread launch reserves its body for its own writer.

use crate::services::agent_protocol::RuntimeHandoffKind;
use crate::services::tui_o::writer::binding::{
    BindingEvent, BindingEvents, BindingRecord, ChannelBindingLog,
};
use crate::services::tui_o::{channel_policy::BootChannels, shadow::ShadowProvider};
use crate::services::tui_prompt_dedupe::binding_context::{
    BindingContext, SpawnNonceMarker, execution_context, observe_spawn_nonce_marker,
};

#[cfg(unix)]
pub(crate) fn launch_policy(channel: u64) -> Option<String> {
    use crate::services::tui_o::channel_policy::Adoption;
    super::test_override::with_channels(|snapshot| {
        let child = snapshot?.child(channel)?.ok()?;
        (child.kind == RuntimeHandoffKind::ClaudeTui
            && child.candidate.late_adopt_origin()
            && (child.candidate.peek() == Adoption::Deferred || child.candidate.child_parked()))
        .then(|| BindingContext::claude_thread_writer_policy(child.parent))
    })
}

fn tmux_context(channel: u64, session: &str) -> Option<BindingContext> {
    use crate::domain::host_observation::HostKind;
    use crate::services::tmux_common::host_marker::{HostKindMarker, read_host_kind_marker};
    let SpawnNonceMarker::Known(nonce) = observe_spawn_nonce_marker(session) else {
        return None;
    };
    let context = own_context(channel, session, &nonce)?;
    (context.claude_thread_writer_parent().is_some()
        || read_host_kind_marker(session) == HostKindMarker::Known(HostKind::Tmux))
    .then_some(context)
}

pub(crate) fn restored_kind(
    channel: u64,
    tmux: Option<&str>,
    native_session: Option<&str>,
) -> Option<RuntimeHandoffKind> {
    let native_session = native_session?;
    if !super::thread_child(channel) {
        return None;
    }
    let tmux = tmux?;
    let context = tmux_context(channel, tmux)?;
    if context.expected_native_session_id.as_deref() != Some(native_session) {
        return None;
    }
    crate::services::tmux_common::resolve_tmux_runtime_kind_marker(tmux).or_else(|| {
        context
            .claude_thread_writer_parent()
            .map(|_| RuntimeHandoffKind::ClaudeTui)
    })
}

fn own_context(channel: u64, session: &str, nonce: &str) -> Option<BindingContext> {
    let context = execution_context("claude", nonce).ok()?;
    (context.channel_id == Some(channel)
        && context.tmux_session == session
        && context.owner_runtime_root == crate::services::tmux_common::current_tmux_owner_marker())
    .then_some(context)
}

pub(crate) fn protected_launch(event: &BindingEvent) -> Option<BindingContext> {
    let context = own_context(
        event.channel_id,
        &event.tmux_session,
        &event.execution_nonce,
    )?;
    context.claude_thread_writer_parent().map(|_| context)
}

pub(super) fn holds_tmux_body(channel: u64, session: &str) -> bool {
    super::test_override::with_channels(|snapshot| {
        snapshot.is_some_and(|snapshot| holds_body(snapshot, channel, Some(session)))
    })
}

fn protected_for(snapshot: &BootChannels, context: Option<BindingContext>) -> bool {
    let Some(context) = context else { return false };
    context.claude_thread_writer_parent().is_some_and(|parent| {
        snapshot.channels().contains(&parent)
            && snapshot.kind(parent) == Some(RuntimeHandoffKind::ClaudeTui)
            && context.channel_id.is_some_and(|channel| {
                snapshot
                    .child(channel)
                    .is_none_or(|child| child.is_ok_and(|child| child.parent == parent))
            })
    })
}

/// For claims without a tmux name, take the child's latest recorded session as the locator.
pub(super) fn holds_body(snapshot: &BootChannels, channel: u64, session: Option<&str>) -> bool {
    let known = |session: &str| protected_for(snapshot, tmux_context(channel, session));
    if session.is_some_and(known) {
        return true;
    }
    ChannelBindingLog::new(channel, ShadowProvider::Claude)
        .binding_events_since(channel, 0)
        .ok()
        .and_then(|events| {
            events
                .into_iter()
                .rev()
                .find(|event| !matches!(event.record, BindingRecord::Rejected { .. }))
        })
        .is_some_and(|event| {
            session.is_none_or(|session| session == event.tmux_session)
                && (known(&event.tmux_session) || protected_for(snapshot, protected_launch(&event)))
        })
}

pub(crate) fn earlier_launch_pending(channel: u64, session: Option<&str>) -> bool {
    super::test_override::with_channels(|snapshot| {
        snapshot.is_some_and(|snapshot| {
            snapshot
                .child(channel)
                .is_some_and(|child| child.is_ok_and(|child| !child.candidate.peek().owned()))
                && holds_body(snapshot, channel, session)
        })
    })
}

pub(crate) fn record_restored(destination: &poise::serenity_prelude::GuildChannel, session: &str) {
    use crate::services::session_host::surface_policy::VerifiedSurface;
    let parent = VerifiedSurface::checked_parent(destination);
    if parent.is_some()
        && tmux_context(destination.id.get(), session)
            .as_ref()
            .and_then(BindingContext::claude_thread_writer_parent)
            == parent
    {
        super::record_thread_child(destination, false);
    }
}
