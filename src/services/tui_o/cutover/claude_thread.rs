//! A Claude thread reserves the source frontier its writer will own.

use crate::services::agent_protocol::RuntimeHandoffKind;
use crate::services::tui_o::writer::binding::{
    BindingEvent, BindingEvents, BindingRecord, ChannelBindingLog,
};
use crate::services::tui_o::{channel_policy::BootChannels, shadow::ShadowProvider};
use crate::services::tui_prompt_dedupe::binding_context::{
    BindingContext, SpawnNonceMarker, execution_context, observe_spawn_nonce_marker,
};
use crate::services::tui_prompt_dedupe::binding_events::claude_fold;

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

#[cfg(unix)]
pub(crate) fn reserve_resume(
    channel: u64,
    session: &str,
    native: &str,
    path: &std::path::Path,
    pane: crate::services::claude::tui_session_launch::ResumePane<'_>,
) -> Result<(), String> {
    use crate::services::claude::tui_session_launch::ResumePane;
    use crate::services::tmux_common::with_tmux_source_authority;
    let selected = !matches!(
        pane,
        ResumePane::BeforeKey {
            selected: false,
            ..
        }
    );
    let child = super::test_override::with_channels(|snapshot| snapshot?.child(channel)?.ok())
        .filter(|child| child.kind == RuntimeHandoffKind::ClaudeTui);
    let Some(child) = child else {
        return if selected {
            Err("the Claude thread policy changed before reservation".into())
        } else {
            Ok(())
        };
    };
    let _body_fence = child.candidate.lock_thread_body();
    let adoption = child.candidate.lock();
    if adoption.owned() {
        if matches!(pane, ResumePane::BeforeKey { warm: false, .. }) {
            return claude_fold::check_thread_resume(channel, child.parent, session, native, path)
                .map_err(|error| error.to_string());
        }
        return Ok(());
    }
    if !selected {
        return Ok(());
    }
    with_tmux_source_authority(session, |authority| {
        if let ResumePane::BeforeKey { nonce, warm, .. } = pane {
            let current = observe_spawn_nonce_marker(session);
            match (nonce, current) {
                (None, SpawnNonceMarker::Absent) if warm => {}
                (Some(expected), SpawnNonceMarker::Known(actual)) if expected == actual => {
                    let context = own_context(channel, session, &actual)
                        .ok_or("the Claude pane changed before writer reservation")?;
                    if context.claude_thread_writer_parent() == Some(child.parent)
                        && context.expected_native_session_id.as_deref() == Some(native)
                    {
                        return Ok(());
                    }
                }
                _ => return Err("the Claude pane changed before writer reservation".into()),
            }
        }
        let transcript = (path, matches!(pane, ResumePane::Cold));
        claude_fold::reserve_thread_resume(
            channel,
            child.parent,
            authority,
            native,
            transcript,
            || {
                if matches!(pane, ResumePane::BeforeKey { warm: false, .. }) {
                    return Err(std::io::Error::other(
                        "the Claude resume source changed before its first key",
                    ));
                }
                let (started, finished) = child.candidate.sends();
                if started != finished
                    || crate::services::discord::terminal_delivery_custody::retains_channel(channel)
                        .map_err(std::io::Error::other)?
                {
                    return Err(std::io::Error::other(
                        "the earlier turn still owns delivery",
                    ));
                }
                Ok(())
            },
        )
        .map_err(|error| error.to_string())
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

pub(super) fn held_runtime_context(channel: u64, session: &str) -> bool {
    tmux_context(channel, session).is_some_and(|ctx| ctx.claude_thread_writer_parent().is_some())
}

fn resume_under(snapshot: &BootChannels, channel: u64, session: Option<&str>) -> bool {
    match claude_fold::read_thread_resumes(channel) {
        Ok(resumes) => resumes.iter().any(|entry| {
            session.is_none_or(|session| session == entry.tmux)
                && snapshot.channels().contains(&entry.parent)
                && snapshot.kind(entry.parent) == Some(RuntimeHandoffKind::ClaudeTui)
                && snapshot
                    .child(channel)
                    .is_none_or(|child| child.is_ok_and(|child| child.parent == entry.parent))
        }),
        Err(_) => {
            snapshot.child(channel).is_some_and(|child| {
                child.is_ok_and(|child| child.kind == RuntimeHandoffKind::ClaudeTui)
            }) || snapshot.unresolved_child(channel).is_some()
        }
    }
}

/// For claims without a tmux name, use the checked child's recorded source scopes.
pub(super) fn holds_body(snapshot: &BootChannels, channel: u64, session: Option<&str>) -> bool {
    if resume_under(snapshot, channel, session) {
        return true;
    }
    let known = |session: &str| protected_for(snapshot, tmux_context(channel, session));
    if session.is_some_and(known) {
        return true;
    }
    ChannelBindingLog::new(channel, ShadowProvider::Claude)
        .binding_events_since(channel, 0)
        .is_ok_and(|events| {
            events.iter().rev().any(|event| {
                !matches!(event.record, BindingRecord::Rejected { .. })
                    && session.is_none_or(|session| session == event.tmux_session)
                    && (known(&event.tmux_session)
                        || protected_for(snapshot, protected_launch(event)))
            })
        })
}

/// The transport rechecks the durable reservation under the same fence as publication.
pub(super) fn resume_fenced(channel: u64, session: Option<&str>, parent: Option<u64>) -> bool {
    !claude_fold::read_thread_resumes(channel).is_ok_and(|resumes| {
        !resumes.iter().any(|entry| {
            parent.is_none_or(|parent| entry.parent == parent)
                && session.is_none_or(|session| session == entry.tmux)
        })
    })
}

pub(super) fn names_legacy_turn(
    channel: u64,
    candidate: &super::channel_policy::Candidate,
    nonce: Option<[u8; 32]>,
    source: Option<[u8; 32]>,
) -> bool {
    let (Some(nonce), Some(source)) = (nonce, source) else {
        return false;
    };
    let fingerprint = super::channel_policy::first_nonce_id;
    let Ok(resumes) = claude_fold::read_thread_resumes(channel) else {
        return false;
    };
    candidate
        .legacy_run()
        .into_iter()
        .chain(candidate.owed_runs())
        .any(|run| {
            let (Some(turn), Some(delivery)) = (run.turn, run.delivery) else {
                return false;
            };
            turn.4.as_deref().map(fingerprint) == Some(nonce)
                && delivery.source_path.to_str().map(fingerprint) == Some(source)
                && resumes.iter().all(|entry| {
                    entry.source.path != delivery.source_path
                        || turn.3.is_some_and(|start| start < entry.offset)
                })
        })
}

pub(crate) fn record_restored(destination: &poise::serenity_prelude::GuildChannel, session: &str) {
    use crate::services::session_host::surface_policy::VerifiedSurface;
    let parent = VerifiedSurface::checked_parent(destination);
    let resumed = claude_fold::read_thread_resumes(destination.id.get()).is_ok_and(|entries| {
        entries
            .iter()
            .any(|entry| Some(entry.parent) == parent && entry.tmux == session)
    });
    if parent.is_some()
        && (resumed
            || tmux_context(destination.id.get(), session)
                .as_ref()
                .and_then(BindingContext::claude_thread_writer_parent)
                == parent)
    {
        super::record_thread_child(destination, false);
    }
}
