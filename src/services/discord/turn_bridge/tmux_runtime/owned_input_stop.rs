//! A tokenless Claude stop derives input authority from the bound transcript and frame ledger.

use std::io::{Read, Seek};
use std::sync::Arc;

use poise::serenity_prelude::ChannelId;
use sha2::{Digest, Sha256};

use super::claude_stop_identity::owned_input_rows;
use super::judged_stop::CommandStop;
use super::native_stop::SourceIncarnation;
use super::stop_host::StopTarget;
use crate::services::agent_protocol::RuntimeHandoffKind;
use crate::services::discord::SharedData;
use crate::services::provider::ProviderKind;
use crate::services::tui_o::shadow::{ShadowProvider, SourceBinding, SourceId};
use crate::services::tui_o::writer::binding::{BindingEvents, ChannelBindingLog};
use crate::services::tui_o::writer::input_facts::{InputFacts, Ordered, TurnState};

#[derive(Clone, PartialEq, Eq)]
struct Observed {
    session: String,
    binding: SourceBinding,
    opener: (u64, u64),
    opener_hash: [u8; 32],
}

pub(super) async fn interrupt(
    shared: &Arc<SharedData>,
    channel: ChannelId,
    reason: &str,
) -> CommandStop {
    #[cfg(test)]
    let binding_root = crate::services::tui_prompt_dedupe::binding_events::test_root();
    #[cfg(test)]
    let root_read = binding_root.clone();
    let shared_read = shared.clone();
    let observed = tokio::task::spawn_blocking(move || {
        #[cfg(test)]
        let _root = TestBindingRoot::enter(root_read.as_deref());
        observe(&shared_read, channel)
    })
    .await;
    let (expected, owns, clean) = match observed.map_err(|e| e.to_string()).and_then(|o| o) {
        Ok(Some(observed)) => observed,
        Ok(None) => return CommandStop::NoActiveTurn,
        Err(error) => {
            tracing::warn!(channel_id = channel.get(), %error, "owned input stop could not observe its evidence");
            return CommandStop::Observed(false);
        }
    };
    if !owns {
        return CommandStop::NoActiveTurn;
    }
    if !clean {
        return CommandStop::Observed(false);
    }
    let target = StopTarget::for_session(&expected.session);
    if matches!(target, StopTarget::Refused { .. }) {
        return CommandStop::Observed(false);
    }
    let incarnation = SourceIncarnation::capture(&expected.session);
    let shared = shared.clone();
    let open = Arc::new(move || {
        #[cfg(test)]
        let _root = TestBindingRoot::enter(binding_root.as_deref());
        incarnation.matches(&expected.session)
            && observe(&shared, channel)
                .ok()
                .flatten()
                .is_some_and(|(now, owns, clean)| now == expected && owns && clean)
    });
    let outcome = super::interrupt_owned_input_on(&target, reason, open).await;
    CommandStop::Observed(outcome.sent_keys)
}

fn session(shared: &SharedData, channel: ChannelId) -> Result<Option<String>, String> {
    let owners = shared.tmux_watchers.authoritative_session_bindings(channel);
    match owners.as_slice() {
        [owner] => return Ok(Some(owner.tmux_session_name.clone())),
        [] => {}
        _ => return Err("ambiguous Claude session owner".into()),
    }
    let core = shared.core.try_lock().map_err(|e| e.to_string())?;
    Ok(core
        .sessions
        .get(&channel)
        .and_then(|session| session.channel_name.as_ref())
        .map(|name| ProviderKind::Claude.build_tmux_session_name(name)))
}

fn observe(
    shared: &SharedData,
    channel: ChannelId,
) -> Result<Option<(Observed, bool, bool)>, String> {
    let Some(session) = session(shared, channel)? else {
        return Ok(None);
    };
    let binding = source(channel, &session)?;
    let mut file = std::fs::File::open(&binding.source.path).map_err(|e| e.to_string())?;
    let metadata = file.metadata().map_err(|e| e.to_string())?;
    let (dev, ino) = crate::services::tui_o::shadow::capture::file_identity(&metadata);
    let mut facts = InputFacts::open(binding.clone())?;
    let (mut through, mut opener) = (0, None);
    let fact = loop {
        let fact = facts.poll(crate::services::tui_o::shadow::MAX_READ_BYTES)?;
        for event in facts.events() {
            match event {
                Ordered::Opened { range, .. } => opener = Some(*range),
                Ordered::Closed { .. } => opener = None,
                Ordered::Input { .. } => {}
            }
        }
        if fact.through == facts.source_len()? {
            break fact;
        }
        if fact.through <= through {
            return Err("incomplete Claude transcript evidence".into());
        }
        through = fact.through;
    };
    match fact.state {
        TurnState::Idle => return Ok(None),
        TurnState::Unknown => return Err("Claude turn state unobserved".into()),
        TurnState::Open { .. } => {}
    }
    let opener = opener.ok_or("Claude open turn has no observed opener")?;
    file.seek(std::io::SeekFrom::Start(opener.0))
        .map_err(|e| e.to_string())?;
    let mut row = vec![0; usize::try_from(opener.1 - opener.0).map_err(|e| e.to_string())?];
    file.read_exact(&mut row).map_err(|e| e.to_string())?;
    let record: serde_json::Value = serde_json::from_slice(&row).map_err(|e| e.to_string())?;
    if record
        .get("sessionId")
        .is_some_and(|id| id.as_str() != Some(binding.source.session_id.as_str()))
    {
        return Err("Claude opener names another native session".into());
    }
    let (owns, clean) = owned_input_rows(
        &binding.source.path,
        (dev, ino),
        &session,
        &binding.source.session_id,
        opener.1,
        fact.through,
    )?;
    Ok(Some((
        Observed {
            session,
            binding,
            opener,
            opener_hash: Sha256::digest(row).into(),
        },
        owns,
        clean,
    )))
}

fn source(channel: ChannelId, session: &str) -> Result<SourceBinding, String> {
    let runtime = crate::services::tui_prompt_dedupe::runtime_binding_for_tmux_session(session);
    let events = ChannelBindingLog::new(channel.get(), ShadowProvider::Claude)
        .binding_events_since(channel.get(), 0)?;
    let logged = !events.is_empty();
    let events: Vec<_> = events
        .into_iter()
        .filter(|event| event.tmux_session == session)
        .collect();
    if logged && events.is_empty() {
        return Err("Claude source log names another session".into());
    }
    if !events.is_empty() {
        let (sources, _) = crate::services::tui_o::writer::adoption::logged(&events)
            .map_err(|error| format!("unresolved Claude source binding: {error:?}"))?;
        let source = sources
            .last()
            .cloned()
            .cloned()
            .ok_or("no Claude source is bound")?;
        if runtime.as_ref().is_some_and(|binding| {
            binding.runtime_kind != RuntimeHandoffKind::ClaudeTui
                || std::path::Path::new(&binding.output_path) != source.path
                || binding.session_id.as_deref() != Some(source.session_id.as_str())
        }) {
            return Err("Claude live and durable source bindings disagree".into());
        }
        return Ok(SourceBinding {
            channel_id: channel.get(),
            provider: ShadowProvider::Claude,
            source,
        });
    }
    let binding = runtime.ok_or("Claude transcript binding unavailable")?;
    if binding.runtime_kind != RuntimeHandoffKind::ClaudeTui {
        return Err("owned input stop requires a Claude TUI source".into());
    }
    let native_session = binding
        .session_id
        .filter(|id| !id.is_empty())
        .ok_or("Claude native session identity unavailable")?;
    let file = std::fs::File::open(&binding.output_path).map_err(|e| e.to_string())?;
    let metadata = file.metadata().map_err(|e| e.to_string())?;
    let (dev, ino) = crate::services::tui_o::shadow::capture::file_identity(&metadata);
    Ok(SourceBinding {
        channel_id: channel.get(),
        provider: ShadowProvider::Claude,
        source: SourceId {
            session_id: native_session,
            path: binding.output_path.into(),
            dev,
            ino,
        },
    })
}

#[cfg(test)]
struct TestBindingRoot(Option<std::path::PathBuf>);

#[cfg(test)]
impl TestBindingRoot {
    fn enter(root: Option<&std::path::Path>) -> Self {
        let previous = crate::services::tui_prompt_dedupe::binding_events::test_root();
        crate::services::tui_prompt_dedupe::binding_events::set_test_root(root);
        Self(previous)
    }
}

#[cfg(test)]
impl Drop for TestBindingRoot {
    fn drop(&mut self) {
        crate::services::tui_prompt_dedupe::binding_events::set_test_root(self.0.as_deref());
    }
}
