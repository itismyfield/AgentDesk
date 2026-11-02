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
    report(pane, request, channel, timing, Policy::BUSY_ONLY)
}

// The caller limits this policy to Claude tmux channels with confirmed transcript output.
pub(crate) fn inject_report_any_turn(
    pane: &Pane,
    request: &Request<'_>,
    channel: Option<u64>,
    timing: &Timing,
) -> Report {
    let policy = Policy {
        gate: TurnGate::AnyTurn,
        answer: Answer::Declared,
        screen_proven: false,
    };
    report(pane, request, channel, timing, policy)
}

/// The input owner's entry for a running pane: pasted whatever turn the transcript reads, a
/// person's draft parked, and no answer declared; a screen it cannot prove submits is held.
#[cfg(unix)]
pub(crate) fn inject_owned(pane: &Pane, request: &Request<'_>, timing: &Timing) -> Report {
    let policy = Policy {
        gate: TurnGate::AnyTurn,
        answer: Answer::FromRow,
        screen_proven: true,
    };
    inject_gated_with(pane, request, timing, Some(person_draft), policy)
}

/// A draft as Herdr's owner reads it: an injected frame or an existing stash is never parked.
#[cfg(unix)]
fn person_draft(capture: &str) -> bool {
    draft_sighting(capture)
        == crate::services::claude_tui::composer_lock::DraftSighting::PersonDraft
}

fn report(
    pane: &Pane,
    request: &Request<'_>,
    channel: Option<u64>,
    timing: &Timing,
    policy: Policy,
) -> Report {
    #[cfg(all(test, unix))]
    if !stash_tests::USE_PRODUCTION_CHANNELS.get() {
        return inject_gated_with(
            pane,
            request,
            timing,
            listed(stash_channel_listed(
                channel,
                &stash_tests::configured_channels(),
            )),
            policy,
        );
    }
    inject_gated_with(
        pane,
        request,
        timing,
        listed(stash_channel_listed(channel, stash_channels_configured())),
        policy,
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
        super::listed(stash_channel_listed(channel, listed)),
        Policy::BUSY_ONLY,
    )
}

#[cfg(all(test, unix))]
pub(super) fn inject_gated(
    pane: &Pane,
    request: &Request<'_>,
    timing: &Timing,
    stash: bool,
) -> Report {
    inject_gated_with(pane, request, timing, listed(stash), Policy::BUSY_ONLY)
}

fn inject_gated_with(
    pane: &Pane,
    request: &Request<'_>,
    timing: &Timing,
    park: Park,
    policy: Policy,
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
                let own = matches!(policy.answer, Answer::Declared);
                // The hook may observe the frame before its submission receipt returns.
                if own {
                    dedupe::record_discord_originated_prompt("claude", request.session, &text);
                }
                let report = inject_locked(pane, request, &text, timing, park, policy);
                if own && matches!(&report.outcome, Outcome::NotSent(_)) {
                    dedupe::remove_discord_originated_prompt("claude", request.session, &text);
                }
                report
            })
        },
    );
    locked.unwrap_or(Report::not_sent(Veto::LockContended))
}
