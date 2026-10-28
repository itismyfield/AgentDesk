//! A resumed source attaches only from its durable start, retaining its live cursor.

use super::*;
use crate::db::dispatched_sessions::hosted_execution::HostedState;
use crate::services::tui_prompt_dedupe::binding_context::execution_context;
use crate::services::tui_prompt_dedupe::{
    RuntimeObservation, runtime_observation_for_kind_under_source_authority,
};

pub(in crate::services::discord) async fn attach_resumed_herdr_source(
    pool: &PgPool,
    owner: &HostedOwner,
    channel: u64,
    nonce: &str,
    source: &SourceId,
    reader: &dyn HerdrExecutionReader,
) -> HerdrSourceAttach {
    let logical = owner.logical_key.as_str();
    let Ok(context) = execution_context("claude", nonce) else {
        return HerdrSourceAttach::NoBaseline;
    };
    if owner.provider != "claude"
        || context.provider != owner.provider
        || context.tmux_session != logical
        || context.channel_id != Some(channel)
    {
        return HerdrSourceAttach::NoBaseline;
    }
    let (verdict, row_nonce, agent, observed) = reconcile(pool, owner, reader).await;
    let original = match &observed {
        HostedLookup::Found(found) => match &found.record {
            HostedRecord::Known(record) if record.owner == *owner => record.clone(),
            _ => return HerdrSourceAttach::NotPublished,
        },
        _ => return HerdrSourceAttach::NotPublished,
    };
    let already = match verdict {
        HostReconcile::Pending(HerdrExecutionMatch::Match)
            if row_nonce.as_deref() == Some(nonce) =>
        {
            false
        }
        HostReconcile::Herdr(HerdrExecutionMatch::Match) if row_nonce.as_deref() == Some(nonce) => {
            true
        }
        verdict => {
            withhold_herdr_execution(logical, Some(nonce));
            return HerdrSourceAttach::Refused(verdict);
        }
    };
    if load(pool, owner).await != observed {
        return HerdrSourceAttach::NotPublished;
    }
    let published = with_tmux_source_authority(logical, |authority| {
        if !matches!(binding_events::claude_resume_start::started_source(&context), Ok(Some(ref current)) if current == source)
        {
            return false;
        }
        let binding = match runtime_observation_for_kind_under_source_authority(
            authority,
            RuntimeHandoffKind::ClaudeTui,
        ) {
            RuntimeObservation::Missing => {
                claude_tui_rehydrated_binding(&source.session_id, &source.path)
            }
            RuntimeObservation::Present(live) | RuntimeObservation::Expired(live)
                if live.runtime_kind == RuntimeHandoffKind::ClaudeTui && live_is(&live, source) =>
            {
                live
            }
            _ => return false,
        };
        register_claude_pane_under_source_authority(
            authority,
            channel,
            binding,
            Record::Exact(source.clone()),
        )
        .is_some_and(Persisted::published)
    });
    if !published {
        withhold_herdr_execution(logical, Some(nonce));
        return HerdrSourceAttach::NotPublished;
    }
    let bound = already
        || match &observed {
            HostedLookup::Found(found) => {
                bind_pg(pool, found, owner, nonce).await == Ok(HostedCasOutcome::Written)
            }
            _ => false,
        };
    let bound = bound
        || matches!(load(pool, owner).await, HostedLookup::Found(found)
        if matches!(&found.record, HostedRecord::Known(record) if record.execution_nonce == nonce && record.state == HostedState::Bound
            && record.owner == *owner && record.location == original.location && record.expected == original.expected));
    if !bound {
        withhold_herdr_execution(logical, Some(nonce));
        return HerdrSourceAttach::NotPublished;
    }
    admit_herdr_execution(logical, nonce);
    HerdrSourceAttach::Published {
        bound: true,
        agent_agrees: agent_agrees(agent, Some(source)),
    }
}
