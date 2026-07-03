//! #3886 — TimedOut-completion-gate status-panel reconcile.
//!
//! A warm hosted TUI turn (`AgentDesk-claude-dm-*`, `AgentDesk-claude-adk-cc`,
//! `AgentDesk-codex-adk-cdx`, …) can leave its Discord live status panel stuck
//! at `진행 중 / Active` forever: the `tui_completion_quiescence` gate returns
//! `TimedOut` (a skill turn keeps running past the 3s window for memento writes
//! etc.), so per #2161 the caller SUPPRESSES `StatusEvent::TurnCompleted`. The
//! gate doc names the placeholder sweeper / next-turn intake as the reconcile
//! that closes the lingering Active panel — but neither emitted a panel-finalize
//! event, and on the relay-dead frontier-0 path the watcher GateTimeout
//! finalizer (gated on `terminal_output_committed`) never fires either. The
//! live-events `DerivedStatus::Running` therefore never transitions to
//! `Completed`.
//!
//! This module adds the missing reconcile, driven from the placeholder sweeper:
//! for a still-tracked warm-TUI inflight whose live panel is still unfinished,
//! finalize the panel to `✅ 응답 완료` IFF the matched session's provider JSONL
//! DETERMINISTICALLY confirms the turn is terminal. The decision is on the turn
//! status (the SAME finalize-safe turn-END terminator the `TurnFinalizer` Done
//! decision uses — NOT the lenient idle-queue-drain readiness probe), NEVER on
//! timestamp age — so the panel resolves as soon as the turn is actually done,
//! not on a 600s backstop.
//!
//! Two cross-turn safety invariants keep this honest (codex review #3951):
//!   * DEFECT 1 — it uses [`jsonl_turn_end_terminator_idle`], the turn-END-only
//!     probe, so a NON-terminator Idle-class envelope (Codex
//!     `session_meta`/`thread.started`/`task_complete`/completed `agent_message`;
//!     Claude `system{init}`; a torn trailing fragment) can NEVER false-finalize
//!     a quiet STILL-RUNNING turn.
//!   * DEFECT 2 — it RE-READS the fresh on-disk inflight row and re-verifies the
//!     panel/turn identity right before the finalize edit; if the channel now
//!     hosts a different/newer turn (a NEXT-turn restart raced the stale sweep
//!     snapshot), it NO-OPs rather than completing the new turn's panel.
//!   * DEFECT 3 — on a committed finalize it registers the #3607 committed-
//!     terminal panel anchor, so the SAME-pass orphan-panel reclaim skips the
//!     panel it just finalized instead of deleting it.
//!   * DEFECT 1b (round-2) — the terminal verdict is OFFSET-SCOPED to the current
//!     turn: the unbounded `jsonl_turn_end_terminator_idle` reverse scan can walk
//!     PAST the current turn's non-terminator Idle-class envelopes and latch a
//!     PRIOR turn's terminator below `turn_start_offset`, false-finalizing a turn
//!     that is still running. The reconcile additionally requires the CURRENT
//!     turn's byte slice `[turn_start_offset, EOF)` to contain its OWN turn-end
//!     terminator, so a prior terminator can never confirm this turn's completion.
//!
//! Lives OUTSIDE the #3016 hot files (declared from the non-hot `tmux.rs`); it
//! never re-runs the gate and never touches relay/cleanup bookkeeping.

use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::Arc;

use poise::serenity_prelude as serenity;

use crate::services::discord::SharedData;
use crate::services::discord::inflight::{
    InflightTurnState, load_inflight_state, parse_started_at_unix,
};
use crate::services::discord::placeholder_cleanup::{
    PlaceholderCleanupOperation, PlaceholderCleanupOutcome, PlaceholderCleanupRecord,
    PlaceholderCleanupRegistry,
};
use crate::services::discord::turn_bridge::{
    complete_status_panel_v2_with_http, normalize_status_panel_message_id,
};
use crate::services::provider::ProviderKind;
use crate::services::tui_turn_state::jsonl_turn_end_terminator_idle;

/// Deterministic "this turn is terminal" probe used by the reconcile.
///
/// Mirrors the gate's `ConfirmedIdle` identity preconditions with PUBLIC inputs
/// only (provider JSONL turn-state + inflight offset fields), so it carries the
/// SAME honesty guarantees without reaching into the hot `tmux_watcher` module:
///   * `!rebind_origin` — operator-launched panes are never AgentDesk-gated.
///   * session-bound (a tmux session + a non-empty output JSONL path).
///   * the CURRENT turn advanced its own output past `turn_start_offset` — so a
///     stale PRIOR terminator envelope still in the shared session JSONL cannot
///     masquerade as this turn's completion.
///   * the provider JSONL holds the CURRENT turn's authoritative turn-END
///     terminator (Claude `result` / `system{turn_duration|stop_hook_summary}`;
///     Codex `turn.completed`) AND the runtime is back at Idle.
///
/// DEFECT 1 (codex #3951): the terminal signal is the finalize-safe
/// [`jsonl_turn_end_terminator_idle`] — the SAME turn-END-only probe the
/// `TurnFinalizer` Done decision uses — NOT the lenient
/// `observe_provider_jsonl_turn_state` idle-queue-drain readiness probe. The
/// lenient probe treats the whole "Idle-class" family as at-rest (Codex
/// `session_meta`/`thread.started`/`event_msg{task_complete}`/a *completed*
/// `agent_message`; Claude `system{init}`; and — via the walk-back fallback —
/// a malformed/torn tail that resolves to a PRIOR `result`). A finalize on any
/// of those over-finalizes a quiet STILL-RUNNING turn (the #2161 premature-
/// completion class). The turn-END-only probe walks PAST every non-terminator
/// Idle-class marker to the real per-turn terminator beneath, and reports Busy
/// (→ `false` here) for a torn/active/unparseable trailing line — so this never
/// finalizes a pane that could still be producing output. No lenient fallback.
///
/// DEFECT 1b (codex #3951 round-2): the `advanced` guard
/// (`last_offset > turn_start_offset`) proves the current turn wrote SOMETHING
/// but does NOT bind the (unbounded) `jsonl_turn_end_terminator_idle` scan to the
/// current turn. When the current turn has so far written ONLY non-terminator
/// Idle-class envelopes (Codex `item.completed`/`agent_message`; Claude
/// `system{init}`), the scan walks past them and latches a PRIOR turn's
/// terminator below `turn_start_offset` — false-finalizing a still-running turn.
/// The finalizer avoids the same false-bind by pinning the finalize identity at
/// the watcher call site (`pinned_finalize_user_msg_id`'s `< current_offset`
/// test); the reconcile has no such call site, so it OFFSET-SCOPES at the probe
/// level via [`current_turn_wrote_turn_end_terminator`] — additionally requiring
/// the CURRENT turn's byte slice `[turn_start_offset, EOF)` to contain its OWN
/// terminator.
///
/// Any other shape returns `false` (the row is left for the existing age-based
/// safety nets).
fn turn_jsonl_deterministically_terminal(
    provider: &ProviderKind,
    state: &InflightTurnState,
) -> bool {
    if state.rebind_origin {
        return false;
    }
    let Some(output_path) = state
        .output_path
        .as_deref()
        .map(str::trim)
        .filter(|path| !path.is_empty())
    else {
        return false;
    };
    if state
        .tmux_session_name
        .as_deref()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .is_none()
    {
        return false;
    }
    // The CURRENT turn must have advanced its own output past the turn-start
    // anchor; a missing anchor is treated as not-advanced (conservative). The
    // anchor is then reused below to offset-scope the terminator search (1b).
    let Some(turn_start_offset) = state.turn_start_offset else {
        return false;
    };
    if state.last_offset <= turn_start_offset {
        return false;
    }
    // Mirror the gate's `matched_session_completion_signal` (GateQuiescence mode)
    // file guard: a missing or empty JSONL cannot confirm completion.
    // `jsonl_turn_end_terminator_idle` already reports Busy on a read error, but
    // we keep the explicit metadata guard so an absent / zero-byte transcript is
    // rejected before any scan.
    let path = Path::new(output_path);
    match std::fs::metadata(path) {
        Ok(metadata) if metadata.is_file() && metadata.len() > 0 => {}
        _ => return false,
    }
    // The shared probe owns the Idle-semantics conservatism (no streaming after
    // the terminator, torn-write skip, housekeeping walk-back); the offset-scoped
    // probe binds that verdict to the CURRENT turn so a PRIOR turn's terminator
    // below `turn_start_offset` cannot confirm completion (DEFECT 1b).
    jsonl_turn_end_terminator_idle(provider, path)
        && current_turn_wrote_turn_end_terminator(provider, path, turn_start_offset)
}

/// DEFECT 1b (codex #3951 round-2): does the CURRENT turn's byte slice
/// `[turn_start_offset, EOF)` contain its OWN authoritative turn-END terminator?
///
/// This binds the unbounded [`jsonl_turn_end_terminator_idle`] verdict to the
/// current turn: only a terminator the current turn actually wrote (at or after
/// its start anchor) can confirm completion, so a PRIOR turn's terminator still
/// present below `turn_start_offset` can never finalize a turn that has only
/// written non-terminator Idle-class envelopes so far.
///
/// It scans ONLY `[turn_start_offset, EOF)`. A leading partial line (when the
/// anchor lands mid-envelope) and a torn trailing line both fail JSON parsing
/// and are simply not counted — never a false terminator. The shared probe still
/// owns "is the turn at rest?"; this answers only "did THIS turn write a
/// terminator?", and the two together are the offset-bound finalize signal.
fn current_turn_wrote_turn_end_terminator(
    provider: &ProviderKind,
    path: &Path,
    turn_start_offset: u64,
) -> bool {
    let Ok(mut file) = std::fs::File::open(path) else {
        return false;
    };
    let Ok(metadata) = file.metadata() else {
        return false;
    };
    if turn_start_offset >= metadata.len() {
        // The current turn has written nothing past its anchor → no own terminator.
        return false;
    }
    if file.seek(SeekFrom::Start(turn_start_offset)).is_err() {
        return false;
    }
    let mut slice = String::new();
    if file.read_to_string(&mut slice).is_err() {
        // Non-UTF-8 / read error cannot prove the current turn ended → conservative.
        return false;
    }
    slice
        .lines()
        .any(|line| line_is_turn_end_terminator(provider, line))
}

/// Positive structural match for the authoritative per-provider TURN-END
/// terminator envelope, mirroring
/// `tui_turn_state::envelope_is_turn_end_terminator` for the STRUCTURAL
/// terminators:
///   * Codex: `type == "turn.completed"`.
///   * Claude: `type == "result"` or `system{turn_duration | stop_hook_summary}`.
///
/// The niche `[Request interrupted by user]` marker is intentionally NOT matched
/// here — an interrupt-only current turn is left to the interrupt/stop finalizer.
/// Fail-safe: an unrecognized terminator only makes the reconcile WAIT (the panel
/// is handled by another safety net), never false-finalize a running turn. A
/// malformed / partial line fails to parse and is treated as "not a terminator".
fn line_is_turn_end_terminator(provider: &ProviderKind, line: &str) -> bool {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return false;
    }
    let Ok(json) = serde_json::from_str::<serde_json::Value>(trimmed) else {
        return false;
    };
    let Some(type_str) = json.get("type").and_then(serde_json::Value::as_str) else {
        return false;
    };
    match provider {
        ProviderKind::Codex => type_str == "turn.completed",
        ProviderKind::Claude => match type_str {
            "result" => true,
            "system" => matches!(
                json.get("subtype").and_then(serde_json::Value::as_str),
                Some("turn_duration" | "stop_hook_summary")
            ),
            _ => false,
        },
        _ => false,
    }
}

/// DEFECT 2 (codex #3951): same-turn identity re-verification.
///
/// The placeholder sweeper snapshots an inflight row, then awaits Discord IO
/// before the reconcile runs. In that gap the OLD turn can complete and a NEXT
/// turn can clear/restart the channel panel onto a brand-new row. The channel-
/// level "panel unfinished" state then reflects the NEW turn's `진행 중` panel,
/// while the stale snapshot's JSONL still reads terminal — so finalizing on the
/// snapshot would wrongly complete the NEW turn's panel.
///
/// Before the finalize edit the reconcile RE-READS the fresh on-disk inflight
/// row and compares the turn/panel identity against the snapshot. The turn is
/// the SAME one IFF every identity field matches: the Discord message ids
/// (`user_msg_id`, `current_msg_id`, `status_message_id`) AND the session
/// binding (`tmux_session_name`, `output_path`). Any mismatch means the channel
/// now hosts a different/newer turn → the caller NO-OPs.
fn reconcile_same_turn_identity(snapshot: &InflightTurnState, fresh: &InflightTurnState) -> bool {
    snapshot.user_msg_id == fresh.user_msg_id
        && snapshot.current_msg_id == fresh.current_msg_id
        && snapshot.status_message_id == fresh.status_message_id
        && snapshot.tmux_session_name == fresh.tmux_session_name
        && snapshot.output_path == fresh.output_path
}

/// Pure reconcile decision: a status panel left unfinished by a suppressed
/// completion (`TuiCompletionGateOutcome::TimedOut.should_emit_completion() ==
/// false`) is finalized to `✅ 응답 완료` IFF — AND ONLY IFF — the turn is now
/// `deterministic_terminal`. `panel_unfinished` keeps it idempotent (an already-
/// `Completed` panel is never re-finalized → no needless re-edit → heartbeat
/// byte-stability preserved); `deterministic_terminal` keeps it honest (a pane
/// still streaming is never marked done).
fn timed_out_panel_should_reconcile_to_done(
    panel_unfinished: bool,
    deterministic_terminal: bool,
) -> bool {
    panel_unfinished && deterministic_terminal
}

/// DEFECT 3 (codex #3951): register the #3607 committed-terminal panel anchor
/// for a panel this reconcile just finalized.
///
/// The reconcile runs in the SAME sweep pass as `sweep_orphan_status_panel`,
/// which would otherwise DELETE this aged panel right after we finalized it to
/// `응답 완료` (the reconcile mutates no inflight row, so the orphan reclaim
/// still sees an abandoned panel-bearing row). Recording an `EditTerminal /
/// Succeeded` tombstone for the finalized panel message makes
/// [`committed_terminal_panel_anchor_skip`] return `true` for it, so the
/// same-pass orphan reclaim SKIPS the delete. The tombstone is idempotent and
/// TTL-bounded (1h), so a repeated registration is harmless.
///
/// [`committed_terminal_panel_anchor_skip`]:
///     crate::services::discord::placeholder_cleanup::committed_terminal_panel_anchor_skip
fn register_finalized_panel_terminal_anchor(
    registry: &PlaceholderCleanupRegistry,
    provider: &ProviderKind,
    channel_id: serenity::ChannelId,
    panel_msg: serenity::MessageId,
    tmux_session_name: Option<&str>,
) {
    registry.record(PlaceholderCleanupRecord {
        provider: provider.clone(),
        channel_id,
        message_id: panel_msg,
        tmux_session_name: tmux_session_name.map(str::to_string),
        operation: PlaceholderCleanupOperation::EditTerminal,
        outcome: PlaceholderCleanupOutcome::Succeeded,
        source: "placeholder_sweeper_timedout_reconcile",
    });
}

/// Reconcile a status panel stuck at `진행 중` after a `TimedOut` completion
/// gate. Returns `true` only when a panel-finalize edit/send actually committed.
///
/// Gating on `status_panel_is_unfinished` means this fires AT MOST ONCE per turn
/// (the push of `StatusEvent::TurnCompleted` flips the live state to `Completed`,
/// so the next sweep skips it) — preserving the heartbeat byte-stability
/// invariant. It only ever ADDS the terminal event the suppressed gate withheld;
/// it does not touch the #3812 confidence line or #3920 subagent surfacing.
///
/// Honesty guards (codex #3951): the terminal probe is finalize-safe (DEFECT 1),
/// the turn/panel identity is re-verified against a FRESH on-disk read right
/// before the edit (DEFECT 2), and a committed finalize registers the #3607
/// terminal anchor so the same-pass orphan reclaim cannot delete it (DEFECT 3).
pub(in crate::services::discord) async fn reconcile_timed_out_tui_status_panel(
    http: &Arc<serenity::Http>,
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    state: &InflightTurnState,
) -> bool {
    if !shared.ui.status_panel_v2_enabled || state.channel_id == 0 {
        return false;
    }
    let channel_id = serenity::ChannelId::new(state.channel_id);
    let panel_unfinished = shared
        .ui
        .placeholder_live_events
        .status_panel_is_unfinished(channel_id);
    if !panel_unfinished {
        return false;
    }
    // DEFECT 2: re-read the FRESH on-disk inflight row and re-verify the turn/
    // panel identity before finalizing. The sweep snapshot may be stale — the
    // channel could now host a NEXT turn whose freshly-restarted panel must NOT
    // be finalized off the old turn's terminal JSONL. If the row is gone (turn
    // fully completed) or now belongs to a different/newer turn, NO-OP.
    let Some(fresh) = load_inflight_state(provider, state.channel_id)
        .filter(|fresh| reconcile_same_turn_identity(state, fresh))
    else {
        return false;
    };
    // The deterministic terminal probe runs on the FRESH row so the JSONL read
    // reflects the on-disk turn we just re-verified, not the stale snapshot.
    let deterministic_terminal = turn_jsonl_deterministically_terminal(provider, &fresh);
    if !timed_out_panel_should_reconcile_to_done(panel_unfinished, deterministic_terminal) {
        return false;
    }
    let started_at_unix =
        parse_started_at_unix(&fresh.started_at).unwrap_or_else(|| chrono::Utc::now().timestamp());
    let status_panel_msg_id =
        normalize_status_panel_message_id(fresh.status_message_id.map(serenity::MessageId::new));
    // The reconcile owns no prior render text; `complete_status_panel_v2_with_http`
    // edits the persisted panel id (or sends a fallback when none) to the freshly
    // rendered `응답 완료` text and pushes the withheld `TurnCompleted` event.
    let mut last_status_panel_text = String::new();
    let committed = complete_status_panel_v2_with_http(
        shared,
        http,
        channel_id,
        status_panel_msg_id,
        provider,
        started_at_unix,
        &mut last_status_panel_text,
        false,
        "placeholder_sweeper_timedout_reconcile",
        (Some(fresh.user_msg_id), Some(&fresh)),
    )
    .await;
    if committed {
        // DEFECT 3: register the #3607 committed-terminal anchor for the panel we
        // just finalized so `sweep_orphan_status_panel` — which runs immediately
        // after this in the same pass — skips the delete instead of reclaiming
        // the `응답 완료` panel. Only meaningful when a real panel message id is
        // owned (the orphan reclaim targets exactly this normalized id).
        if let Some(panel_msg) = status_panel_msg_id {
            register_finalized_panel_terminal_anchor(
                &shared.ui.placeholder_cleanup,
                provider,
                channel_id,
                panel_msg,
                fresh.tmux_session_name.as_deref(),
            );
        }
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::warn!(
            provider = %provider.as_str(),
            channel = channel_id.get(),
            tmux_session = fresh.tmux_session_name.as_deref().unwrap_or(""),
            "[{ts}] \u{2705} #3886 reconciled status panel stuck at '진행 중' after TUI completion-gate TimedOut — provider JSONL deterministically confirms the turn is terminal; finalized panel to 응답 완료"
        );
    }
    committed
}

#[cfg(test)]
#[path = "status_panel_timedout_reconcile_tests.rs"]
mod status_panel_timedout_reconcile_tests;
