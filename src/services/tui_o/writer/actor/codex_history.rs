//! Folds a complete observation without opening files, publishing sources or granting cleanup.

use std::collections::BTreeMap;
use std::path::{Component, Path};

use crate::services::tui_o::shadow::SourceId;
use crate::services::tui_o::writer::codex_snapshot::{
    Coverage, HostPath, Inputs, Observation, PaneState, Snapshot,
};
use crate::services::tui_prompt_dedupe::binding_events::BindingTarget;

pub(crate) fn assess(inputs: Inputs) -> Observation {
    match checked(inputs) {
        Ok(snapshot) => Observation::Complete(Box::new(snapshot)),
        Err(reason) => Observation::Unknown(reason),
    }
}

fn clean(path: &Path) -> bool {
    path.is_absolute()
        && !path
            .components()
            .any(|part| matches!(part, Component::ParentDir | Component::CurDir))
}

fn checked(mut inputs: Inputs) -> Result<Snapshot, String> {
    let complete = matches!(inputs.coverage, Coverage::Ok(()));
    #[cfg(test)]
    let complete = complete || mutant("coverage");
    if !complete {
        return Err(match inputs.coverage {
            Coverage::Err(reason) => format!("coverage unknown: {reason}"),
            Coverage::Ok(()) => "coverage unknown".into(),
        });
    }
    let target = &inputs.target;
    let context = &inputs.context;
    let root = context
        .provider_root
        .as_deref()
        .ok_or("provider root absent")?;
    if target.channel == 0
        || target.tmux.trim().is_empty()
        || target.nonce.len() != 32
        || !target.nonce.bytes().all(|byte| byte.is_ascii_hexdigit())
        || target.host.trim().is_empty()
        || context.schema != 1
        || context.provider != "codex"
        || context.channel_id != Some(target.channel)
        || context.tmux_session != target.tmux
        || context.execution_nonce != target.nonce
        || context.host.as_deref() != Some(target.host.as_str())
        || context.owner_runtime_root != target.owner_runtime_root
        || !clean(Path::new(&target.owner_runtime_root))
        || !clean(root)
    {
        return Err("current context identity mismatch".into());
    }
    let policy = match &target.host_path {
        HostPath::Legacy => matches!(
            context.source_policy.as_deref(),
            Some("shadow" | "verified")
        ),
        HostPath::Herdr { identity } => {
            !identity.trim().is_empty() && context.source_policy.is_none()
        }
    };
    let current = &inputs.current;
    if !policy
        || current.session_id.is_empty()
        || !clean(&current.path)
        || !current.path.starts_with(root)
    {
        return Err("current source or host path mismatch".into());
    }
    if inputs.events.is_empty()
        || inputs.tail != inputs.events.len() as u64
        || inputs.log_digest.is_empty()
        || inputs.census_digest.is_empty()
    {
        return Err("full channel evidence unavailable".into());
    }
    inputs.panes.sort_by(|a, b| a.name.cmp(&b.name));
    let mut panes = BTreeMap::new();
    for pane in &inputs.panes {
        if pane.name.is_empty() || panes.insert(pane.name.as_str(), &pane.state).is_some() {
            return Err("pane census is ambiguous".into());
        }
        let expected = if pane.name == target.tmux {
            PaneState::Live
        } else {
            PaneState::Dead
        };
        if pane.state != expected {
            return Err("additional live or unknown pane".into());
        }
    }
    if panes.get(target.tmux.as_str()) != Some(&&PaneState::Live) {
        return Err("current pane absent from census".into());
    }
    let (mut latest, mut pending) = (None, None);
    let (mut held, mut visited, mut resumed) = (Vec::new(), false, None);
    for (index, event) in inputs.events.iter().enumerate() {
        if event.seq != index as u64 + 1
            || event.channel_id != target.channel
            || event.provider != "codex"
            || !panes.contains_key(event.tmux_session.as_str())
        {
            return Err("channel log coverage mismatch".into());
        }
        if event.tmux_session != target.tmux {
            continue;
        }
        let own = event.execution_nonce.as_deref() == Some(target.nonce.as_str());
        if own && event.committed_at < context.created_at {
            return Err("binding predates its execution".into());
        }
        if visited && !own && !matches!(event.new, BindingTarget::Rejected { .. }) {
            return Err("pane moved after the current execution".into());
        }
        let source = match &event.new {
            BindingTarget::Source(source) if pending.is_none() => Some(source),
            BindingTarget::Source(_) => return Err("source conflicts with Pending".into()),
            BindingTarget::Pending {
                payload_session_id,
                payload_transcript_path,
            } => {
                if pending.is_some() {
                    return Err("multiple unresolved Pending bindings".into());
                }
                pending = Some((
                    event.seq,
                    &event.execution_nonce,
                    payload_session_id,
                    payload_transcript_path,
                ));
                None
            }
            BindingTarget::Resolved {
                pending_seq,
                source,
            } => {
                let Some((seq, nonce, session, path)) = pending.take() else {
                    return Err("Resolved has no Pending".into());
                };
                if seq != *pending_seq
                    || nonce != &event.execution_nonce
                    || session != &source.session_id
                    || path
                        .as_deref()
                        .is_some_and(|path| Path::new(path) != source.path)
                {
                    return Err("Resolved does not name its Pending".into());
                }
                Some(source)
            }
            BindingTarget::Rejected { .. } => None,
        };
        if let Some(source) = source {
            if own {
                if latest
                    .as_ref()
                    .is_some_and(|prior| event.old.as_ref() != Some(prior))
                {
                    return Err("current execution source chain conflicts".into());
                }
                if !clean(&source.path)
                    || !source.path.starts_with(root)
                    || source.session_id.is_empty()
                {
                    return Err("visited source outside current root".into());
                }
                if !visited && context.launch_mode == "resume" {
                    resumed = latest
                        .as_ref()
                        .filter(|source: &&SourceId| {
                            Some(source.session_id.as_str())
                                == context.expected_native_session_id.as_deref()
                        })
                        .cloned();
                }
                held.push(source.clone());
                visited = true;
            }
            latest = Some(source.clone());
        }
    }
    if let Some((seq, _, session, path)) = pending {
        return Err(format!(
            "unresolved Pending seq={seq} session={session} path={path:?}"
        ));
    }
    if latest.as_ref() != Some(current) {
        return Err("current source differs from the log".into());
    }
    match context.launch_mode.as_str() {
        "fresh" if context.expected_native_session_id.is_none() && visited => {}
        "resume" => {
            let expected = context
                .expected_native_session_id
                .as_deref()
                .ok_or("resume selector absent")?;
            let selected = resumed
                .as_ref()
                .or_else(|| held.iter().find(|source| source.session_id == expected))
                .or_else(|| (current.session_id == expected).then_some(current))
                .cloned()
                .ok_or("resume selector has no exact source")?;
            held.push(selected);
        }
        _ => return Err("execution has no complete fresh or resume source".into()),
    }
    held.push(current.clone());
    held.sort_by(|a, b| {
        (&a.path, &a.session_id, a.dev, a.ino).cmp(&(&b.path, &b.session_id, b.dev, b.ino))
    });
    held.dedup();
    #[cfg(test)]
    if mutant("latest_only") {
        held = vec![current.clone()];
    }
    #[cfg(test)]
    if mutant("full_tail") {
        inputs.tail = 0;
        inputs.log_digest.clear();
    }
    #[cfg(test)]
    if mutant("context") {
        inputs.context.first_prompt_digest = None;
        inputs.context.created_at = chrono::DateTime::UNIX_EPOCH;
    }
    Ok(Snapshot {
        target: inputs.target,
        context: inputs.context,
        current: inputs.current,
        held,
        tail: inputs.tail,
        log_digest: inputs.log_digest,
        census_digest: inputs.census_digest,
        panes: inputs.panes,
    })
}

#[cfg(test)]
fn mutant(name: &str) -> bool {
    std::env::var("AGENTDESK_CODEX_SNAPSHOT_MUTANT").is_ok_and(|value| value == name)
}

#[cfg(test)]
#[path = "codex_history_tests.rs"]
mod tests;
