//! A confirmed channel with no Discord turn token interrupts its bound parent turn without a lease.

use std::sync::Arc;

use poise::serenity_prelude::ChannelId;

use super::interrupt_policy::ProviderTurnInterruptOutcome;
use super::judged_stop::CommandStop;
use super::stop_host::StopTarget;
use crate::services::discord::SharedData;
use crate::services::discord::tmux_watcher_registry::AuthoritativeSessionBinding;
use crate::services::provider::ProviderKind;
use crate::services::tui_o::shadow::{ShadowProvider, SourceBinding};
use crate::services::tui_o::writer::binding::{BindingEvents, ChannelBindingLog};
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
    #[cfg(test)]
    binding_root: Option<std::path::PathBuf>,
}

impl SessionStop {
    pub(super) async fn judge(
        shared: &Arc<SharedData>,
        provider: &ProviderKind,
        channel: ChannelId,
    ) -> CommandStop {
        #[cfg(test)]
        let binding_root = crate::services::tui_prompt_dedupe::binding_events::test_root();
        #[cfg(test)]
        let root_read = binding_root.clone();
        let (shared_read, provider_read) = (shared.clone(), provider.clone());
        let observed = tokio::task::spawn_blocking(move || {
            #[cfg(test)]
            let _root = TestBindingRoot::enter(root_read.as_deref());
            observe(&shared_read, &provider_read, channel)
        })
        .await;
        let observed = match observed.map_err(|e| e.to_string()).and_then(|o| o) {
            Ok(Some(observed)) => observed,
            Ok(None) => return CommandStop::NoActiveTurn,
            // An unreadable binding or transcript is not proof the turn ended.
            Err(error) => {
                tracing::warn!(channel_id = channel.get(), %error, "session stop could not observe the turn");
                return CommandStop::HostRefused;
            }
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
            #[cfg(test)]
            binding_root,
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
        #[cfg(test)]
        let binding_root = self.binding_root.clone();
        let open = Arc::new(move || {
            #[cfg(test)]
            let _root = TestBindingRoot::enter(binding_root.as_deref());
            #[cfg(all(test, unix))]
            if crate::services::discord::tmux_watcher_registry::owner_stop_mutant(
                "2a-delivery-recheck-off",
            ) {
                return true;
            }
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
    let owners: Vec<AuthoritativeSessionBinding> =
        shared.tmux_watchers.authoritative_session_bindings(channel);
    #[cfg(all(test, unix))]
    let owners = tests::mutate_initial_owners(channel, owners);
    if owners.is_empty() {
        return Ok(None);
    }
    let events =
        ChannelBindingLog::new(channel.get(), shadow).binding_events_since(channel.get(), 0);
    #[cfg(all(test, unix))]
    let events = tests::mutate_events(channel, events);
    let events = events?;
    #[cfg(all(test, unix))]
    let owners = tests::mutate_bound_owners(channel, &events, owners);
    let mut candidates = Vec::new();
    for owner in owners {
        if owner.owner_channel_id != channel {
            continue;
        }
        let session = owner.tmux_session_name;
        let session_events: Vec<_> = events
            .iter()
            .filter(|event| event.tmux_session == session)
            .cloned()
            .collect();
        if session_events.is_empty() {
            continue;
        }
        let (sources, _) = crate::services::tui_o::writer::adoption::logged(&session_events)
            .map_err(|error| format!("unresolved session binding: {error:?}"))?;
        let source = sources
            .last()
            .cloned()
            .cloned()
            .ok_or("no source is bound")?;
        candidates.push((
            session,
            SourceBinding {
                channel_id: channel.get(),
                provider: shadow,
                source,
            },
        ));
    }
    #[cfg(all(test, unix))]
    tests::mutate_candidates(&mut candidates);
    if candidates.len() > 1 {
        return Err("ambiguous authoritative session binding".into());
    }
    let Some((session, binding)) = candidates.pop() else {
        return Ok(None);
    };
    let mut facts = InputFacts::open(binding.clone())?;
    let mut through = 0;
    let fact = loop {
        let fact = facts.poll(crate::services::tui_o::shadow::MAX_READ_BYTES)?;
        let length = std::fs::metadata(&binding.source.path)
            .map_err(|e| e.to_string())?
            .len();
        if fact.through == length {
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

#[cfg(test)]
struct TestBindingRoot(Option<std::path::PathBuf>);

#[cfg(test)]
impl TestBindingRoot {
    fn enter(root: Option<&std::path::Path>) -> Self {
        let p5 = crate::services::tui_prompt_dedupe::binding_events::test_root();
        crate::services::tui_prompt_dedupe::binding_events::set_test_root(root);
        Self(p5)
    }
}

#[cfg(test)]
impl Drop for TestBindingRoot {
    fn drop(&mut self) {
        crate::services::tui_prompt_dedupe::binding_events::set_test_root(self.0.as_deref());
    }
}

#[cfg(all(test, unix))]
#[path = "session_stop_tests.rs"]
mod tests;
