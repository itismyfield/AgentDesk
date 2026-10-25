//! Entry policies share the same composer lock, safety checks and submission transaction.
use super::*;

#[cfg(test)]
pub(crate) fn inject(pane: &Pane, request: &Request<'_>, timing: &Timing) -> Outcome {
    inject_report(pane, request, None, timing).outcome
}

pub(crate) fn inject_report(
    pane: &Pane,
    request: &Request<'_>,
    channel: Option<u64>,
    timing: &Timing,
) -> Report {
    report(pane, request, channel, timing, TurnGate::BusyOnly)
}

// The caller limits this policy to Claude tmux channels with confirmed transcript output.
pub(crate) fn inject_report_any_turn(
    pane: &Pane,
    request: &Request<'_>,
    channel: Option<u64>,
    timing: &Timing,
) -> Report {
    report(pane, request, channel, timing, TurnGate::AnyTurn)
}

fn report(
    pane: &Pane,
    request: &Request<'_>,
    channel: Option<u64>,
    timing: &Timing,
    turn_gate: TurnGate,
) -> Report {
    #[cfg(all(test, unix))]
    if !stash_tests::USE_PRODUCTION_CHANNELS.get() {
        return inject_gated_with(
            pane,
            request,
            timing,
            stash_channel_listed(channel, &stash_tests::configured_channels()),
            turn_gate,
        );
    }
    inject_gated_with(
        pane,
        request,
        timing,
        stash_channel_listed(channel, stash_channels_configured()),
        turn_gate,
    )
}

#[cfg(all(test, unix))]
pub(super) fn inject_listed(
    pane: &Pane,
    request: &Request<'_>,
    channel: Option<u64>,
    listed: &[u64],
    timing: &Timing,
) -> Report {
    inject_gated_with(
        pane,
        request,
        timing,
        stash_channel_listed(channel, listed),
        TurnGate::BusyOnly,
    )
}

#[cfg(all(test, unix))]
pub(super) fn inject_gated(
    pane: &Pane,
    request: &Request<'_>,
    timing: &Timing,
    stash: bool,
) -> Report {
    inject_gated_with(pane, request, timing, stash, TurnGate::BusyOnly)
}

fn inject_gated_with(
    pane: &Pane,
    request: &Request<'_>,
    timing: &Timing,
    stash: bool,
    turn_gate: TurnGate,
) -> Report {
    let text = frame(
        request.source,
        request.author,
        request.name,
        request.nonce,
        request.text,
    );
    // Both names reach tmux command strings, so only plain characters are accepted.
    let plain = |value: &str, extra: &[char]| {
        !value.is_empty()
            && value
                .chars()
                .all(|c| c.is_alphanumeric() || extra.contains(&c))
    };
    if request.text.trim().is_empty()
        || text.len() > MAX_INPUT_BYTES
        || !plain(request.session, &['-', '_'])
        || !request.nonce.chars().all(|c| c.is_ascii_alphanumeric())
        || request.nonce.is_empty()
    {
        return Report::not_sent(Veto::InvalidInput);
    }
    let started = Instant::now();
    // The short key sequence runs under the lock: one stash at a time.
    let locked = at_offsets(
        timing.lock_retries,
        || started.elapsed(),
        std::thread::sleep,
        || {
            super::super::composer_lock::try_with_composer_mutation_lock(request.session, || {
                use crate::services::tui_prompt_dedupe as dedupe;
                let own = matches!(turn_gate, TurnGate::AnyTurn);
                // The hook may observe the frame before its submission receipt returns.
                if own {
                    dedupe::record_discord_originated_prompt("claude", request.session, &text);
                }
                let report = inject_locked(pane, request, &text, timing, stash, turn_gate);
                if own && matches!(&report.outcome, Outcome::NotSent(_)) {
                    dedupe::remove_discord_originated_prompt("claude", request.session, &text);
                }
                report
            })
        },
    );
    locked.unwrap_or(Report::not_sent(Veto::LockContended))
}
