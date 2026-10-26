//! The complete pane universe and its identity; enumeration failures never become absence.
use crate::services::discord::SharedData;
use crate::services::tmux_common as tc;
use crate::services::tui_o::writer::codex_snapshot::{Pane, PaneState, Target};
use crate::services::tui_prompt_dedupe::binding_context::{self, BindingContext};
use crate::services::tui_prompt_dedupe::binding_events::BindingEvent;
use poise::serenity_prelude::ChannelId;
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;

pub(super) const LIMIT: usize = 65_536;

#[derive(Clone)]
pub(super) struct HostInventory {
    pub live: BTreeSet<String>,
    pub identity: String,
}

pub(super) fn host_inventory(
    physical: Option<&str>,
    logical: &str,
) -> Result<HostInventory, String> {
    let (live, identity) = if let Some(physical) = physical {
        let census = crate::services::session_host::herdr_endpoints().read_pane_census(LIMIT)?;
        let mut live = BTreeSet::new();
        let mut found = 0;
        for (_, panes) in &census {
            for pane in panes {
                if pane == physical {
                    found += 1;
                    live.insert(logical.to_owned());
                } else {
                    return Err("unassigned Herdr pane in complete census".into());
                }
            }
        }
        if found != 1 {
            return Err("current Herdr pane is absent or ambiguous".into());
        }
        (live, format!("{census:?}"))
    } else {
        let names = crate::services::platform::tmux::list_session_names()?;
        if names.len() > LIMIT {
            return Err("tmux census budget exceeded".into());
        }
        let mut live = BTreeSet::new();
        for name in names {
            if name.starts_with("AgentDesk-") {
                live.insert(name);
            }
        }
        let identity = format!("{live:?}");
        (live, identity)
    };
    Ok(HostInventory { live, identity })
}

pub(super) fn read(
    shared: &SharedData,
    target: &Target,
    context: &BindingContext,
    events: &[BindingEvent],
    host: &HostInventory,
) -> Result<(Vec<Pane>, String), String> {
    let contexts = binding_context::inventory("codex", LIMIT)?;
    if !contexts.contains(context) {
        return Err("current canonical context absent from census".into());
    }
    let owners: Vec<_> = shared
        .tmux_watchers
        .authoritative_session_bindings(ChannelId::new(target.channel))
        .into_iter()
        .map(|owner| owner.tmux_session_name)
        .collect();
    let mut names: BTreeSet<_> = events
        .iter()
        .map(|event| event.tmux_session.clone())
        .collect();
    names.extend(owners.iter().cloned());
    names.insert(target.tmux.clone());
    for candidate in &contexts {
        if candidate.channel_id.is_none() {
            return Err("unassigned managed launch context".into());
        }
        if candidate.channel_id != Some(target.channel) {
            continue;
        }
        if candidate.host != context.host
            || candidate.owner_runtime_root != context.owner_runtime_root
        {
            return Err("channel context belongs to another host/root".into());
        }
        names.insert(candidate.tmux_session.clone());
        if candidate.execution_nonce != target.nonce {
            // A newer pane incarnation accounts for an older launch that never bound.
            let accounted = candidate.created_at < context.created_at
                && (events.iter().any(|event| {
                    event.execution_nonce.as_deref() == Some(candidate.execution_nonce.as_str())
                }) || matches!(
                    binding_context::observe_spawn_nonce_marker(&candidate.tmux_session),
                    binding_context::SpawnNonceMarker::Known(nonce) if nonce != candidate.execution_nonce
                ));
            if !accounted {
                return Err("prepared or unaccounted launch context".into());
            }
        }
    }
    for name in &host.live {
        match tc::read_tmux_channel_binding(name) {
            Some(channel) if channel == target.channel => {
                names.insert(name.clone());
            }
            Some(_) => {}
            None if name == &target.tmux => {
                names.insert(name.clone());
            }
            None => return Err("unassigned managed live pane".into()),
        }
    }
    if names.len() > LIMIT {
        return Err("pane census budget exceeded".into());
    }
    let panes = names
        .into_iter()
        .map(|name| {
            let state = if host.live.contains(&name) {
                PaneState::Live
            } else if owners.contains(&name) {
                PaneState::Unknown
            } else {
                PaneState::Dead
            };
            Pane { name, state }
        })
        .collect();
    let bytes =
        serde_json::to_vec(&(contexts, owners, &host.identity)).map_err(|e| e.to_string())?;
    Ok((panes, format!("sha256:{:x}", Sha256::digest(bytes))))
}
