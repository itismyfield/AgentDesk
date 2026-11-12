//! A worker validates physical ends before replay; effect points only watch their version.

use super::{InputFacts, Key, LivePorts, Outcome, Ports, Watch};
use crate::services::tui_o::store::restart_end::{self, RestartEnd};

pub(super) fn version(channel: u64) -> u64 {
    *restart_end::subscribe(channel).borrow()
}

pub(super) fn version_changed(watch: &mut Watch, ports: &dyn Ports, channel: u64) -> bool {
    let next = ports.restart_version(channel);
    let changed = watch.restart_version != next;
    watch.restart_version = next;
    changed
        || facts(watch).is_some_and(|facts| {
            facts.restart_needs_reload()
                || (facts.restart_ended() && !facts.restart_proof_current())
        })
}

pub(super) fn read(_ports: &LivePorts, channel: u64) -> Result<Vec<RestartEnd>, String> {
    #[cfg(test)]
    if let Some(root) = &_ports.root {
        return restart_end::read_durable(root, channel);
    }
    let Some(root) = crate::config::runtime_root() else {
        return Ok(Vec::new());
    };
    restart_end::read_durable(&root, channel)
}

pub(super) fn install(facts: &mut InputFacts, ports: &dyn Ports, key: &Key) -> Result<(), String> {
    let ends: Vec<_> = ports
        .restart_ends(key.channel)?
        .into_iter()
        .filter(|end| end.tmux_session == key.session)
        .collect();
    if ends.is_empty() {
        facts.set_restart_ends(Vec::new());
        return Ok(());
    }
    let history = ports.binding_events(key.channel, key.provider)?;
    let checked =
        crate::services::tui_o::writer::restart_end::validated(ends, &history, facts.binding())?;
    facts.set_restart_ends(checked);
    Ok(())
}

pub(super) fn prepare(
    facts: Result<InputFacts, String>,
    ports: &dyn Ports,
    key: &Key,
) -> Result<InputFacts, String> {
    let mut facts = facts?;
    install(&mut facts, ports, key)?;
    Ok(facts)
}

pub(super) fn unreadable() -> Outcome {
    Outcome::Unreadable {
        retry_at: std::time::Instant::now() + super::UNREADABLE_RETRY_AFTER,
    }
}

pub(super) fn worker_facts(
    facts: Result<InputFacts, String>,
    ports: &dyn Ports,
    key: &Key,
) -> Result<InputFacts, Outcome> {
    let facts = facts.map_err(|_| Outcome::Halted)?;
    prepare(Ok(facts), ports, key).map_err(|_| unreadable())
}

fn facts(watch: &Watch) -> Option<&InputFacts> {
    match &watch.outcome {
        Outcome::Source { facts, .. } => Some(facts),
        Outcome::Tail(reader) => Some(&reader.facts),
        _ => None,
    }
}

pub(super) fn closed(watch: &Watch) -> bool {
    facts(watch).is_some_and(InputFacts::restart_ended)
}

pub(super) fn closed_at_eof(watch: &Watch) -> bool {
    facts(watch).is_some_and(|facts| {
        facts.restart_ended()
            && facts.restart_proof_current()
            && facts.caught_up().is_ok_and(|complete| complete)
    })
}

#[cfg(all(test, unix))]
#[path = "effort_restart_tests.rs"]
mod tests;
