//! Placement may precede restoration only for a real, owned and running writer.

use super::*;
use std::time::Duration;

pub(crate) fn initial_fresh_refusal(channel: u64) -> Option<&'static str> {
    let readiness = process_readiness();
    let live = locked(&readiness.live);
    let Some(live) = live.get(&channel) else {
        return Some("host_missing");
    };
    let owned = owned_now(channel, &live.gate);
    #[cfg(test)]
    let owned = owned || crate::services::claude_tui::source_verify::n2b_mutant("b1b-host");
    if !owned {
        return Some("ownership_not_owned");
    }
    if *live.stopping.borrow() {
        return Some("stopping");
    }
    if live.resumed.has_changed().is_err() {
        return Some("actor_closed");
    }
    if live.submission.is_none() {
        return Some("writer_sources_unready");
    }
    None
}

pub(super) fn hold_metadata(store: &ChannelStore) -> Option<&'static str> {
    #[cfg(test)]
    if crate::services::claude_tui::source_verify::n2b_mutant("b1b-meta") {
        return None;
    }
    if !crate::services::tui_prompt_dedupe::clear_relaunch::fresh_scope_eligible(
        store.init().channel,
    ) {
        return None;
    }
    let selection = super::super::suspension::Suspension::default();
    let restored = match selection.restore_current(store) {
        Ok(restored) => restored,
        Err(_) => return Some("store_selection_unready"),
    };
    let channel_ready = crate::services::tui_prompt_dedupe::codex_verified_channel_delivery_allowed(
        store.init().channel,
    );
    let ordinary = super::super::deliver::source_delivery_allowed(
        store.init().channel,
        store
            .cursors()
            .filter(|cursor| {
                let suspended = selection.contains(&cursor.source);
                #[cfg(test)]
                let suspended = suspended
                    && !crate::services::claude_tui::source_verify::n2b_mutant(
                        "b1b-boot-scope-subtract",
                    );
                !cursor.retired && !suspended
            })
            .map(|cursor| cursor.source.clone()),
    );
    #[cfg(test)]
    let ordinary =
        ordinary && !crate::services::claude_tui::source_verify::n2b_mutant("b1b-boot-ordinary");
    if ordinary {
        return None;
    }
    if restored {
        return Some(boot_reason(channel_ready, "store_selection_conflict"));
    }
    if store.cursors().next().is_some() && !matches!(store.binding_checkpoint(), Ok(Some(_))) {
        return Some("store_metadata_unready");
    }
    if store.ledger().unresolved().is_some()
        || super::super::local_retry::pending(store.ledger()).is_some()
        || store.ledger().resume_pieces().next().is_some()
    {
        return Some(boot_reason(channel_ready, "unresolved_work"));
    }
    None
}

fn boot_reason(channel_ready: bool, terminal: &'static str) -> &'static str {
    #[cfg(test)]
    if crate::services::claude_tui::source_verify::n2b_mutant("b1b-boot-gate-terminal") {
        return terminal;
    }
    if channel_ready {
        terminal
    } else {
        "channel_gate_unready"
    }
}

// Allow relay discovery and rehydration before reserving the channel's halt notification.
const BOOT_GATE_GRACE: Duration = Duration::from_secs(10);

pub(super) async fn wait_metadata(
    store: &ChannelStore,
    alarms: &impl AlarmSink,
    stopping: &watch::Receiver<bool>,
) -> bool {
    let channel = store.init().channel;
    let started = tokio::time::Instant::now();
    let grace = BOOT_GATE_GRACE;
    #[cfg(test)]
    let grace = if crate::services::claude_tui::source_verify::n2b_mutant("p2fu-grace-zero") {
        Duration::ZERO
    } else {
        grace
    };
    let mut wait = Duration::from_millis(250);
    let mut pending = false;
    let mut warned = false;
    loop {
        match hold_metadata(store) {
            None => {
                if pending {
                    alarms.resume_pending(channel, false);
                    tracing::info!(
                        elapsed_ms = started.elapsed().as_millis() as u64,
                        "boot gate opened"
                    );
                }
                if warned {
                    alarms.halt_cleared(channel);
                }
                return true;
            }
            Some("channel_gate_unready") => {
                if !pending {
                    alarms.resume_pending(channel, true);
                    pending = true;
                }
                if !warned && started.elapsed() >= grace {
                    let reason = crate::services::tui_prompt_dedupe::codex_verified_hold_reason(
                        crate::services::codex_tui::canary::CANARY_TMUX,
                    );
                    hold(alarms, channel, &format!("channel_gate_unready:{reason}"));
                    warned = true;
                }
            }
            Some(reason) => {
                if pending {
                    alarms.resume_pending(channel, false);
                }
                #[cfg(test)]
                if warned
                    && crate::services::claude_tui::source_verify::n2b_mutant(
                        "p2fu-terminal-reason",
                    )
                {
                    return false;
                }
                hold(alarms, channel, reason);
                return false;
            }
        }
        let delay = if warned {
            wait
        } else {
            wait.min(grace.saturating_sub(started.elapsed()))
        };
        if unless_stopped(stopping, tokio::time::sleep(delay))
            .await
            .is_none()
        {
            if pending {
                alarms.resume_pending(channel, false);
            }
            let clear = warned;
            #[cfg(test)]
            let clear =
                clear && !crate::services::claude_tui::source_verify::n2b_mutant("p2fu-stop-clear");
            if clear {
                alarms.halt_cleared(channel);
            }
            return false;
        }
        wait = (wait * 2).min(resume::FIRST_WAIT);
    }
}

#[cfg(test)]
#[path = "fresh_input_tests.rs"]
mod tests;
