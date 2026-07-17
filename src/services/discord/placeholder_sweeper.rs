//! #1115 placeholder stall sweeper.
//!
//! Background safety net for the case where neither the in-stream lifecycle
//! finalization (#1113) nor the in-band terminal status edits ever fire —
//! e.g. the bridge process is stuck on an external IPC, the JSONL file
//! rotates out from under the parser, or the source Claude Code session is
//! killed without emitting a terminal event. The sweeper periodically scans
//! every persisted inflight state per provider; for placeholders whose
//! `updated_at` has not advanced in a configurable window, it edits the
//! Discord message into a "stalled" or "abandoned" state and clears the
//! inflight state only after terminal-safe or completed cleanup. Uncertain
//! owner evidence preserves the row for a later retry.
//!
//! Scope notes for the initial landing:
//! - AgentDesk-tracked inflight states only. Operator-level Claude Code
//!   sessions that never wrote an inflight state file are out of scope and
//!   tracked as a follow-up to the #1112 epic.
//! - Process-alive (`pid` / session close) detection is similarly deferred.
//!   Time-based staleness is the v1 trigger.

use std::collections::HashSet;
use std::sync::Arc;

use poise::serenity_prelude as serenity;

use super::SharedData;
use super::formatting::{
    MonitorHandoffReason, MonitorHandoffStatus, PLACEHOLDER_PROBE_MARKER,
    build_monitor_handoff_placeholder, build_monitor_handoff_placeholder_with_context,
};
use super::gateway::edit_outbound_message;
use super::inflight::{
    InflightTurnState, delete_inflight_state_file, emit_reap_abandoned_rebind_origin,
    load_inflight_states_for_sweep, opt_channel_id, parse_started_at_unix,
    reap_abandoned_rebind_origin_locked, should_reap_abandoned_rebind_origin,
    sweep_reap_dead_watcher_rebind_origin,
};
use crate::services::provider::ProviderKind;

mod abandon_guard;
use abandon_guard::{
    AbandonedTmuxCleanupDecision, abandoned_tmux_cleanup_decision_for,
    finalize_owner_dead_cleanup_if_same_turn,
};

/// Age (seconds since `updated_at`) at which a placeholder is treated as
/// stalled. Below this threshold the sweeper does nothing.
///
/// #2438 (#2427 final): bumped 60 → 300 after the four explicit-signal
/// wires landed (D: TurnCompleted, A: pane death, B: heartbeat-gap
/// inflight sweeper, C: generation-mismatch bulk invalidate). The
/// sweeper is now a pure safety net; the explicit signals catch the
/// common "completion hook missed" cases within seconds, so the
/// time-based stall edit only fires when *all four* signals are silent
/// — i.e. a real bug. 300s also matches `INFLIGHT_MAX_AGE_SECS` so the
/// stall edit cannot precede the load path's own GC.
pub(crate) const STALL_THRESHOLD_SECS: u64 = 300;

/// Age at which the placeholder is treated as abandoned. The sweeper edits
/// the message to its terminal "abandoned" form and clears inflight state only
/// when tmux cleanup is terminal-safe or actually completes.
///
/// #2438 (#2427 final): bumped 300 → 1800 (30 min). At this point the
/// sweeper is the **last** layer: every explicit signal that should
/// have cleaned the inflight row has had ample time to fire. A row
/// that reaches 30 minutes without a cleanup signal is a leaked
/// inflight (hook miss, missing wire, unexpected silent stall) and we
/// want the warn log to flag it as such for triage. False-positive
/// cleanup of a 25-minute legitimate long-running tool is far worse
/// than a 30-minute extra wait before the eventual safety-net abort,
/// so the threshold is set conservatively high.
pub(crate) const ABANDON_THRESHOLD_SECS: u64 = 1800;

/// Polling interval for `spawn_placeholder_sweeper`. Picked low enough that
/// the stall transition is observed within ≤ ~1 polling delay, but high
/// enough that we do not spam Discord edits on idle startups.
pub(crate) const SWEEP_INTERVAL_SECS: u64 = 30;

/// Emit a low-volume liveness log even when no placeholder transitions were
/// found. This keeps ops from confusing "healthy but idle" with "sweeper died".
pub(crate) const SWEEP_HEARTBEAT_INTERVAL_SWEEPS: u64 = 120;

/// Initial delay before the first sweep runs after dcserver bootstrap. Skips
/// the boot-up window where active turns from the previous generation are
/// still being recovered and may legitimately appear stalled while
/// inflight-state migration is in progress.
///
/// #2438 (#2427 final): bumped 90 → 180 to absorb the recovery-engine
/// retry budget (#2428 H5) at boot. Recovery now needs more time to
/// settle before the sweeper can safely classify a row as stalled
/// without racing the recovery sweep itself.
pub(crate) const INITIAL_DELAY_SECS: u64 = 180;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SweepDecision {
    Active,
    Stalled,
    Abandoned,
}

fn classify_age(age_secs: u64) -> SweepDecision {
    if age_secs >= ABANDON_THRESHOLD_SECS {
        SweepDecision::Abandoned
    } else if age_secs >= STALL_THRESHOLD_SECS {
        SweepDecision::Stalled
    } else {
        SweepDecision::Active
    }
}

fn build_stalled_placeholder(state: &InflightTurnState) -> String {
    let started_at_unix = parse_started_at_unix(&state.started_at).unwrap_or_else(|| {
        // Fall back to now only for malformed legacy state. The normal path
        // uses persisted started_at so the stalled content stays stable.
        chrono::Utc::now().timestamp()
    });
    build_monitor_handoff_placeholder_with_context(
        MonitorHandoffStatus::Stalled,
        MonitorHandoffReason::AsyncDispatch,
        started_at_unix,
        state.current_tool_line.as_deref(),
        None,
        Some("stalled — no stream progress"),
        None,
        None,
        None,
    )
}

fn build_abandoned_placeholder(state: &InflightTurnState) -> String {
    let started_at_unix =
        parse_started_at_unix(&state.started_at).unwrap_or_else(|| chrono::Utc::now().timestamp());
    build_monitor_handoff_placeholder(
        MonitorHandoffStatus::Aborted,
        MonitorHandoffReason::AsyncDispatch,
        started_at_unix,
        state.current_tool_line.as_deref(),
        None,
    )
}

async fn edit_placeholder_safe(
    http: &Arc<serenity::Http>,
    shared: &Arc<SharedData>,
    channel_id: u64,
    message_id: u64,
    content: &str,
) -> bool {
    if channel_id == 0 || message_id == 0 {
        return false;
    }
    let channel = serenity::ChannelId::new(channel_id);
    let message = serenity::MessageId::new(message_id);
    edit_outbound_message(http.clone(), shared.clone(), channel, message, content)
        .await
        .is_ok()
}

/// Outcome of pre-flight checking whether the placeholder message on Discord
/// is still a placeholder (and therefore safe to overwrite with an abandoned
/// badge) or has already been replaced with a delivered response.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::services::discord) enum PlaceholderProbe {
    /// Known placeholder content; an abandoned edit is permitted.
    StillPlaceholder,
    /// Real response content; preserve it and finalize as delivered.
    AlreadyDelivered,
    /// Permanent 404 / 403 / 410; editing cannot succeed.
    MessageGone,
    /// Transient/unknown failure; preserve everything for a later retry.
    ProbeFailed,
}

/// True for HTTP status codes that signal the placeholder message will
/// never come back: 404 NOT_FOUND, 403 FORBIDDEN, 410 GONE. Anything else
/// (5xx, 429 rate-limit, no status at all) is treated as transient.
///
/// Split out so the classification can be unit-tested without constructing
/// the `#[non_exhaustive]` `serenity::http::ErrorResponse`. #3293 reuses the
/// same allowlist for the recovery terminal-relay outcome classifier.
pub(in crate::services::discord) fn is_permanent_message_gone_status(status: u16) -> bool {
    matches!(status, 404 | 403 | 410)
}

/// Classify a `serenity::Error` from `Http::get_message` into a permanent
/// "message is gone" (404 / 403 / 410) vs a transient failure that should
/// be retried on the next sweep pass.
fn classify_get_message_error(err: &serenity::Error) -> PlaceholderProbe {
    if let serenity::Error::Http(http_err) = err {
        if let Some(status) = http_err.status_code() {
            if is_permanent_message_gone_status(status.as_u16()) {
                return PlaceholderProbe::MessageGone;
            }
        }
    }
    PlaceholderProbe::ProbeFailed
}

/// Fetch the current Discord message content and classify whether it is
/// still a placeholder. The fetch itself uses the same `http` handle as the
/// sweeper edits, so we inherit the same rate-limit / proxy behavior.
///
/// Transient probe failures (network errors, rate limits, 5xx) return
/// `ProbeFailed` — callers must NOT take destructive action in that case.
pub(in crate::services::discord) async fn probe_placeholder_state(
    http: &Arc<serenity::Http>,
    channel_id: u64,
    message_id: u64,
) -> PlaceholderProbe {
    if channel_id == 0 || message_id == 0 {
        // No addressable message at all → treat as permanently gone so
        // the cap-bounded controller map does not retain a zero row.
        return PlaceholderProbe::MessageGone;
    }
    let channel = serenity::ChannelId::new(channel_id);
    let message = serenity::MessageId::new(message_id);
    match http.get_message(channel, message).await {
        Ok(msg) => {
            if is_message_still_placeholder(&msg.content) {
                PlaceholderProbe::StillPlaceholder
            } else {
                PlaceholderProbe::AlreadyDelivered
            }
        }
        Err(err) => {
            let outcome = classify_get_message_error(&err);
            match outcome {
                PlaceholderProbe::MessageGone => {
                    tracing::debug!(
                        "[placeholder_sweeper] message gone for {}/{} (permanent: {})",
                        channel_id,
                        message_id,
                        err
                    );
                }
                _ => {
                    tracing::debug!(
                        "[placeholder_sweeper] probe failed for {}/{} (transient — \
                         will retry next sweep): {}",
                        channel_id,
                        message_id,
                        err
                    );
                }
            }
            outcome
        }
    }
}

/// True when `content` still looks like a placeholder card the sweeper itself
/// (or the streaming pipeline) might have produced — i.e. not a user-facing
/// response body. Conservative: only known placeholder shapes pass.
///
/// Patterns recognised as placeholder:
///   - Streaming spinner block: starts with one of the braille spinner
///     glyphs followed by a space, e.g. `⠋ Processing...` or
///     `⠹ ⚙ Bash: cargo build`. Produced by
///     [`build_placeholder_status_block`] / [`build_processing_status_block`].
///   - Monitor handoff card: the authoritative signal is the structured
///     [`PLACEHOLDER_PROBE_MARKER`] embedded by `monitor_handoff_header`. Every
///     card emitted since #2896 carries it, so detection no longer depends on
///     the card's locale-specific header/footer prose.
///   - Legacy fallback (#3031C): pre-marker cards still in flight are matched by
///     the card's *structural* scaffold — the `> **시작**: <t:…:R>` started-at
///     blockquote plus another `> **…**:` field line — never by exact Korean
///     header/footer strings. This drops the manual cross-file lockstep with
///     `monitor_handoff_header` in `formatting.rs` while still protecting the
///     dwindling set of unmarked legacy cards.
///
/// Anything else (real prose, code blocks, embeds rendered as text) is
/// treated as a delivered response and protected from sweeper overwrite.
pub(in crate::services::discord) fn is_message_still_placeholder(content: &str) -> bool {
    const SPINNER_FRAMES: &[char] = &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

    let trimmed = content.trim_start();
    if trimmed.is_empty() {
        // Empty / whitespace-only message: nothing user-visible to protect.
        // Treat as still-placeholder so the sweeper proceeds with the abort
        // edit (which gives the user a clearer terminal state).
        return true;
    }

    let mut chars = trimmed.chars();
    if let Some(first) = chars.next() {
        if SPINNER_FRAMES.contains(&first) {
            // Spinner block must be `{spinner}{whitespace}…` to count.
            if chars.next().map(|c| c.is_whitespace()).unwrap_or(false) {
                return true;
            }
        }
    }

    // Authoritative, locale-independent signal: the structured probe marker that
    // `monitor_handoff_header` embeds in every placeholder card.
    if trimmed.contains(PLACEHOLDER_PROBE_MARKER) {
        return true;
    }

    // #3031C — minimal legacy fallback for pre-marker cards still in flight.
    // Matches the card's structural scaffold only (the `> **시작**: <t:…:R>`
    // started-at blockquote + at least one other `> **…**:` field line), so it
    // stays correct regardless of header/footer wording and requires no manual
    // lockstep with `formatting.rs`. First-line header matching is intentionally
    // gone (it caused #2877 false StillPlaceholder classifications).
    let lines = trimmed.lines().collect::<Vec<_>>();
    if legacy_handoff_card_shape(&lines) {
        return true;
    }

    false
}

/// #3031C — locale-independent structural detector for legacy (pre-marker)
/// handoff cards. Keys off the markdown blockquote scaffold the card always
/// renders rather than any translatable header/footer prose.
fn legacy_handoff_card_shape(lines: &[&str]) -> bool {
    let has_started_at = lines
        .iter()
        .any(|line| line.trim().starts_with("> **") && line.contains(": <t:"));
    // A second blockquote field (도구/사유/요약 in any locale) distinguishes the
    // card scaffold from an arbitrary message that merely quotes a timestamp.
    let blockquote_field_lines = lines
        .iter()
        .filter(|line| {
            let line = line.trim();
            line.starts_with("> **") && line.contains("**:")
        })
        .count();
    has_started_at && blockquote_field_lines >= 2
}

/// Run a single sweep pass for the given provider. Public for testability —
/// callers in the bootstrap path schedule this on a fixed cadence via
/// `spawn_placeholder_sweeper`.
async fn run_placeholder_sweep_pass(
    http: &Arc<serenity::Http>,
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    stalled_tracker: &mut StalledEditTracker,
) -> SweepPassReport {
    let mut report = SweepPassReport::default();
    let sweep_started_before = std::time::Instant::now();
    let states = load_inflight_states_for_sweep(provider);
    report.scanned = states.len();
    stalled_tracker.retain_live(provider, &states);
    for (state, age_secs) in states {
        if state.rebind_origin {
            // #3581: a rebind-origin inflight has no placeholder to edit (skipped)
            // — but an abandoned, never-progressed orphan (STALL-WATCHDOG respawn)
            // would live forever and wedge turn-start. Reap it once past its
            // deadline; `should_reap_abandoned_rebind_origin`'s strict conjunction
            // keeps any live/adopted rebind untouched, and the locked helper
            // re-validates under the sidecar lock so a racing live intake/TUI claim
            // is never clobbered (codex TOCTOU). `age_secs` = file-mtime age.
            let current_generation = super::runtime_store::load_generation();
            if should_reap_abandoned_rebind_origin(&state, age_secs, current_generation)
                && reap_abandoned_rebind_origin_locked(provider, &state, current_generation)
            {
                emit_reap_abandoned_rebind_origin(
                    provider,
                    &state,
                    age_secs,
                    current_generation,
                    "placeholder_sweep_deadline",
                );
                report.abandoned += 1;
            } else if sweep_reap_dead_watcher_rebind_origin(
                provider,
                &state,
                age_secs,
                current_generation,
            )
            .await
            {
                // #3635: the None-owner predicate above can NEVER touch a
                // Watcher-owned rebind orphan (the #897 birth shape), so it leaked
                // forever after its watcher died. `sweep_reap_dead_watcher_*`
                // reaps it ONLY when a runtime-liveness probe proves the watcher
                // dead (live watcher preserved per #3154/#3540); this warm path
                // is the sole carrier of the liveness gate — see the helper.
                emit_reap_abandoned_rebind_origin(
                    provider,
                    &state,
                    age_secs,
                    current_generation,
                    "placeholder_sweep_dead_watcher",
                );
                report.abandoned += 1;
            }
            continue;
        }
        // #3003: reclaim an orphaned status-panel-v2 BEFORE the placeholder skips
        // below — a panel-only row can have `current_msg_id == 0` or a non-empty
        // `full_response`, both of which the placeholder sweep skips. Planned
        // restart/hot-swap rows are left for recovery (matching the placeholder
        // restart_mode guard).
        if state.restart_mode.is_none() {
            sweep_orphan_status_panel(
                http,
                shared,
                provider,
                &shared.token_hash,
                &state,
                age_secs,
                sweep_started_before,
                &mut report,
            )
            .await;
        }
        if state.current_msg_id == 0 || state.channel_id == 0 {
            continue;
        }
        // Skip planned restart / hot-swap inflights. Their cleanup TTL is
        // intentionally extended (DrainRestart 1800s, HotSwapHandoff 900s)
        // by `inflight::load_inflight_states_from_root` so recovery can pick
        // them up after a restart. The sweeper would otherwise edit them as
        // abandoned and delete the state file, defeating recovery.
        if state.restart_mode.is_some() {
            continue;
        }
        // Only sweep messages that are still pure placeholders. Once any
        // real response text has been streamed, `current_msg_id` points at
        // a partially delivered response; overwriting it with a stalled or
        // abandoned label would corrupt user-visible output for healthy
        // long-running tools that simply haven't emitted a new event in a
        // while.
        //
        // The "stalled after partial output" case is intentionally left for
        // a follow-up: it requires an append (rather than replace) strategy
        // so the partial response stays visible above the badge.
        // codex round-2 P2 on PR #1308: a long-running tool placeholder may
        // be opened after assistant prose has already streamed, so
        // `full_response` is non-empty even though `current_msg_id` now points
        // at a pure background card. Honour the explicit flag from the turn
        // loop and let those flow through to the stalled/abandoned branches.
        if !state.long_running_placeholder_active
            && (!state.full_response.is_empty() || state.response_sent_offset > 0)
        {
            continue;
        }
        // Re-stat guard for the EDIT path: between
        // `load_inflight_states_for_sweep` and the awaited Discord edit, the
        // owning turn may complete entirely (state file removed), have a
        // brand-new turn replace it (different user_msg_id), or stream the
        // first response chunk (mtime advances). Skip the edit (and the
        // abandoned-branch evict) unless the same turn we snapshotted is
        // still on disk and still stale.
        if !inflight_state_still_same_turn(provider, &state, age_secs) {
            continue;
        }
        // codex round-8 P1 on PR #1308: long-running placeholders rely on the
        // turn loop bumping `updated_at` every 30s (see
        // `LIVE_LONG_RUN_HEARTBEAT_INTERVAL` in `turn_bridge::mod`) so the
        // sweeper can still abandon them if the owning process actually dies
        // — only the live ones keep advancing mtime. Treat all states
        // uniformly here.
        let decision = classify_age(age_secs);
        // #2415: defensive probe. The streaming pipeline can hand off live
        // relay to a delegated owner (watcher / standby) that updates the
        // Discord message in place without mirroring `full_response` /
        // `response_sent_offset` back into the persisted inflight state.
        // The placeholder-only gate above therefore lets the row through
        // even though the user already received the answer. Fetch the
        // current Discord content BEFORE any destructive edit (stalled at
        // 60s OR abandoned at 300s) and skip the overwrite when the
        // message body no longer looks like a placeholder.
        //
        // Codex round 1 HIGH on PR #2417:
        //   1. The probe must gate the Stalled branch too — same data-loss
        //      class kicks in 60s after handoff, not just 300s.
        //   2. Transient probe errors (5xx / network / rate limit) MUST
        //      leave the inflight state untouched so a later sweep can
        //      retry. Only permanent failures (404 / 403 / 410) classify
        //      as MessageGone and trigger eviction.
        let probe = if matches!(decision, SweepDecision::Stalled | SweepDecision::Abandoned) {
            probe_placeholder_state(http, state.channel_id, state.current_msg_id).await
        } else {
            PlaceholderProbe::StillPlaceholder
        };
        // Transient probe failure: leave everything for next sweep.
        // Applies to both stalled and abandoned classifications.
        if matches!(probe, PlaceholderProbe::ProbeFailed) {
            tracing::debug!(
                "[placeholder_sweeper] skipping {:?} pass for {}/{} — probe failed, \
                 will retry next sweep (#2415)",
                decision,
                state.channel_id,
                state.current_msg_id
            );
            continue;
        }
        match decision {
            SweepDecision::Active => {}
            SweepDecision::Stalled => {
                // Codex round 1 HIGH-1 on PR #2417: if the Discord
                // content has already been replaced by a real response
                // (delivered class), the stalled-edit at 60s would clobber
                // it just like the abandoned path used to. Skip the edit.
                // Leave the inflight state on disk — the abandoned branch
                // at 300s will probe again and finalize eviction (or
                // re-skip on transient failure).
                if matches!(probe, PlaceholderProbe::AlreadyDelivered) {
                    tracing::info!(
                        "[placeholder_sweeper] skipped stalled overwrite for {}/{} — \
                         content already delivered, deferring state eviction to \
                         abandoned pass (#2415)",
                        state.channel_id,
                        state.current_msg_id
                    );
                    continue;
                }
                // MessageGone @ Stalled: the message is permanently gone.
                // An edit would fail anyway. Leave state for the abandoned
                // pass to fully evict — Stalled is purely advisory and
                // does not own eviction semantics.
                if matches!(probe, PlaceholderProbe::MessageGone) {
                    continue;
                }
                if !stalled_tracker.mark_pending(provider, &state) {
                    continue;
                }
                let text = build_stalled_placeholder(&state);
                if edit_placeholder_safe(
                    http,
                    shared,
                    state.channel_id,
                    state.current_msg_id,
                    &text,
                )
                .await
                {
                    stalled_tracker.mark_edited(provider, &state);
                    report.stalled += 1;
                } else {
                    stalled_tracker.clear_pending(provider, &state);
                }
            }
            SweepDecision::Abandoned => {
                match probe {
                    PlaceholderProbe::AlreadyDelivered => {
                        // Response already on screen: terminal delivery is certain.
                        // Release the matching mailbox/inflight even if its reusable
                        // tmux pane remains live; preserve that session itself.
                        abandon_guard::finalize_probe_cleanup_if_same_turn(
                            shared,
                            provider,
                            &state,
                            age_secs,
                            sweep_started_before,
                            probe,
                        )
                        .await;
                        tracing::info!(
                            "[placeholder_sweeper] skipped abandon overwrite for {}/{} — \
                             content already delivered; cleanup policy applied (#2415)",
                            state.channel_id,
                            state.current_msg_id
                        );
                        continue;
                    }
                    PlaceholderProbe::MessageGone => {
                        // The Discord message is permanently unreachable, but that
                        // is not terminal-delivery evidence. Re-probe owner death
                        // after the awaited GET before touching mailbox/state.
                        abandon_guard::finalize_probe_cleanup_if_same_turn(
                            shared,
                            provider,
                            &state,
                            age_secs,
                            sweep_started_before,
                            probe,
                        )
                        .await;
                        continue;
                    }
                    PlaceholderProbe::ProbeFailed => {
                        // Already handled above by the early `continue`.
                        // This arm is unreachable but kept for exhaustive
                        // matching clarity.
                        continue;
                    }
                    PlaceholderProbe::StillPlaceholder => {
                        // Fall through to the original abort-edit path.
                    }
                }
                let cleanup_decision = abandoned_tmux_cleanup_decision_for(&state).await;
                if cleanup_decision == AbandonedTmuxCleanupDecision::PreserveRetry {
                    continue;
                }
                // #2438 (#2427 final): time-based abandon is now a pure
                // safety net — the four explicit-signal wires (D
                // TurnCompleted / A pane death / B heartbeat-gap / C
                // generation-mismatch) should have evicted this row long
                // before age reaches `ABANDON_THRESHOLD_SECS` (1800s).
                // A row that lands here is a leaked inflight. Log it as
                // SAFETY-NET so triage can hunt the missing hook.
                tracing::warn!(
                    "[sweeper SAFETY-NET] abandoning inflight age={age_secs}s — \
                     explicit cleanup signal missed for {provider}/{channel_id} (msg {msg_id}); \
                     investigate (pane_dead/generation/heartbeat hooks)",
                    age_secs = age_secs,
                    provider = provider.as_str(),
                    channel_id = state.channel_id,
                    msg_id = state.current_msg_id,
                );
                // Re-probe immediately before the edit so a revived session wins.
                if !cleanup_decision.allows_discord_cleanup()
                    || !abandoned_tmux_cleanup_decision_for(&state)
                        .await
                        .allows_discord_cleanup()
                    || !inflight_state_still_same_turn(provider, &state, age_secs)
                {
                    continue;
                }
                let text = build_abandoned_placeholder(&state);
                let edited = edit_placeholder_safe(
                    http,
                    shared,
                    state.channel_id,
                    state.current_msg_id,
                    &text,
                )
                .await;
                // Recheck after the awaited edit covers four concerns:
                //   1. Edit failure (rate limit / 5xx): leave state for the
                //      next pass to retry.
                //   2. New turn raced in during the await (different
                //      user_msg_id): do not abandon the new turn's mailbox
                //      or delete its state.
                //   3. Original turn completed during the await (state file
                //      gone): turn_bridge already finalized its mailbox —
                //      calling mailbox_finish_turn again would no-op or
                //      corrupt a freshly started follow-up turn.
                //   4. The tmux pane revived during the edit: owner-death planning
                //      returns PreserveRetry, which keeps both mailbox and row.
                // `inflight_state_still_same_turn` covers (2) and (3); edit
                // success covers (1), and the production cleanup plan covers (4).
                if edited
                    && finalize_owner_dead_cleanup_if_same_turn(
                        shared,
                        provider,
                        &state,
                        age_secs,
                        sweep_started_before,
                        true,
                    )
                    .await
                {
                    report.abandoned += 1;
                }
            }
        }
    }
    report
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct StalledEditKey {
    provider: String,
    channel_id: u64,
    message_id: u64,
    updated_at: String,
}

impl StalledEditKey {
    fn new(provider: &ProviderKind, state: &InflightTurnState) -> Self {
        Self {
            provider: provider.as_str().to_string(),
            channel_id: state.channel_id,
            message_id: state.current_msg_id,
            updated_at: state.updated_at.clone(),
        }
    }
}

#[derive(Debug, Default)]
struct StalledEditTracker {
    edited: HashSet<StalledEditKey>,
    pending: HashSet<StalledEditKey>,
}

impl StalledEditTracker {
    fn retain_live(&mut self, provider: &ProviderKind, states: &[(InflightTurnState, u64)]) {
        let provider_id = provider.as_str();
        let live: HashSet<StalledEditKey> = states
            .iter()
            .map(|(state, _)| StalledEditKey::new(provider, state))
            .collect();
        self.edited
            .retain(|key| key.provider != provider_id || live.contains(key));
        self.pending
            .retain(|key| key.provider != provider_id || live.contains(key));
    }

    fn mark_pending(&mut self, provider: &ProviderKind, state: &InflightTurnState) -> bool {
        let key = StalledEditKey::new(provider, state);
        if self.edited.contains(&key) || self.pending.contains(&key) {
            return false;
        }
        self.pending.insert(key);
        true
    }

    fn mark_edited(&mut self, provider: &ProviderKind, state: &InflightTurnState) {
        let key = StalledEditKey::new(provider, state);
        self.pending.remove(&key);
        self.edited.insert(key);
    }

    fn clear_pending(&mut self, provider: &ProviderKind, state: &InflightTurnState) {
        self.pending.remove(&StalledEditKey::new(provider, state));
    }
}

/// True when the inflight state on disk still names the same turn and its
/// mtime is not significantly fresher than the sweep snapshot.
fn inflight_state_still_same_turn(
    provider: &ProviderKind,
    snapshot: &InflightTurnState,
    snapshot_age_secs: u64,
) -> bool {
    const SLACK_SECS: u64 = 5;
    let states = load_inflight_states_for_sweep(provider);
    let Some((current, current_age)) = states
        .into_iter()
        .find(|(state, _)| state.channel_id == snapshot.channel_id)
    else {
        // File gone — original turn completed (turn_bridge cleared its
        // own state on success/cancel). Do not act: any edit would target
        // a message the completing turn already owned, and a mailbox
        // finalize would race a fresh follow-up turn.
        return false;
    };
    if current.user_msg_id != snapshot.user_msg_id
        || current.current_msg_id != snapshot.current_msg_id
    {
        return false;
    }
    observed_age_still_stale(snapshot_age_secs, current_age, SLACK_SECS)
}

fn observed_age_still_stale(
    snapshot_age_secs: u64,
    current_age_secs: u64,
    slack_secs: u64,
) -> bool {
    current_age_secs + slack_secs >= snapshot_age_secs
}

/// #3003 durable safety net: reclaim an orphaned status-panel-v2 message left on
/// an abandoned inflight row.
///
/// A watcher-created TUI-direct panel whose turn never completed — e.g. a
/// transient Discord delete failure during the turn, or the owning process died
/// before the in-loop reclaim ran — keeps its `status_message_id` on the
/// lingering inflight and would otherwise stay stuck at "계속 처리 중". This is
/// the panel counterpart to the placeholder (`current_msg_id`) sweep below, and
/// runs even for rows the placeholder sweep skips (`current_msg_id == 0`, or a
/// non-empty `full_response`).
///
/// Mirrors the placeholder abandon semantics: act only at the time-based abandon
/// threshold, normalise synthetic-headless ids, guard against a replacement
/// turn, and treat transient Discord errors as retryable (id preserved for a
/// later pass). A permanent gone status (404/403/410) is treated as success so
/// the persisted id is cleared. Returns true when a real delete committed.
/// Gate for [`sweep_orphan_status_panel`]: returns the real Discord panel id to
/// reclaim, or `None` when this row is not an abandoned panel-bearing row.
/// Pure (no IO) so the threshold / synthetic-id / channel gating is unit-tested.
fn panel_reclaim_target(state: &InflightTurnState, age_secs: u64) -> Option<serenity::MessageId> {
    if !matches!(classify_age(age_secs), SweepDecision::Abandoned) {
        return None;
    }
    if state.channel_id == 0 {
        return None;
    }
    super::turn_bridge::normalize_status_panel_message_id(
        state.status_message_id.map(serenity::MessageId::new),
    )
}

async fn sweep_orphan_status_panel(
    http: &Arc<serenity::Http>,
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    token_hash: &str,
    state: &InflightTurnState,
    age_secs: u64,
    sweep_started_before: std::time::Instant,
    report: &mut SweepPassReport,
) {
    let Some(panel_msg) = panel_reclaim_target(state, age_secs) else {
        return;
    };
    // The abandoned-state policy is fail-closed: live, recent, or uncertain
    // evidence keeps the Discord panel and inflight row for a later retry.
    // Panel-only/TUI-direct rows have no real user-message identity to finalize,
    // so only those terminal-marker rows can discard their stale marker.
    if !abandoned_tmux_cleanup_decision_for(state)
        .await
        .allows_discord_cleanup()
    {
        return;
    }
    // Do not delete a panel a replacement turn now owns, or one whose turn has
    // already completed (state file gone).
    if !inflight_state_still_same_turn(provider, state, age_secs) {
        return;
    }
    let Some(channel) = opt_channel_id(state.channel_id) else {
        return;
    };
    if super::placeholder_cleanup::committed_terminal_panel_anchor_skip(
        &shared.ui.placeholder_cleanup,
        provider,
        channel,
        panel_msg,
        state,
    ) {
        return; // #3607: a committed terminal cleanup owns this panel.
    }
    let delete_result = channel.delete_message(http, panel_msg).await;
    super::placeholder_cleanup::emit_orphan_panel_sweep_delete(
        provider,
        channel,
        panel_msg,
        &delete_result,
    );
    let committed = match delete_result {
        Ok(_) => true,
        Err(serenity::Error::Http(http_err))
            if http_err
                .status_code()
                .is_some_and(|status| is_permanent_message_gone_status(status.as_u16())) =>
        {
            // Permanently gone — count as reclaimed, clear the persisted id.
            true
        }
        Err(err) => {
            // Transient: hand off to the durable store so the retry survives even
            // if this inflight row is evicted/cleared before the next sweep (codex
            // P2 r10/r11/r13). The drain in the sweeper loop owns the retry.
            super::status_panel_orphan_store::enqueue(
                provider,
                token_hash,
                state.channel_id,
                panel_msg.get(),
            );
            tracing::debug!(
                "[placeholder_sweeper] orphan status-panel-v2 delete for {}/{} failed transiently \
                 — enqueued for durable retry: {err}",
                state.channel_id,
                panel_msg.get()
            );
            false
        }
    };
    // Converge the inflight row so the sweeper stops re-detecting this panel.
    if state.current_msg_id == 0 {
        // Panel-only abandoned row: it has no placeholder, so the placeholder
        // abandoned branch below skips it forever. Once the panel is handled
        // (deleted, or — on transient failure — enqueued to the durable store) the
        // row has nothing left, so evict it instead of only clearing the panel id
        // (codex P2 r23) — otherwise it lingers and keeps the channel busy.
        finalize_owner_dead_cleanup_if_same_turn(
            shared,
            provider,
            state,
            age_secs,
            sweep_started_before,
            false,
        )
        .await;
    } else if super::placeholder_cleanup::placeholder_sweep_leaves_row_unevicted(state)
        && let Some(panel_msg_id) = state.status_message_id
    {
        // Partial-response rows (real placeholder + streamed output) are owned by
        // the placeholder sweeper's deferred follow-up; do not evict them here.
        // Only clear our panel reference so we stop re-detecting it (codex P2 r12).
        // On a transient failure the durable store owns the retry, so this is safe.
        //
        // #3077: compare-and-clear under the inflight flock. The user_msg_id +
        // current_msg_id + msg-id guards reproduce the prior "same turn, same
        // panel" precondition atomically, so a newer turn that rebound the panel
        // between our snapshot load and this clear is never wiped.
        let _ = super::inflight::clear_status_panel_if_current(
            provider,
            state.channel_id,
            panel_msg_id,
            &super::inflight::StatusPanelClearGuard {
                require_user_msg_id: Some(state.user_msg_id),
                require_current_msg_id: Some(state.current_msg_id),
                ..Default::default()
            },
        );
    }
    if !committed {
        return;
    }
    report.reclaimed_panels += 1;
    tracing::warn!(
        "[sweeper SAFETY-NET] reclaimed orphan status-panel-v2 age={age_secs}s for {}/{} \
         (panel_msg {})",
        provider.as_str(),
        state.channel_id,
        panel_msg.get()
    );
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct SweepPassReport {
    pub scanned: usize,
    pub stalled: usize,
    pub abandoned: usize,
    /// #3003: orphaned status-panel-v2 messages reclaimed this pass.
    pub reclaimed_panels: usize,
}

fn should_log_sweep_report(report: SweepPassReport, sweeps_since_heartbeat: u64) -> bool {
    report.stalled > 0
        || report.abandoned > 0
        || report.reclaimed_panels > 0
        || sweeps_since_heartbeat >= SWEEP_HEARTBEAT_INTERVAL_SWEEPS
}

/// Spawn the long-lived background task that runs the stall sweeper at the
/// configured interval until the runtime exits. Should be called once per
/// provider during dcserver bootstrap.
pub(super) fn spawn_placeholder_sweeper(
    http: Arc<serenity::Http>,
    shared: Arc<SharedData>,
    provider: ProviderKind,
) {
    tokio::spawn(async move {
        let mut stalled_tracker = StalledEditTracker::default();
        let mut sweeps_since_heartbeat = 0u64;
        tokio::time::sleep(tokio::time::Duration::from_secs(INITIAL_DELAY_SECS)).await;
        loop {
            let report =
                run_placeholder_sweep_pass(&http, &shared, &provider, &mut stalled_tracker).await;
            // #3003: retry any durably-queued orphan status-panel deletes whose
            // inline reclaim failed transiently (and whose inflight row is gone, so
            // there is no per-turn handle left). Independent of inflight lifecycle.
            let drained = super::status_panel_orphan_store::drain(
                &http,
                &shared,
                &provider,
                &shared.token_hash,
            )
            .await;
            // #3296: reconcile durable aborted-anchor markers — retry the ✅ for
            // markers a terminal commit already covered, and apply the TTL'd
            // `⏳ → ⚠` fallback for anchors nothing ever covered (held while a
            // live inflight for the session may still cover them). The sweeper
            // owns this reclaim so an aborted anchor always converges (#3282).
            let drained_abort_markers =
                super::tui_direct_abort_marker::sweep_expired(&shared, &provider).await;
            // #4278 orphan-`⏳` sweep (mechanism: turn_view_reconciler::orphan_sweep).
            let swept_orphan_anchors =
                super::turn_view_reconciler::sweep_orphan_tui_anchor_reactions(&shared, &provider)
                    .await;
            // #3859: finalize placeholders stranded by a failure-path inflight
            // eviction (turn-task Drop / heartbeat-gap sweeper). Each durable
            // abandon-request is edited to its terminal "중단됨" card BY MESSAGE
            // ID — decoupled from the inflight lifecycle, so a re-adopt (new row
            // + new placeholder) never collides with it.
            let drained_abandon_requests =
                super::abandon_request_store::drain(&http, &shared, &provider, &shared.token_hash)
                    .await;
            sweeps_since_heartbeat = sweeps_since_heartbeat.saturating_add(1);
            if should_log_sweep_report(report, sweeps_since_heartbeat)
                || drained > 0
                || drained_abort_markers > 0
                || drained_abandon_requests > 0
                || swept_orphan_anchors > 0
            {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::info!(
                    "  [{ts}] 🧹 placeholder sweeper ({}): scanned={} stalled={} abandoned={} reclaimed_panels={} drained_orphans={} drained_abort_markers={} drained_abandon_requests={} swept_orphan_anchors={}",
                    provider.as_str(),
                    report.scanned,
                    report.stalled,
                    report.abandoned,
                    report.reclaimed_panels,
                    drained,
                    drained_abort_markers,
                    drained_abandon_requests,
                    swept_orphan_anchors
                );
                sweeps_since_heartbeat = 0;
            }
            tokio::time::sleep(tokio::time::Duration::from_secs(SWEEP_INTERVAL_SECS)).await;
        }
    });
}

#[cfg(test)]
mod probe_classification_tests {
    use super::is_permanent_message_gone_status;

    #[test]
    fn permanent_statuses_classified_as_gone() {
        assert!(is_permanent_message_gone_status(404));
        assert!(is_permanent_message_gone_status(403));
        assert!(is_permanent_message_gone_status(410));
    }

    #[test]
    fn transient_statuses_classified_as_retryable() {
        // 5xx server errors → retry next sweep
        assert!(!is_permanent_message_gone_status(500));
        assert!(!is_permanent_message_gone_status(502));
        assert!(!is_permanent_message_gone_status(503));
        assert!(!is_permanent_message_gone_status(504));
        // 429 rate limit → retry next sweep
        assert!(!is_permanent_message_gone_status(429));
        // 408 request timeout → retry next sweep
        assert!(!is_permanent_message_gone_status(408));
        // 401 unauthorized: transient credential rotation; do not evict
        assert!(!is_permanent_message_gone_status(401));
        // Hypothetical 2xx that somehow landed in the error path
        assert!(!is_permanent_message_gone_status(200));
        // Edge: 0 (no status code available)
        assert!(!is_permanent_message_gone_status(0));
    }
}

#[cfg(test)]
mod is_message_still_placeholder_tests {
    use super::is_message_still_placeholder;
    use crate::services::discord::formatting::PLACEHOLDER_PROBE_MARKER;

    #[test]
    fn spinner_prefixed_placeholder_is_placeholder() {
        assert!(is_message_still_placeholder("⠋ Processing..."));
        assert!(is_message_still_placeholder("⠹ ⚙ Bash: cargo build"));
        assert!(is_message_still_placeholder("⠧ mcp__memento__recall"));
        // All 10 braille spinner glyphs recognised.
        for ch in ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'] {
            let s = format!("{} working", ch);
            assert!(
                is_message_still_placeholder(&s),
                "frame {ch} not recognised"
            );
        }
    }

    #[test]
    fn handoff_card_headers_are_placeholder() {
        assert!(is_message_still_placeholder(&format!(
            "🔄 **응답 처리 중**\n{PLACEHOLDER_PROBE_MARKER}"
        )));
        assert!(is_message_still_placeholder(&format!(
            "🔄 **백그라운드 처리 중**\n{PLACEHOLDER_PROBE_MARKER}"
        )));
        assert!(is_message_still_placeholder(&format!(
            "⚠ **응답 정체**\n{PLACEHOLDER_PROBE_MARKER}"
        )));
        assert!(is_message_still_placeholder(&format!(
            "⚠ **백그라운드 정체**\n{PLACEHOLDER_PROBE_MARKER}"
        )));
        assert!(is_message_still_placeholder(&format!(
            "📬 **메시지 대기 중**\n{PLACEHOLDER_PROBE_MARKER}"
        )));
        assert!(is_message_still_placeholder(&format!(
            "⏱ **응답 타임아웃**\n{PLACEHOLDER_PROBE_MARKER}"
        )));
        assert!(is_message_still_placeholder(&format!(
            "❌ **응답 실패**: foo\n{PLACEHOLDER_PROBE_MARKER}"
        )));
        assert!(is_message_still_placeholder(&format!(
            "✅ **응답 완료**\n{PLACEHOLDER_PROBE_MARKER}"
        )));
        assert!(is_message_still_placeholder(
            "⚠ **응답 중단**\n> **도구**: Bash · **사유**: 응답 스트리밍 중\n> **시작**: <t:123:R>\n브릿지 또는 세션이 종료되었습니다."
        ));
    }

    #[test]
    fn exact_handoff_header_without_marker_is_not_placeholder() {
        assert!(!is_message_still_placeholder("✅ **응답 완료**"));
        assert!(!is_message_still_placeholder(
            "✅ **응답 완료**\n이 줄부터 실제 답변입니다."
        ));
        assert!(!is_message_still_placeholder("⚠ **응답 중단**"));
    }

    #[test]
    fn delivered_response_with_status_style_prefix_is_not_placeholder() {
        // Codex round 2 HIGH on PR #2417: assistant output legitimately
        // starts with status-style emoji + bold prose. These must be
        // protected from sweeper overwrite — they look like handoff
        // headers but are NOT the canonical Korean header strings.
        assert!(!is_message_still_placeholder("✅ **Done**"));
        assert!(!is_message_still_placeholder("✅ **Done**\n결과 요약..."));
        assert!(!is_message_still_placeholder(
            "⚠ **주의**: 이 부분 확인 필요"
        ));
        assert!(!is_message_still_placeholder("⚠ **Warning**"));
        assert!(!is_message_still_placeholder(
            "❌ **Error**: file not found"
        ));
        assert!(!is_message_still_placeholder("❌ **Build failed**"));
        assert!(!is_message_still_placeholder(
            "🔄 **Retry attempted**\nsecond paragraph"
        ));
        assert!(!is_message_still_placeholder("📬 **Inbox**: 3 unread"));
        assert!(!is_message_still_placeholder("⏱ **Elapsed**: 1.2s"));
        // English equivalents of the Korean headers must not match either.
        assert!(!is_message_still_placeholder("✅ **Response complete**"));
        assert!(!is_message_still_placeholder("⚠ **Response aborted**"));
        // Header text with extra trailing content beyond the `**` close on
        // the same line — e.g. a response title that uses the same Korean
        // bold + emoji pattern — must NOT match.
        assert!(!is_message_still_placeholder(
            "✅ **응답 완료** — 검토 결과 정상 동작"
        ));
        assert!(!is_message_still_placeholder("⚠ **응답 중단** 이거 농담"));
    }

    #[test]
    fn delivered_response_text_is_not_placeholder() {
        // Plain English prose
        assert!(!is_message_still_placeholder(
            "Sure — here is the answer you asked for."
        ));
        // Korean prose
        assert!(!is_message_still_placeholder(
            "네, 알려드리겠습니다. 첫 번째로 ..."
        ));
        // Code block
        assert!(!is_message_still_placeholder("```rust\nfn main() {}\n```"));
        // Markdown heading
        assert!(!is_message_still_placeholder(
            "## 결과\n\n분석 완료했습니다."
        ));
        // Leading bullet that happens to start with an emoji that is NOT
        // a placeholder header marker should not be classified as
        // placeholder.
        assert!(!is_message_still_placeholder("🟢 status: green"));
    }

    #[test]
    fn spinner_without_space_is_not_placeholder() {
        // Spinner char as part of regular content (no separating whitespace)
        // is not a placeholder.
        assert!(!is_message_still_placeholder("⠋text"));
    }

    #[test]
    fn empty_content_treated_as_placeholder() {
        // Empty message: nothing user-visible to protect.
        assert!(is_message_still_placeholder(""));
        assert!(is_message_still_placeholder("   "));
        assert!(is_message_still_placeholder("\n\n"));
    }

    #[test]
    fn leading_whitespace_does_not_mask_placeholder_shape() {
        assert!(is_message_still_placeholder("   ⠋ Processing..."));
        assert!(is_message_still_placeholder(&format!(
            "\n🔄 **응답 처리 중**\n{PLACEHOLDER_PROBE_MARKER}"
        )));
    }

    #[test]
    fn marker_is_authoritative_regardless_of_header_wording() {
        // #3031C: detection keys off the structured marker, not the card's
        // header/footer prose. A card with a non-Korean (or future-localized)
        // header is still recognised purely via the marker — proving the brittle
        // exact-string lockstep with formatting.rs is gone.
        assert!(is_message_still_placeholder(&format!(
            "🔄 **Processing response**\n{PLACEHOLDER_PROBE_MARKER}"
        )));
        assert!(is_message_still_placeholder(&format!(
            "🆕 **completely new header text**\n> **whatever**: x\n{PLACEHOLDER_PROBE_MARKER}"
        )));
        // The marker alone, with no recognizable header at all.
        assert!(is_message_still_placeholder(&format!(
            "arbitrary leading text\n{PLACEHOLDER_PROBE_MARKER}"
        )));
    }

    #[test]
    fn legacy_unmarked_card_matched_by_structural_scaffold_not_header_strings() {
        // #3031C: pre-marker legacy cards are recognised by the markdown
        // blockquote scaffold (started-at `<t:…>` line + a second field line),
        // independent of the exact (translatable) header/footer wording.
        assert!(is_message_still_placeholder(
            "ANY HEADER LINE\n> **사유**: 응답 스트리밍 중\n> **시작**: <t:123:R>\nfooter prose in any language"
        ));
        // A delivered answer that merely starts with a status-style header but
        // lacks the blockquote scaffold must NOT be treated as a placeholder.
        assert!(!is_message_still_placeholder(
            "✅ **응답 완료**\n실제 답변 본문입니다."
        ));
        // A single quoted timestamp without a second blockquote field is not a
        // card scaffold.
        assert!(!is_message_still_placeholder(
            "지난 알림 인용:\n> **시작**: <t:123:R>"
        ));
    }
}

#[cfg(test)]
mod safety_net_threshold_tests {
    //! #2438 (#2427 final): pin the safety-net threshold relationships
    //! so a future refactor cannot accidentally invert them. The
    //! placeholder sweeper is now the LAST cleanup layer — every
    //! explicit-signal wire (D / A / B / C) must have time to fire
    //! before the sweeper does anything destructive.
    use super::{
        ABANDON_THRESHOLD_SECS, INITIAL_DELAY_SECS, STALL_THRESHOLD_SECS, SWEEP_INTERVAL_SECS,
        panel_reclaim_target,
    };
    use crate::services::provider::ProviderKind;

    fn sweep_state_with_panel(status_message_id: Option<u64>) -> super::InflightTurnState {
        let mut state = super::InflightTurnState::new(
            ProviderKind::Claude,
            4242,
            None,
            7,
            9101,
            9102,
            "prompt".to_string(),
            Some("session".to_string()),
            Some("AgentDesk-claude-adk".to_string()),
            Some("/tmp/recovery.jsonl".to_string()),
            None,
            0,
        );
        state.status_message_id = status_message_id;
        state
    }

    #[test]
    fn stall_threshold_is_at_least_five_minutes() {
        // Five minutes (300s) matches `INFLIGHT_MAX_AGE_SECS` —
        // anything fresher than that the load path itself will not GC.
        assert!(STALL_THRESHOLD_SECS >= 300);
    }

    #[test]
    fn abandon_threshold_is_at_least_thirty_minutes() {
        // The abandon path is the last destructive action the sweeper
        // can take. Thirty minutes is the floor: long-running tools
        // (compilation, large refactors, deep ripgrep) routinely run
        // 10–20 minutes; we add another 10 minutes of safety margin
        // on top.
        assert!(ABANDON_THRESHOLD_SECS >= 1800);
    }

    #[test]
    fn abandon_strictly_greater_than_stall() {
        // The stall → abandoned ladder must remain monotonic.
        assert!(ABANDON_THRESHOLD_SECS > STALL_THRESHOLD_SECS);
    }

    #[test]
    fn initial_delay_lets_recovery_settle() {
        // Recovery retries (#2428 H5) burn up to ~120s on retry
        // backoff alone. Boot recovery needs at least that plus
        // headroom before the sweeper starts judging staleness.
        assert!(INITIAL_DELAY_SECS >= 180);
    }

    #[test]
    fn sweep_interval_is_within_a_minute() {
        // We don't want the safety-net log latency to drift higher
        // than one minute. 30s is the current cadence; pin the
        // upper bound.
        assert!(SWEEP_INTERVAL_SECS <= 60);
    }

    #[test]
    fn off_to_on_stale_panel_still_reaches_sweeper_reclaim_target() {
        let state = sweep_state_with_panel(Some(5001));

        let target = panel_reclaim_target(&state, ABANDON_THRESHOLD_SECS);

        assert_eq!(target.map(|id| id.get()), Some(5001));
    }

    #[test]
    fn footer_mode_none_panel_has_no_sweeper_reclaim_target() {
        let state = sweep_state_with_panel(None);

        let target = panel_reclaim_target(&state, ABANDON_THRESHOLD_SECS);

        assert_eq!(target, None);
    }
}
