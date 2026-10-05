//! A Herdr Claude pane's source is registered under its logical key, through the binding log's
//! append-before-publish path, only for an admitted execution; a refusal withholds hook switches.
#![cfg_attr(not(test), allow(dead_code))]

use std::cell::RefCell;
use std::path::Path;

use sqlx::PgPool;

use super::super::recovery_engine::host_reconcile::{
    HerdrEndpointId, HerdrExecutionReader, HerdrPaneReading, HostReconcile, reconcile_hosted,
};
use super::launch_script::claude_tui_rehydrated_binding;
use crate::db::dispatched_session_canonical_identity::{
    CanonicalSessionIdentity, SessionIdentityKind,
};
use crate::db::dispatched_sessions::hosted_execution::{
    HostedCasOutcome, HostedLookup, HostedLookupKey, HostedOwner, HostedRecord, bind_pg,
    load_hosted_execution_pg,
};
use crate::services::agent_protocol::RuntimeHandoffKind;
use crate::services::discord::tmux::execution_identity::herdr_observation::HerdrExecutionMatch;
use crate::services::tmux_common::with_tmux_source_authority;
use crate::services::tui_prompt_dedupe::binding_events::{
    self, BindingCause, BindingEvent, BindingTarget, SourceId,
};
use crate::services::tui_prompt_dedupe::pane_registration::register_claude_pane_under_source_authority;
use crate::services::tui_prompt_dedupe::{self as dedupe, Persisted, Record, TuiRuntimeBinding};
use dedupe::withhold_herdr_execution;
use dedupe::{admit_herdr_execution, runtime_binding_for_tmux_session_under_source_authority};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::services::discord) enum HerdrSourceAttach {
    /// The row holds no hosted execution; the tmux path keeps it.
    NotHerdr,
    /// The reconcile did not admit this execution; nothing was registered.
    Refused(HostReconcile),
    /// The log names no source of this execution to restore.
    NoBaseline,
    /// The execution's own canonical clear waits on its new session: input is admitted and no
    /// source or cursor is restored until that session's record lands.
    AwaitingClear,
    /// The binding event was not persisted, the log refused it, or another source is live there;
    /// nothing was published.
    NotPublished,
    /// `bound` once the pane's latest logged source is this execution's. `agent_agrees` compares
    /// Herdr's reported agent session with that source: a hint for a re-read, never a switch.
    Published {
        bound: bool,
        agent_agrees: Option<bool>,
    },
}

/// The caller's reader, keeping the agent session of the pane read the verdict came from.
struct Recorded<'a>(&'a dyn HerdrExecutionReader, RefCell<Option<String>>);

impl HerdrExecutionReader for Recorded<'_> {
    fn endpoint(&self) -> Option<&HerdrEndpointId> {
        self.0.endpoint()
    }

    fn read_pane(&self, pane_id: &str) -> HerdrPaneReading {
        let reading = self.0.read_pane(pane_id);
        if let HerdrPaneReading::Present(evidence) = &reading {
            self.1.replace(evidence.agent_session_id.clone());
        }
        reading
    }
}

async fn load(pool: &PgPool, owner: &HostedOwner) -> HostedLookup {
    let identity = CanonicalSessionIdentity {
        kind: SessionIdentityKind::DiscordChannel,
        discord_token_hash: &owner.discord_token_hash,
        channel_id: &owner.channel_id,
    };
    let provider = &owner.provider;
    load_hosted_execution_pg(pool, HostedLookupKey::Canonical { provider, identity }).await
}

/// The verdict, the row's execution nonce and the agent session Herdr reported for its pane.
async fn reconcile(
    pool: &PgPool,
    owner: &HostedOwner,
    reader: &dyn HerdrExecutionReader,
) -> (HostReconcile, Option<String>, Option<String>) {
    let lookup = load(pool, owner).await;
    let nonce = match &lookup {
        HostedLookup::Found(found) => match &found.record {
            HostedRecord::Known(record) => Some(record.execution_nonce.clone()),
            _ => None,
        },
        _ => None,
    };
    let recorded = Recorded(reader, RefCell::new(None));
    let verdict = reconcile_hosted(&lookup, &recorded);
    (verdict, nonce, recorded.1.into_inner())
}

/// The pane's logged records in order; refusal audits do not move the pane.
fn pane_records(channel: u64, logical: &str) -> Vec<BindingEvent> {
    let events = binding_events::binding_events_since(channel, 0).unwrap_or_default();
    let moved = |event: &BindingEvent| !matches!(event.new, BindingTarget::Rejected { .. });
    let pane = events
        .into_iter()
        .filter(|event| event.tmux_session == logical);
    pane.filter(moved).collect()
}

/// The source `event` names when execution `nonce` logged it.
fn source_of(event: &BindingEvent, nonce: &str) -> Option<SourceId> {
    match &event.new {
        _ if event.execution_nonce.as_deref() != Some(nonce) => None,
        BindingTarget::Source(source) | BindingTarget::Resolved { source, .. } => {
            Some(source.clone())
        }
        _ => None,
    }
}

/// The pane's latest logged source when execution `nonce` logged it; a later Pending or another
/// execution's record leaves none.
fn nonce_baseline(channel: u64, logical: &str, nonce: &str) -> Option<SourceId> {
    source_of(pane_records(channel, logical).last()?, nonce)
}

/// Whether the pane's latest record is execution `nonce`'s own SessionStart(clear) Pending, taken
/// from the source that execution logged just before it.
fn awaits_own_clear(channel: u64, logical: &str, nonce: &str) -> bool {
    let records = pane_records(channel, logical);
    let Some((pending, earlier)) = records.split_last() else {
        return false;
    };
    let canonical = matches!(pending.new, BindingTarget::Pending { .. })
        && pending.execution_nonce.as_deref() == Some(nonce)
        && pending.cause == BindingCause::Clear
        && pending.evidence.hook_event.as_deref() == Some("session_start");
    let from = earlier.last().and_then(|event| source_of(event, nonce));
    canonical && from.is_some_and(|source| pending.old.as_ref() == Some(&source))
}

fn live_is(live: &TuiRuntimeBinding, source: &SourceId) -> bool {
    live.session_id.as_deref() == Some(source.session_id.as_str())
        && Path::new(&live.output_path) == source.path
}

fn agent_agrees(agent: Option<String>, source: Option<&SourceId>) -> Option<bool> {
    agent.map(|agent| source.is_some_and(|source| source.session_id == agent))
}

/// Attaches launch `nonce`'s native session only for its own matched Pending execution; the row turns
/// Bound once the log names this execution's source, so a cold start or old-source resume waits.
pub(in crate::services::discord) async fn attach_launched_herdr_source(
    pool: &PgPool,
    owner: &HostedOwner,
    channel: u64,
    nonce: &str,
    session_id: &str,
    transcript: &Path,
    reader: &dyn HerdrExecutionReader,
) -> HerdrSourceAttach {
    let logical = owner.logical_key.as_str();
    let (verdict, row_nonce, agent) = reconcile(pool, owner, reader).await;
    match verdict {
        HostReconcile::Legacy => return HerdrSourceAttach::NotHerdr,
        HostReconcile::Pending(HerdrExecutionMatch::Match)
            if row_nonce.as_deref() == Some(nonce) => {}
        verdict => {
            // Only this launch's execution is withheld; a newer one keeps its own gate.
            withhold_herdr_execution(logical, Some(nonce));
            return HerdrSourceAttach::Refused(verdict);
        }
    }
    // The tmux launch handoff's registration, naming the launched native session.
    let binding = TuiRuntimeBinding {
        runtime_kind: RuntimeHandoffKind::ClaudeTui,
        output_path: transcript.display().to_string(),
        relay_output_path: None,
        input_fifo_path: None,
        session_id: Some(session_id.to_owned()),
        last_offset: std::fs::metadata(transcript).map_or(0, |meta| meta.len()),
        relay_last_offset: None,
    };
    dedupe::register_tmux_channel(logical, channel);
    let published = with_tmux_source_authority(logical, |authority| {
        dedupe::register_launched_tmux_runtime_binding_under_source_authority(authority, binding)
    });
    if !published {
        withhold_herdr_execution(logical, Some(nonce));
        return HerdrSourceAttach::NotPublished;
    }
    admit_herdr_execution(logical, nonce);
    let baseline = nonce_baseline(channel, logical, nonce);
    let bound = match (&baseline, load(pool, owner).await) {
        (Some(_), HostedLookup::Found(observed)) => {
            bind_pg(pool, &observed, owner, nonce).await == Ok(HostedCasOutcome::Written)
        }
        _ => false,
    };
    let agent_agrees = agent_agrees(agent, baseline.as_ref());
    HerdrSourceAttach::Published {
        bound,
        agent_agrees,
    }
}

/// Re-attaches a Bound execution after a restart: only a confirmed match restores the source the
/// log names for that execution, and only while its path still names that same file. Its own
/// pending clear admits input without restoring a source.
pub(in crate::services::discord) async fn attach_restarted_herdr_source(
    pool: &PgPool,
    owner: &HostedOwner,
    channel: u64,
    reader: &dyn HerdrExecutionReader,
) -> HerdrSourceAttach {
    let logical = owner.logical_key.as_str();
    let (verdict, row_nonce, agent) = reconcile(pool, owner, reader).await;
    let nonce = match (verdict, row_nonce) {
        (HostReconcile::Legacy, _) => return HerdrSourceAttach::NotHerdr,
        (verdict, Some(nonce)) if verdict.admits_reconnect() => nonce,
        (verdict, nonce) => {
            withhold_herdr_execution(logical, nonce.as_deref());
            return HerdrSourceAttach::Refused(verdict);
        }
    };
    let Some(source) = nonce_baseline(channel, logical, &nonce) else {
        if awaits_own_clear(channel, logical, &nonce) {
            // The next prompt writes the cleared session, which its Pending already awaits.
            admit_herdr_execution(logical, &nonce);
            return HerdrSourceAttach::AwaitingClear;
        }
        withhold_herdr_execution(logical, Some(&nonce));
        return HerdrSourceAttach::NoBaseline;
    };
    // A live binding of this source keeps its unread cursor; another live source is left alone.
    let registered = with_tmux_source_authority(logical, |authority| {
        let binding = match runtime_binding_for_tmux_session_under_source_authority(authority) {
            None => claude_tui_rehydrated_binding(&source.session_id, &source.path),
            Some(live) if live_is(&live, &source) => live,
            Some(_) => return None,
        };
        let record = Record::Exact(source.clone());
        register_claude_pane_under_source_authority(authority, channel, binding, record)
    });
    if !registered.is_some_and(Persisted::published) {
        withhold_herdr_execution(logical, Some(&nonce));
        return HerdrSourceAttach::NotPublished;
    }
    admit_herdr_execution(logical, &nonce);
    HerdrSourceAttach::Published {
        bound: true,
        agent_agrees: agent_agrees(agent, Some(&source)),
    }
}

#[cfg(test)]
#[path = "herdr_source_tests.rs"]
mod tests;
