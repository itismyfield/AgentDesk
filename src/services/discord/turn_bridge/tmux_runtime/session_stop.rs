//! A confirmed channel interrupts its bound parent turn without owning a mailbox lease.

use std::sync::Arc;

use poise::serenity_prelude::ChannelId;

use super::interrupt_policy::ProviderTurnInterruptOutcome;
use super::judged_stop::CommandStop;
use super::stop_host::StopTarget;
use crate::services::discord::SharedData;
use crate::services::provider::ProviderKind;
use crate::services::tui_o::shadow::{ShadowProvider, SourceBinding};
use crate::services::tui_o::writer::binding::{
    BindingEvents, BindingLog, BindingRecord, BindingTarget,
};
use crate::services::tui_o::writer::input_facts::{InputFacts, TurnState};

#[derive(Clone, PartialEq, Eq)]
struct ObservedTurn {
    session: String,
    binding: SourceBinding,
    native_turn_id: Option<String>,
}

pub(in crate::services::discord) struct SessionStop {
    target: StopTarget,
    provider: ProviderKind,
    shared: Arc<SharedData>,
    channel: ChannelId,
    observed: ObservedTurn,
}

impl SessionStop {
    pub(super) async fn judge(
        shared: &Arc<SharedData>,
        provider: &ProviderKind,
        channel: ChannelId,
    ) -> CommandStop {
        let (shared_read, provider_read) = (shared.clone(), provider.clone());
        let observed =
            tokio::task::spawn_blocking(move || observe(&shared_read, &provider_read, channel))
                .await;
        let Ok(Ok(Some(observed))) = observed else {
            return CommandStop::NoActiveTurn;
        };
        let target = StopTarget::for_session(&observed.session);
        if matches!(target, StopTarget::Refused { .. }) {
            return CommandStop::HostRefused;
        }
        CommandStop::Session(Self {
            target,
            provider: provider.clone(),
            shared: shared.clone(),
            channel,
            observed,
        })
    }

    /// Re-read the same binding and parent turn at delivery, without publishing token ownership.
    pub(in crate::services::discord) async fn interrupt(
        &self,
        reason: &str,
    ) -> ProviderTurnInterruptOutcome {
        let (shared, provider, channel, expected) = (
            self.shared.clone(),
            self.provider.clone(),
            self.channel,
            self.observed.clone(),
        );
        let open = Arc::new(move || {
            observe(&shared, &provider, channel).ok().flatten().as_ref() == Some(&expected)
        });
        super::interrupt_session_on(&self.target, &self.provider, reason, open).await
    }
}

fn observe(
    shared: &SharedData,
    provider: &ProviderKind,
    channel: ChannelId,
) -> Result<Option<ObservedTurn>, String> {
    let shadow = match provider {
        ProviderKind::Claude => ShadowProvider::Claude,
        ProviderKind::Codex => ShadowProvider::Codex,
        _ => return Ok(None),
    };
    let Some(watcher) = shared.tmux_watchers.channel_binding(&channel) else {
        return Ok(None);
    };
    if watcher.owner_channel_id != channel {
        return Ok(None);
    }
    let session = watcher.tmux_session_name;
    let events = BindingLog.binding_events_since(channel.get(), 0)?;
    let latest = events.iter().rev().find(|event| {
        event.tmux_session == session && !matches!(event.record, BindingRecord::Rejected { .. })
    });
    let Some(event) = latest.filter(|event| event.provider == shadow) else {
        return Ok(None);
    };
    let source = match &event.record {
        BindingRecord::Bound {
            new: BindingTarget::Source(source),
            ..
        }
        | BindingRecord::Resolved { source, .. } => source.clone(),
        _ => return Ok(None),
    };
    let binding = SourceBinding {
        channel_id: channel.get(),
        provider: shadow,
        source,
    };
    let length = std::fs::metadata(&binding.source.path)
        .map_err(|e| e.to_string())?
        .len();
    let mut facts = InputFacts::open(binding.clone())?;
    let mut through = 0;
    let fact = loop {
        let fact = facts.poll(crate::services::tui_o::shadow::MAX_READ_BYTES)?;
        if fact.through >= length {
            break fact;
        }
        if fact.through <= through {
            return Ok(None);
        }
        through = fact.through;
    };
    let TurnState::Open { native_turn_id } = fact.state else {
        return Ok(None);
    };
    Ok(Some(ObservedTurn {
        session,
        binding,
        native_turn_id,
    }))
}
