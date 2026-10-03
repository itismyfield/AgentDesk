//! Intake for an O-owned channel runs only where that channel's writer can take it: the gateway
//! hosting a resumed actor while it holds the lease. A new placement is also held off the O home
//! and ends a pending adoption on it. A delegated channel is held wherever its home gate takes no
//! intake. Every other channel routes as before.

use crate::services::agent_protocol::RuntimeHandoffKind;
use crate::services::cluster::channel_home;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum IntakeRoute {
    /// Not an O channel, or the writer is off: existing placement applies unchanged.
    Unselected,
    /// This process's writer can take the channel now.
    Gateway,
    /// An O channel this process must not run; held, never handed to Legacy.
    Hold(String),
}

pub(crate) fn route(provider: &str, channel: u64) -> IntakeRoute {
    route_parsed(provider, || Some(channel))
}

/// For a textual destination; an unparseable one is held once any channel is O-owned.
pub(crate) fn route_text(provider: &str, channel: &str) -> IntakeRoute {
    route_parsed(provider, || channel.parse().ok())
}

/// For a new placement rather than a claim: a selected channel is held off the O home, and on it a
/// pending adoption is released before routing as `route` does.
pub(crate) fn route_for_placement(provider: &str, channel: u64) -> IntakeRoute {
    placed(provider, Some(channel))
}

pub(crate) fn route_text_for_placement(provider: &str, channel: &str) -> IntakeRoute {
    placed(provider, channel.parse().ok())
}

fn placed(provider: &str, channel: Option<u64>) -> IntakeRoute {
    if let Err(detail) = super::claim_for_placement(channel) {
        return IntakeRoute::Hold(detail);
    }
    route_parsed(provider, || channel)
}

/// O-owned channels this process must not claim for `provider`, and delegated channels whose
/// home takes no intake here, as stored in intake rows.
pub(crate) fn held_channels(provider: &str) -> Vec<String> {
    let owned = super::owned_channels();
    let held = |&(channel, kind): &(u64, RuntimeHandoffKind)| {
        let held = judge(provider, channel, kind) != IntakeRoute::Gateway;
        held.then(|| channel.to_string())
    };
    let mut held: Vec<String> = owned.iter().filter_map(held).collect();
    // A gate closing between the two reads can name a channel twice; keep the first.
    for channel in channel_home::intake_held_channels() {
        if !held.contains(&channel) {
            held.push(channel);
        }
    }
    held
}

/// A known destination reads only its own adoption, so another channel's adoption in progress
/// never delays it; an unknown one is held once any channel is O-owned.
fn route_parsed(provider: &str, channel: impl FnOnce() -> Option<u64>) -> IntakeRoute {
    match channel() {
        Some(channel) => match super::owned_kind(channel) {
            Some(kind) => judge(provider, channel, kind),
            None if super::delegated(channel).is_some() => IntakeRoute::Hold(format!(
                "O channel {channel} is delegated and its home does not take intake here"
            )),
            None => IntakeRoute::Unselected,
        },
        None if super::owned_channels().is_empty() && !channel_home::any_registered() => {
            IntakeRoute::Unselected
        }
        None => IntakeRoute::Hold("intake destination channel is unknown".into()),
    }
}

fn judge(provider: &str, channel: u64, kind: RuntimeHandoffKind) -> IntakeRoute {
    let provider_kind = match provider.trim().to_ascii_lowercase().as_str() {
        "claude" => Some(RuntimeHandoffKind::ClaudeTui),
        "codex" => Some(RuntimeHandoffKind::CodexTui),
        _ => None,
    };
    if provider_kind != Some(kind) {
        return IntakeRoute::Hold(format!(
            "O channel {channel} is {kind:?}, not provider {provider}"
        ));
    }
    if accepts(channel) {
        IntakeRoute::Gateway
    } else {
        IntakeRoute::Hold(format!(
            "O channel {channel} runs only on the gateway with its writer ready"
        ))
    }
}

pub(super) fn accepts(channel: u64) -> bool {
    #[cfg(test)]
    if let Some(answer) = test_probe::answer(channel) {
        return answer;
    }
    crate::services::tui_o::writer::host::channel_accepts(channel)
}

/// Stands in for this process's writer readiness on the current thread.
#[cfg(test)]
pub(crate) mod test_probe {
    use std::cell::RefCell;

    type Probe = Box<dyn FnMut(u64) -> bool>;

    thread_local! {
        static PROBE: RefCell<Option<Probe>> = const { RefCell::new(None) };
    }

    pub(crate) struct ProbeGuard(Option<Probe>);

    pub(crate) fn answer_with(probe: impl FnMut(u64) -> bool + 'static) -> ProbeGuard {
        ProbeGuard(PROBE.with(|cell| cell.replace(Some(Box::new(probe)))))
    }

    /// Each readiness read takes the next answer; a read past the list fails the test.
    pub(crate) fn answers(answers: &[bool]) -> ProbeGuard {
        let mut answers = answers.to_vec().into_iter();
        answer_with(move |channel| {
            answers
                .next()
                .unwrap_or_else(|| panic!("unexpected readiness read for channel {channel}"))
        })
    }

    pub(super) fn answer(channel: u64) -> Option<bool> {
        PROBE.with(|cell| cell.borrow_mut().as_mut().map(|probe| probe(channel)))
    }

    impl Drop for ProbeGuard {
        fn drop(&mut self) {
            PROBE.with(|cell| cell.replace(self.0.take()));
        }
    }
}
