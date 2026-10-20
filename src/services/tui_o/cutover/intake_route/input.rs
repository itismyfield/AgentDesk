//! Fresh admission is only for input; ordinary writer readiness retains its own meaning.

use super::{IntakeRoute, RuntimeHandoffKind};
use crate::services::tui_o::writer::host;
use crate::services::tui_prompt_dedupe::clear_relaunch;

pub(crate) async fn for_input_placement(provider: &str, channel: u64) -> IntakeRoute {
    placement(provider, Some(channel)).await
}

pub(crate) async fn text_for_input_placement(provider: &str, channel: &str) -> IntakeRoute {
    placement(provider, channel.parse().ok()).await
}

async fn placement(provider: &str, channel: Option<u64>) -> IntakeRoute {
    if !channel.is_some_and(|channel| selected_codex_input(provider, channel)) {
        return match channel {
            Some(channel) => super::route_for_placement(provider, channel),
            None => super::placed(provider, None),
        };
    }
    let provider = provider.to_owned();
    blocking_route(move || super::placed_with(&provider, channel, judge)).await
}

/// A claimed row keeps its adoption; only its current intake eligibility is checked again.
pub(crate) async fn for_claimed_input(
    provider: &str,
    channel: u64,
    row: i64,
    phase: &'static str,
) -> IntakeRoute {
    if !selected_codex_input(provider, channel) {
        return super::route(provider, channel);
    }
    #[cfg(test)]
    if crate::services::claude_tui::source_verify::n2b_mutant("b1b-fanout") {
        return super::route(provider, channel);
    }
    let provider = provider.to_owned();
    blocking_route(move || {
        super::route_parsed_with(
            &provider,
            || Some(channel),
            |provider, channel, kind| judge_at(provider, channel, kind, phase, Some(row)),
        )
    })
    .await
}

/// Advisory discovery only; both claimed-row boundaries observe eligibility again.
pub(crate) async fn held_input_channels(provider: &str) -> Result<Vec<String>, String> {
    if !provider.trim().eq_ignore_ascii_case("codex")
        || !super::super::owned_channels()
            .iter()
            .any(|(channel, _)| selected_codex_input(provider, *channel))
    {
        return Ok(super::held_channels(provider));
    }
    let provider = provider.to_owned();
    tokio::task::spawn_blocking(move || super::held_with(&provider, judge_discovery))
        .await
        .map_err(|error| format!("O input discovery failed: {error}"))
}

pub(crate) fn selected_codex_input(provider: &str, channel: u64) -> bool {
    provider.trim().eq_ignore_ascii_case("codex")
        && channel == crate::services::codex_tui::canary::CANARY_CHANNEL
        && super::super::owned_kind(channel) == Some(RuntimeHandoffKind::CodexTui)
}

async fn blocking_route(work: impl FnOnce() -> IntakeRoute + Send + 'static) -> IntakeRoute {
    #[cfg(test)]
    if crate::services::claude_tui::source_verify::n2b_mutant("b1b-async-route") {
        return work();
    }
    tokio::task::spawn_blocking(work)
        .await
        .unwrap_or_else(|error| IntakeRoute::Hold(format!("O input observation failed: {error}")))
}

fn judge(provider: &str, channel: u64, kind: RuntimeHandoffKind) -> IntakeRoute {
    judge_at(provider, channel, kind, "placement", None)
}

fn judge_discovery(provider: &str, channel: u64, kind: RuntimeHandoffKind) -> IntakeRoute {
    judge_at(provider, channel, kind, "discovery", None)
}

fn judge_at(
    provider: &str,
    channel: u64,
    kind: RuntimeHandoffKind,
    phase: &'static str,
    row: Option<i64>,
) -> IntakeRoute {
    let mut reason = "provider_or_turn_guard";
    let route = super::judge_with(provider, channel, kind, |channel| {
        if kind != RuntimeHandoffKind::CodexTui {
            return super::writer_route(channel);
        }
        #[cfg(test)]
        probe(channel);
        if clear_relaunch::successor_unproved_for_channel(channel) {
            reason = "successor_unproved";
            return IntakeRoute::Hold(format!("O channel {channel} successor_unproved"));
        }
        if super::accepts(channel) {
            return IntakeRoute::Gateway;
        }
        if let Some(refusal) = host::initial_fresh_refusal(channel) {
            reason = refusal;
            return super::writer_unready(channel);
        }
        if clear_relaunch::input_reason_for_channel(channel).is_some() {
            IntakeRoute::Gateway
        } else {
            reason = clear_relaunch::input_hold_reason_for_channel(channel);
            super::writer_unready(channel)
        }
    });
    note(channel, provider, phase, row, reason, &route);
    route
}

fn note(
    channel: u64,
    provider: &str,
    phase: &'static str,
    row: Option<i64>,
    reason: &'static str,
    route: &IntakeRoute,
) {
    type Seen =
        std::collections::BTreeMap<(u64, &'static str), (Option<u64>, Option<i64>, &'static str)>;
    static LAST: std::sync::LazyLock<std::sync::Mutex<Seen>> =
        std::sync::LazyLock::new(Default::default);
    let key = (channel, phase);
    let generation = host::process_readiness().hosted_by(channel);
    let mut last = LAST.lock().unwrap_or_else(|error| error.into_inner());
    if !matches!(route, IntakeRoute::Hold(_)) {
        last.remove(&key);
    } else if last.insert(key, (generation, row, reason)) != Some((generation, row, reason)) {
        tracing::info!(
            channel,
            provider,
            phase,
            reason,
            "[tui_o] input admission held"
        );
    }
}

#[cfg(test)]
pub(crate) struct InputProbe {
    pub channel: u64,
    pub entered: std::sync::mpsc::Sender<std::thread::ThreadId>,
    pub scheduled: tokio::sync::oneshot::Sender<()>,
    pub release: std::sync::mpsc::Receiver<()>,
}

#[cfg(test)]
static PROBE: std::sync::Mutex<Option<InputProbe>> = std::sync::Mutex::new(None);

#[cfg(test)]
pub(crate) struct ProbeGuard;

#[cfg(test)]
pub(crate) fn install_probe(probe: InputProbe) -> ProbeGuard {
    let mut slot = PROBE.lock().unwrap();
    assert!(slot.is_none(), "an O input probe is already installed");
    *slot = Some(probe);
    ProbeGuard
}

#[cfg(test)]
impl Drop for ProbeGuard {
    fn drop(&mut self) {
        PROBE.lock().unwrap().take();
    }
}

#[cfg(test)]
fn probe(channel: u64) {
    let probe = {
        let mut slot = PROBE.lock().unwrap();
        if slot.as_ref().is_some_and(|probe| probe.channel == channel) {
            slot.take()
        } else {
            None
        }
    };
    if let Some(probe) = probe {
        probe.entered.send(std::thread::current().id()).unwrap();
        let _ = probe.scheduled.send(());
        probe
            .release
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("the test must release its O input probe");
    }
}
