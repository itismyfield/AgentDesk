//! Restart-path helpers (issue #1074 landing zone; first occupant: #3293).
//!
//! Hosts the side-effecting epilogue for the boot-time recovery branches in
//! `recovery_engine::restore_inflight_turns` whose terminal relay to Discord
//! did NOT deliver. The pure decision matrix lives in [`super::shared`]; this
//! module executes the chosen [`RowDisposition`] with the no-silent-delete
//! guarantees from the #3293 design: every force-clear writes an on-disk
//! force-clear report (full response text + row metadata, see
//! [`persist_force_clear_report`]) + a termination audit + a structured WARN,
//! and the preserve path persists the attempt counter through a
//! restart-marker-preserving identity-guarded bump.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use poise::serenity_prelude as serenity;
use poise::serenity_prelude::ChannelId;
use serde::{Deserialize, Serialize};

use super::super::inflight::opt_channel_id;
use super::super::recovery_engine::{finish_recovered_turn_mailbox, save_missing_session_handoff};
use super::super::runtime_store::{atomic_write, discord_recovery_force_clear_root};
use super::super::{SharedData, inflight};
use super::shared::{
    ChannelProbeVerdict, RecoveryRelayOutcome, RowDisposition, classify_channel_probe_status,
    disposition_reason_code, unrecoverable_relay_disposition,
};
use crate::services::provider::ProviderKind;
use crate::services::turn_orchestrator::ChannelMailboxRegistry;

/// #3297 finding 2: active channel-liveness probe used as the second opinion
/// after a transient-looking relay failure. A direct `get_channel` bypasses
/// the String-flattened formatting error chain entirely: 404/403/410 on the
/// CHANNEL itself is an authoritative "gone" verdict; everything else
/// (success, 5xx, 429, transport) stays inconclusive so the conservative
/// transient classification survives.
pub(in crate::services::discord) async fn probe_channel_liveness(
    http: &serenity::Http,
    channel_id: ChannelId,
) -> ChannelProbeVerdict {
    match http.get_channel(channel_id).await {
        Ok(_) => ChannelProbeVerdict::Inconclusive,
        Err(serenity::Error::Http(http_err)) => {
            classify_channel_probe_status(http_err.status_code().map(|status| status.as_u16()))
        }
        Err(_) => ChannelProbeVerdict::Inconclusive,
    }
}

/// #3293 (c): finish the recovered turn's mailbox ONLY when a registry entry
/// already exists. `finish_recovered_turn_mailbox` routes through the turn
/// finalizer, whose channel-scoped resolution mints a mailbox actor on first
/// touch — on a force-clear of a row for a non-existent (bogus) channel that
/// would re-pollute the registry with a permanent entry. Peek, never create.
pub(in crate::services::discord) async fn finish_recovered_turn_mailbox_if_registered(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: ChannelId,
    stop_source: &'static str,
) {
    let registered = shared.mailbox_peek(channel_id).is_some()
        || ChannelMailboxRegistry::global_handle(channel_id).is_some();
    if !registered {
        tracing::debug!(
            provider = %provider.as_str(),
            channel_id = channel_id.get(),
            stop_source,
            "recovery force-clear: no mailbox registry entry — skipping finish to avoid creating one"
        );
        return;
    }
    finish_recovered_turn_mailbox(shared, provider, channel_id, stop_source).await;
}

/// #3293: shared epilogue for the five recovery notice branches. Computes the
/// [`RowDisposition`] from the relay `outcome` and executes it:
///
/// * `FinishAndClear` (delivered) — the branch's historical epilogue:
///   `finish_recovered_turn_mailbox(finish_stop_source)` + clear.
/// * everything else — [`apply_undeliverable_relay_disposition`].
#[allow(clippy::too_many_arguments)]
pub(in crate::services::discord) async fn dispose_recovery_relay_outcome(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    state: &inflight::InflightTurnState,
    outcome: RecoveryRelayOutcome,
    tmux_alive: bool,
    finish_stop_source: &'static str,
    branch: &'static str,
    best_response: &str,
    handoff_already_saved: bool,
) {
    match unrecoverable_relay_disposition(
        outcome,
        state.recovery_relay_attempts,
        inflight::RECOVERY_RELAY_RESTART_ATTEMPT_BUDGET,
        tmux_alive,
    ) {
        RowDisposition::FinishAndClear => {
            let Some(channel_id) = opt_channel_id(state.channel_id) else {
                tracing::warn!(
                    provider = %provider.as_str(),
                    "recovery relay disposition skipped mailbox cleanup because persisted channel id is zero"
                );
                return;
            };
            finish_recovered_turn_mailbox(shared, provider, channel_id, finish_stop_source).await;
            // #3918: do NOT silently ignore the clear result. A `false` here
            // means the row is still on disk, so the next boot re-enters this
            // branch — for the anchor-repost path that would re-probe the gone
            // anchor and, absent a durable marker, re-post. Correctness no
            // longer depends on this call succeeding (the `anchor_reposted`
            // marker set BEFORE this dispose blocks a duplicate send-new), but a
            // persistent clear failure is an operational signal worth surfacing.
            if !inflight::clear_inflight_state(provider, state.channel_id) {
                tracing::warn!(
                    provider = %provider.as_str(),
                    channel_id = state.channel_id,
                    branch,
                    "recovery: clear_inflight_state returned false (row still on disk) after a \
                     delivered relay — next boot may re-enter this branch (an anchor-repost \
                     re-run is blocked by the durable 'anchor_reposted' marker)"
                );
            }
        }
        disposition => {
            apply_undeliverable_relay_disposition(
                shared,
                provider,
                state,
                disposition,
                branch,
                tmux_alive,
                best_response,
                handoff_already_saved,
            )
            .await;
        }
    }
}

/// #3610 PR-2 (stale-anchor range guard, pure/testable): does the durable anchor's
/// recorded JSONL `range` belong to THIS recovered turn?
///
/// The anchor reader
/// ([`delivery_frontier_probe::current_generation_delivered_anchor`])
/// already enforces the #1270 GENERATION gate — a stale prior-generation frontier
/// (e.g. a same-named tmux respawn) is rejected before we get here. This adds the
/// per-turn TEMPORAL check on top: the frontier is channel-scoped and holds the
/// LATEST commit, so for it to be THIS turn's anchor its recorded `range` must be
/// (a) a real non-empty slice (`end > start`), (b) contain the turn's persisted
/// `last_offset` — the committed terminal answer covered the output the row last
/// saw but is not a future slice from another delivery channel — and (c)
/// `turn_start_offset` (when present) must not sit ABOVE the anchor's end, which
/// would mean the anchor predates this turn's slice.
///
/// Conservative on missing data: an absent `last_offset` signal cannot happen
/// (it is a plain `u64`), but a `range_end == 0` / empty range fails (a), so a
/// blank/sentinel frontier never reposts.
fn anchor_range_matches_turn(
    anchor_range: (u64, u64),
    turn_start_offset: Option<u64>,
    last_offset: u64,
) -> bool {
    let (start, end) = anchor_range;
    // (a) real non-empty slice.
    if end <= start {
        return false;
    }
    // (b) the commit reached the output the row last observed for this turn.
    if end < last_offset {
        return false;
    }
    // Also reject a later owner-frontier slice. Owner records can be shared by
    // multiple delivery channels, so a stale row whose last_offset is before the
    // anchor's start must not borrow a later channel's terminal anchor.
    if last_offset <= start {
        return false;
    }
    // (c) the turn's own start must not be beyond the anchor's end (a later turn).
    if let Some(turn_start) = turn_start_offset {
        if turn_start > end {
            return false;
        }
    }
    true
}

fn anchor_panel_channel_matches_turn(
    anchor_panel_channel_id: u64,
    delivery_channel_id: u64,
) -> bool {
    anchor_panel_channel_id != 0 && anchor_panel_channel_id == delivery_channel_id
}

/// Durable delivery records are keyed by the tmux watcher's offset-authority
/// channel. The sidecar owner is the preferred lookup key, but it is deliberately
/// not the only key: rollback or mixed-binary windows can leave a current
/// same-channel delivery record behind while an older owner-context sidecar still
/// points elsewhere. Try the legacy row key after the sidecar-selected key misses
/// or fails the structural stale-anchor guards.
fn anchor_record_lookup_channel_ids(
    provider: &ProviderKind,
    state: &inflight::InflightTurnState,
) -> Vec<u64> {
    let mut channel_ids = Vec::with_capacity(2);
    if let Some(tmux_session_name) = state.tmux_session_name.as_deref()
        && let Some(delivery_channel_id) = opt_channel_id(state.channel_id)
        && let Some(owner) =
            super::super::outbound::delivery_record::watcher_owner_channel_for_delivery_channel(
                provider,
                delivery_channel_id,
                tmux_session_name,
            )
    {
        channel_ids.push(owner.get());
    }
    if let Some(fallback_channel_id) =
        opt_channel_id(state.delivery_record_owner_channel_id()).map(ChannelId::get)
        && !channel_ids.contains(&fallback_channel_id)
    {
        channel_ids.push(fallback_channel_id);
    }
    channel_ids
}

fn matching_recovery_anchors(
    provider: &ProviderKind,
    state: &inflight::InflightTurnState,
    tmux_session_name: &str,
) -> Vec<(
    u64,
    super::super::outbound::delivery_frontier_probe::CurrentGenerationAnchor,
)> {
    use super::super::outbound::delivery_frontier_probe;

    let mut anchors = Vec::with_capacity(2);
    let output_eof = state
        .output_path
        .as_deref()
        .and_then(|path| std::fs::metadata(path).ok().map(|meta| meta.len()));
    for record_channel_id in anchor_record_lookup_channel_ids(provider, state) {
        let Some(record_channel_id) = opt_channel_id(record_channel_id) else {
            continue;
        };
        let Some(anchor) = delivery_frontier_probe::current_generation_delivered_anchor(
            provider,
            record_channel_id,
            tmux_session_name,
            output_eof,
        ) else {
            tracing::debug!(
                provider = %provider.as_str(),
                channel_id = state.channel_id,
                record_channel = record_channel_id.get(),
                "  · recovery anchor-repost: no current anchor at candidate record channel"
            );
            continue;
        };

        // When an owner record is shared by multiple delivery channels, the
        // terminal anchor must still live in THIS row's delivery channel.
        if !anchor_panel_channel_matches_turn(anchor.panel_channel_id, state.channel_id) {
            tracing::warn!(
                provider = %provider.as_str(),
                channel_id = state.channel_id,
                record_channel = record_channel_id.get(),
                anchor_channel_id = anchor.panel_channel_id,
                "  ✗ recovery anchor-repost: anchor channel does not match this turn — trying next candidate (stale-owner guard)"
            );
            continue;
        }

        // The anchor's recorded range must belong to THIS recovered turn.
        if !anchor_range_matches_turn(anchor.range, state.turn_start_offset, state.last_offset) {
            tracing::warn!(
                provider = %provider.as_str(),
                channel_id = state.channel_id,
                record_channel = record_channel_id.get(),
                anchor_range = ?anchor.range,
                turn_start_offset = ?state.turn_start_offset,
                last_offset = state.last_offset,
                "  ✗ recovery anchor-repost: anchor range does not match this turn — trying next candidate (stale-anchor guard)"
            );
            continue;
        }

        anchors.push((record_channel_id.get(), anchor));
    }
    anchors
}

fn first_repost_anchor_if_all_probed_candidates_gone(
    probed_anchors: &[(
        u64,
        super::super::outbound::delivery_frontier_probe::CurrentGenerationAnchor,
        super::super::placeholder_sweeper::PlaceholderProbe,
    )],
) -> Option<(
    u64,
    super::super::outbound::delivery_frontier_probe::CurrentGenerationAnchor,
)> {
    let mut first_gone = None;
    for (record_channel_id, anchor, probe) in probed_anchors {
        if !anchor_probe_should_repost(*probe) {
            return None;
        }
        first_gone.get_or_insert((*record_channel_id, *anchor));
    }
    first_gone
}

/// #3610 PR-2 (pure/testable): given a placeholder probe of the durable anchor,
/// should the recovery fallback REPOST the terminal text? ONLY when the anchored
/// message is permanently GONE (404/403/410). Every other verdict is a no-op:
/// the message is still there (`StillPlaceholder` / `AlreadyDelivered` — reposting
/// would DUPLICATE a live message) or the probe was inconclusive (`ProbeFailed` —
/// a transient GET error must never be read as "gone").
fn anchor_probe_should_repost(probe: super::super::placeholder_sweeper::PlaceholderProbe) -> bool {
    matches!(
        probe,
        super::super::placeholder_sweeper::PlaceholderProbe::MessageGone
    )
}

/// #3918 (codex round-3): the outcome of [`try_recover_anchor_repost`], richer
/// than the previous `Option<RecoveryRelayOutcome>` so the caller can tell apart
/// THREE distinct on-disk-row dispositions. The old `None` conflated "no repost
/// needed — run the committed-delivery clear" with "refused the send — PRESERVE
/// the row", and the caller's `None` arm clears the row UNCONDITIONALLY. That
/// clear (a) dropped a committed answer whose pre-send bump hit a transient
/// `IoError` (the deferral the bump-gate intends is lost) and (b) could DELETE a
/// row now owned by a NEWER turn on an `IdentityMismatch`. This enum makes the
/// three contracts explicit so the caller never clears a row it must preserve.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::services::discord) enum AnchorRepostOutcome {
    /// No send-new was needed/possible by the existing guards (flag OFF, blank
    /// body, already-reposted / pre-send budget exhausted, anchor still present,
    /// or no usable record). The committed answer is delivered or we are giving
    /// up → the caller runs its legacy committed-delivery finish + clear.
    NotReposted,
    /// A send-new was attempted → the caller disposes this relay outcome via
    /// [`dispose_recovery_relay_outcome`].
    Relayed(RecoveryRelayOutcome),
    /// The pre-send `anchor_repost_attempts` bump did NOT durably persist
    /// (`IoError` / `Missing` / `IdentityMismatch`), so the send was REFUSED (the
    /// at-most-`budget` bound cannot be guaranteed without a durable attempt
    /// record). The on-disk row was left UNTOUCHED — the caller MUST preserve it
    /// (do NOT clear): a transient `IoError` is re-posted by a later boot whose
    /// bump succeeds, and a `Missing` / stranger (`IdentityMismatch`, a newer
    /// turn now owns the row) row must never be cleared by this path.
    RefusedPreserveRow,
}

/// Pure mapping of the pre-send `anchor_repost_attempts` bump outcome to the
/// send decision: ONLY a durably-persisted (`Saved`) attempt may proceed to the
/// send; every failure refuses the send AND preserves the on-disk row. Returns
/// `None` to mean "proceed to send-new". Extracted so the bump-gate safety
/// matrix is unit-testable without the async send path.
fn anchor_repost_pre_send_refusal(
    bump_outcome: inflight::GuardedSaveOutcome,
) -> Option<AnchorRepostOutcome> {
    match bump_outcome {
        // Durable attempt record → the at-most-`budget` bound holds → send.
        inflight::GuardedSaveOutcome::Saved => None,
        // No durable attempt record → refuse and preserve the row (see the
        // `RefusedPreserveRow` doc for the per-variant rationale).
        inflight::GuardedSaveOutcome::IoError
        | inflight::GuardedSaveOutcome::Missing
        | inflight::GuardedSaveOutcome::IdentityMismatch => {
            Some(AnchorRepostOutcome::RefusedPreserveRow)
        }
    }
}

/// #3610 PR-2: anchor-based recovery repost fallback (the #3607 "committed, then
/// the message disappeared" backstop). Flag-gated DARK (default OFF) — when OFF
/// this returns [`AnchorRepostOutcome::NotReposted`] BEFORE any record read /
/// probe / relay, so the recovery loop is a byte-for-byte no-op.
///
/// Called ONLY from the committed branch of `restore_inflight_turns`
/// (`recovery_terminal_delivery_already_committed(&state)` true) — the anchor is
/// recorded ONLY on a committed delivery (PR-1~1d's `is_delivered` gate), so a
/// committed row's current-generation anchor is THIS turn's, never a stale one.
///
/// SIX guards, each a distinct duplicate-repost defense:
/// * **G1** — flag OFF → `NotReposted` (outermost; dark-deploy no-op).
/// * **G2** — no trustworthy anchor → `NotReposted`. The reader enforces the #1270
///   generation gate AND a populated non-zero `(panel_msg_id, panel_channel_id)`;
///   we additionally reject an EMPTY `terminal_text` (no blank repost) and a
///   `range` that does not match this turn ([`anchor_range_matches_turn`]).
/// * **G2c (#3918 idempotency)** — the send-new is not a transaction with the
///   row retirement, so a crash / silently-failing `clear_inflight_state` after
///   Discord accepts the message would re-enter this branch on the next boot.
///   Refuse when the durable `anchor_reposted` marker is already set (this turn
///   was reposted — never duplicate) OR the pre-send `anchor_repost_attempts`
///   budget is exhausted (hard-bounds the residual crash window). Pure decision:
///   [`super::shared::anchor_repost_send_new_permitted`].
/// * **G3** — probe every structurally matching anchor candidate: repost ONLY
///   when ALL candidates are `MessageGone` (404/403/410). A live message
///   (`StillPlaceholder` / `AlreadyDelivered`) or a transient `ProbeFailed` on
///   ANY candidate → `NotReposted` (never duplicate a live or unverified message).
/// * **G4** — relay as a NEW message: the anchor is gone so it cannot be edited;
///   we pass `placeholder = None`, which routes through `send_long_message_raw`
///   (NOT an edit). Returns `Relayed(outcome)` for the caller to `dispose_*`; a
///   pre-send bump that does not durably persist returns `RefusedPreserveRow`
///   (the send is refused and the row preserved for a later boot).
/// * **G5 (passive)** — after a `Delivered` repost the caller's `dispose_*` clears
///   the row; the watcher cannot re-relay this range because it sits within the
///   committed floor (`committed_floor_for_resend_dedup` ≥ this generation's
///   `delivered_frontier.end`, and `range_already_committed` suppresses it). We
///   read the anchor only — no offset is written — so the dedup math is unchanged.
#[allow(clippy::too_many_arguments)]
pub(in crate::services::discord) async fn try_recover_anchor_repost(
    http: &Arc<serenity::Http>,
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    state: &inflight::InflightTurnState,
    terminal_text: &str,
) -> AnchorRepostOutcome {
    use super::super::placeholder_sweeper;

    // G1: flag OFF → no-op (outermost guard; dark deploy is byte-identical).
    if !super::shared::recovery_anchor_repost_enabled() {
        return AnchorRepostOutcome::NotReposted;
    }

    // G2a: never repost a blank body.
    if terminal_text.trim().is_empty() {
        return AnchorRepostOutcome::NotReposted;
    }

    // #3918 G2c (idempotency): the send-new below is NOT a transaction with the
    // row retirement — Discord can accept the new message and the process then
    // crash (or `clear_inflight_state` can silently fail) before the row is
    // cleared, re-entering this branch on the next boot. Refuse to fire when the
    // durable `anchor_reposted` marker is already set (this turn was reposted —
    // never duplicate) OR the pre-send attempt budget is exhausted (hard-bounds
    // the residual crash window so duplication is never unbounded). Both inputs
    // are persisted ON this row; see `shared::anchor_repost_send_new_permitted`.
    if !super::shared::anchor_repost_send_new_permitted(
        state.anchor_reposted,
        state.anchor_repost_attempts,
        inflight::RECOVERY_RELAY_RESTART_ATTEMPT_BUDGET,
    ) {
        tracing::info!(
            provider = %provider.as_str(),
            channel_id = state.channel_id,
            anchor_reposted = state.anchor_reposted,
            anchor_repost_attempts = state.anchor_repost_attempts,
            budget = inflight::RECOVERY_RELAY_RESTART_ATTEMPT_BUDGET,
            "  · recovery anchor-repost: already reposted or pre-send budget exhausted — no-op (idempotent)"
        );
        return AnchorRepostOutcome::NotReposted;
    }

    // G2b: resolve the current-generation, fully-populated anchor (the reader is
    // the stale-anchor structural guard: generation-gated + non-zero pair). The
    // delivery record is normally keyed by `watcher_owner_channel_id`, but stale
    // owner-context sidecars must not mask a valid legacy same-channel record left
    // by an older binary. Never use `logical_channel_id` here: it is a
    // thread-parent mapping, not the offset-authority key.
    let tmux_session_name = state.tmux_session_name.as_deref().unwrap_or("");
    let candidates = matching_recovery_anchors(provider, state, tmux_session_name);
    if candidates.is_empty() {
        return AnchorRepostOutcome::NotReposted;
    }
    let mut probed_anchors = Vec::with_capacity(candidates.len());
    for (record_channel_id, anchor) in candidates {
        // G3: probe the anchored message; repost ONLY when every structurally
        // matching candidate is permanently gone.
        let probe = placeholder_sweeper::probe_placeholder_state(
            http,
            anchor.panel_channel_id,
            anchor.panel_msg_id,
        )
        .await;
        probed_anchors.push((record_channel_id, anchor, probe));
        if !anchor_probe_should_repost(probe) {
            tracing::info!(
                provider = %provider.as_str(),
                channel_id = state.channel_id,
                record_channel = record_channel_id,
                anchor_msg_id = anchor.panel_msg_id,
                anchor_channel_id = anchor.panel_channel_id,
                ?probe,
                "  · recovery anchor-repost: a candidate anchor probe is not MessageGone — no-op"
            );
        }
    }
    let Some((record_channel_id, anchor)) =
        first_repost_anchor_if_all_probed_candidates_gone(&probed_anchors)
    else {
        return AnchorRepostOutcome::NotReposted;
    };

    // G4: the anchor is gone → send a NEW message (placeholder = None → send-new,
    // NOT an edit). Repost into the channel the anchor lived in. The D1 context
    // deliberately disables recorded-anchor reuse here because that anchor is the
    // one just proven gone; it still gates the send on the delivery lease and
    // records the replacement anchor after a successful POST.
    tracing::warn!(
        provider = %provider.as_str(),
        channel_id = state.channel_id,
        record_channel = record_channel_id,
        anchor_msg_id = anchor.panel_msg_id,
        anchor_channel_id = anchor.panel_channel_id,
        "  ↻ recovery anchor-repost: committed terminal message is GONE — reposting (send-new)"
    );

    // #3918: persist the send-new ATTEMPT BEFORE the send, and HARD-GATE the
    // send on that attempt being durably recorded. The durable `anchor_reposted`
    // marker recorded after a Delivered send is the primary at-most-once guard,
    // but it cannot cover the narrow window between "Discord accepted the
    // message" and "marker written". Counting the attempt up-front
    // (identity-guarded so a row now owned by a newer turn is never touched)
    // bounds that window: the G2c guard above refuses once the budget is reached.
    // But that bound ONLY holds when the attempt actually PERSISTS. If the bump
    // is not `Saved` we MUST NOT send:
    //   * `IoError` — the attempt did not persist. Sending anyway would escape
    //     the budget: under a persistent write fault every boot re-loads the SAME
    //     `attempts == 0` row and would send again → UNBOUNDED duplicate relay.
    //     Refusing is strictly safer; if the fault is transient a later boot's
    //     bump succeeds and the answer is re-posted then (deferred, not dropped).
    //   * `Missing` / `IdentityMismatch` — the row this answer belonged to is
    //     gone or now owned by a newer turn; re-posting a stale recovered answer
    //     is wrong anyway. Refuse.
    // No durable attempt record ⇒ no send. Only a `Saved` bump proceeds.
    //
    // On refusal we leave the on-disk row UNTOUCHED and return
    // `RefusedPreserveRow` (NOT `NotReposted`): the caller's `NotReposted` arm
    // force-clears the committed row, which would (a) drop a committed answer
    // whose bump hit a transient `IoError` — defeating the deferral, the answer
    // would be retried-then-reposted on a later boot — and (b) DELETE a row now
    // owned by a NEWER turn on `IdentityMismatch`. `RefusedPreserveRow` tells the
    // caller to preserve the row and move on. (`Missing` ⇒ the row is already
    // gone, so preserving is a safe no-op.)
    let repost_identity = inflight::InflightTurnIdentity::from_state(state);
    let bump_outcome = inflight::anchor_repost::bump_anchor_repost_attempts_if_matches_identity(
        provider,
        state.channel_id,
        &repost_identity,
        state.turn_start_offset,
    );
    if let Some(refusal) = anchor_repost_pre_send_refusal(bump_outcome) {
        tracing::warn!(
            provider = %provider.as_str(),
            channel_id = state.channel_id,
            outcome = ?bump_outcome,
            "  ⚠ recovery anchor-repost: pre-send attempt counter NOT persisted — \
             REFUSING the send-new and PRESERVING the inflight row (the hard bound \
             cannot be guaranteed without a durable attempt record; a transient \
             fault is re-posted by a later boot whose bump succeeds, and a \
             gone/replaced row must not be reposted nor cleared by this path)"
        );
        return refusal;
    }

    let Some(anchor_channel_id) = opt_channel_id(anchor.panel_channel_id) else {
        tracing::warn!(
            provider = %provider.as_str(),
            channel_id = state.channel_id,
            "recovery anchor-repost skipped because recorded anchor channel id is zero"
        );
        return AnchorRepostOutcome::NotReposted;
    };
    let Some(record_channel_id) = opt_channel_id(record_channel_id) else {
        tracing::warn!(
            provider = %provider.as_str(),
            channel_id = state.channel_id,
            "recovery anchor-repost skipped because delivery record channel id is zero"
        );
        return AnchorRepostOutcome::NotReposted;
    };
    let recovery_context =
        super::super::recovery_engine::RecoveryDeliveryContext::send_new_after_gone_anchor(
            provider,
            state,
            anchor_channel_id,
            Some(anchor.range),
            shared.restart.current_generation,
        )
        .with_record_channel_id(record_channel_id);
    let outcome = super::super::recovery_engine::relay_recovered_terminal_text_to_placeholder(
        http,
        shared,
        anchor_channel_id,
        None,
        terminal_text,
        Some(&recovery_context),
    )
    .await;

    // #3918: the answer reached Discord — record the durable idempotency marker
    // NOW, before the caller's `dispose_*` clears the row, so that if the clear
    // fails (or the process crashes after this write) the next boot re-loads
    // this row with `anchor_reposted = true` and the G2c guard refuses to post
    // the same answer a second time. Identity-guarded; a failed marker write
    // WARNs (the pre-send attempt counter still bounds the window).
    if matches!(outcome, RecoveryRelayOutcome::Delivered) {
        let marked = inflight::anchor_repost::mark_anchor_reposted_if_matches_identity(
            provider,
            state.channel_id,
            &repost_identity,
            state.turn_start_offset,
        );
        if !matches!(marked, inflight::GuardedSaveOutcome::Saved) {
            tracing::warn!(
                provider = %provider.as_str(),
                channel_id = state.channel_id,
                outcome = ?marked,
                "  ⚠ recovery anchor-repost: durable 'anchor_reposted' marker NOT persisted after \
                 a delivered send — a crash before the row clears could re-post (bounded by the \
                 pre-send attempt budget)"
            );
        }
    }

    // G5 (passive): see doc — the committed floor already covers this range, so the
    // watcher will not re-relay it; the caller disposes `outcome`.
    AnchorRepostOutcome::Relayed(outcome)
}

/// #3918 committed-branch entry point: run the anchor-repost send-new fallback
/// for an already-committed-but-vanished terminal answer AND apply the resulting
/// on-disk row disposition, returning whether the caller has fully handled the
/// row (so the committed-branch caller stays a single gated `continue`).
///
/// * `true` → the caller must `continue` WITHOUT running its legacy
///   committed-delivery clear, because EITHER the answer was relayed and disposed
///   ([`AnchorRepostOutcome::Relayed`]) OR the pre-send attempt bump did not
///   durably persist, so the send was REFUSED and the on-disk row deliberately
///   PRESERVED for a later boot ([`AnchorRepostOutcome::RefusedPreserveRow`] —
///   clearing it here would drop an `IoError`-deferred answer or delete a newer
///   turn's row on `IdentityMismatch`).
/// * `false` → no repost was needed/possible
///   ([`AnchorRepostOutcome::NotReposted`]) → the caller runs its legacy
///   committed-delivery finish + clear.
pub(in crate::services::discord) async fn recover_committed_anchor_repost(
    http: &Arc<serenity::Http>,
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    state: &inflight::InflightTurnState,
    terminal_text: &str,
) -> bool {
    let outcome = try_recover_anchor_repost(http, shared, provider, state, terminal_text).await;
    if let AnchorRepostOutcome::Relayed(relayed) = outcome {
        // #3610 PR-2 (codex r2 Issue-2, storm guard): pass `tmux_alive = false`
        // to the dispose so a repeatedly TransientFailure-ing send-new is
        // BUDGET-BOUNDED. This row's terminal answer is ALREADY committed — pane
        // liveness is irrelevant to whether the *anchor message* can be re-posted,
        // so the normal-turn "live pane may still own the answer" preservation
        // (`unrecoverable_relay_disposition`'s `tmux_alive == true` arm, shared.rs)
        // must NOT apply here. Were the real probe passed, a live pane would force
        // `PreserveAndCount` every boot and a transient send-new failure could loop
        // FOREVER (preserve+retry each restart). `tmux_alive` reaches only (a) the
        // disposition's budget gate and (b) the `termination_audit` `tmux_alive`
        // column — never a kill / extra force-clear path (verified) — so `false`
        // is the minimal, side-effect-free way to enforce the bound.
        dispose_recovery_relay_outcome(
            shared,
            provider,
            state,
            relayed,
            false,
            "recovery_anchor_repost",
            "anchor_repost",
            terminal_text,
            false,
        )
        .await;
    }
    committed_anchor_repost_handled(outcome)
}

/// Pure caller-interaction contract for [`recover_committed_anchor_repost`]:
/// has the committed row been fully handled (caller `continue`s WITHOUT the
/// legacy committed-delivery clear)? `Relayed` (answer relayed + disposed) and
/// `RefusedPreserveRow` (send refused, row deliberately PRESERVED for a later
/// boot) are BOTH handled; only `NotReposted` falls through to the legacy clear.
/// Extracted so the "refused ⇒ do NOT clear" contract is unit-testable without
/// the async send path.
fn committed_anchor_repost_handled(outcome: AnchorRepostOutcome) -> bool {
    match outcome {
        AnchorRepostOutcome::Relayed(_) | AnchorRepostOutcome::RefusedPreserveRow => true,
        AnchorRepostOutcome::NotReposted => false,
    }
}

/// Execute a non-`Delivered` [`RowDisposition`] for a recovery branch.
///
/// * `ClearPermanent` / `ClearBudgetExhausted` — persist the on-disk
///   force-clear report (the full response text + row metadata; a write
///   failure WARNs but never blocks the clear), termination audit (when the
///   row carries a `session_key`), the legacy missing-session handoff WARN
///   (unless the branch already emitted one), a structured WARN that ALWAYS
///   fires, then finish the mailbox (existing entries only) and clear the
///   inflight row.
/// * `PreserveAndCount` — persist `recovery_relay_attempts + 1` through
///   [`inflight::budget::bump_recovery_relay_attempts_if_matches_identity`],
///   which preserves restart/rebind markers on the carrier row (the #3297
///   finding-1 fix) while still refusing rows owned by a different turn, and
///   WARN with the attempt budget so the loop is observable.
/// * `FinishAndClear` is the caller's delivered epilogue — a no-op here.
#[allow(clippy::too_many_arguments)]
async fn apply_undeliverable_relay_disposition(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    state: &inflight::InflightTurnState,
    disposition: RowDisposition,
    branch: &'static str,
    tmux_alive: bool,
    best_response: &str,
    handoff_already_saved: bool,
) {
    match disposition {
        RowDisposition::FinishAndClear => {}
        RowDisposition::ClearPermanent | RowDisposition::ClearBudgetExhausted => {
            let reason_code = disposition_reason_code(disposition)
                .expect("clearing dispositions always carry a reason code");
            // #3297 finding 3: preserve the full response + row metadata on
            // disk BEFORE the row is destroyed. Failure to write must not
            // block the clear (the loop must still terminate) but is WARNed.
            let report = RecoveryForceClearReport::from_state(
                provider,
                state,
                branch,
                reason_code,
                best_response,
            );
            let report_path = match persist_force_clear_report(&report) {
                Ok(path) => Some(path),
                Err(error) => {
                    tracing::warn!(
                        provider = %provider.as_str(),
                        channel_id = state.channel_id,
                        reason_code,
                        error = %error,
                        "recovery force-clear report write FAILED — clearing anyway; \
                         full response text is NOT preserved on disk for this row"
                    );
                    None
                }
            };
            if let Some(ref session_key) = state.session_key {
                crate::services::termination_audit::record_termination_with_handles(
                    shared.pg_pool.as_ref(),
                    session_key,
                    state.dispatch_id.as_deref(),
                    "recovery",
                    reason_code,
                    Some("recovery terminal relay unrecoverable; inflight force-cleared"),
                    None,
                    Some(state.last_offset),
                    Some(tmux_alive),
                );
            }
            if !handoff_already_saved {
                save_missing_session_handoff(provider, state, best_response);
            }
            let report_path_display = report_path.as_ref().map(|path| path.display().to_string());
            tracing::warn!(
                provider = %provider.as_str(),
                channel_id = state.channel_id,
                user_msg_id = state.user_msg_id,
                branch,
                reason_code,
                attempts = state.recovery_relay_attempts,
                budget = inflight::RECOVERY_RELAY_RESTART_ATTEMPT_BUDGET,
                report_path = report_path_display.as_deref(),
                "recovery relay unrecoverable — force-clearing inflight row"
            );
            if let Some(channel_id) = opt_channel_id(state.channel_id) {
                finish_recovered_turn_mailbox_if_registered(
                    shared,
                    provider,
                    channel_id,
                    reason_code,
                )
                .await;
            } else {
                tracing::warn!(
                    provider = %provider.as_str(),
                    "recovery force-clear skipped mailbox cleanup because persisted channel id is zero"
                );
            }
            inflight::clear_inflight_state(provider, state.channel_id);
        }
        RowDisposition::PreserveAndCount => {
            let identity = inflight::InflightTurnIdentity::from_state(state);
            // #3297 finding 1: the carrier row is restart-marked on every
            // boot, so the counter must persist through a bump that PRESERVES
            // the marker instead of the generic guarded save that refuses it.
            let save_outcome = inflight::budget::bump_recovery_relay_attempts_if_matches_identity(
                provider,
                state.channel_id,
                &identity,
                state.turn_start_offset,
            );
            tracing::warn!(
                provider = %provider.as_str(),
                channel_id = state.channel_id,
                branch,
                attempts = state.recovery_relay_attempts.saturating_add(1),
                budget = inflight::RECOVERY_RELAY_RESTART_ATTEMPT_BUDGET,
                counter_persisted = matches!(save_outcome, inflight::GuardedSaveOutcome::Saved),
                "recovery relay failed — preserving inflight for retry on next restart"
            );
        }
    }
}

const FORCE_CLEAR_REPORT_VERSION: u32 = 1;

/// #3297 finding 3: the durable artifact a recovery force-clear leaves
/// behind. Written to `runtime/discord_recovery_force_clear/<provider>/`
/// immediately before the inflight row is destroyed, so the full assistant
/// response and enough row metadata to re-deliver it by hand survive even a
/// misclassified clear (e.g. a transient 403 surge read as permanent).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(in crate::services::discord) struct RecoveryForceClearReport {
    pub version: u32,
    pub provider: String,
    pub channel_id: u64,
    pub user_msg_id: u64,
    pub current_msg_id: u64,
    pub session_key: Option<String>,
    pub dispatch_id: Option<String>,
    pub tmux_session_name: Option<String>,
    pub branch: String,
    pub reason_code: String,
    pub attempts: u32,
    pub budget: u32,
    pub started_at: String,
    pub cleared_at: String,
    /// The FULL recovered response text (best of extracted output and the
    /// row's accumulated `full_response`) — never truncated.
    pub full_response: String,
}

impl RecoveryForceClearReport {
    fn from_state(
        provider: &ProviderKind,
        state: &inflight::InflightTurnState,
        branch: &str,
        reason_code: &str,
        best_response: &str,
    ) -> Self {
        Self {
            version: FORCE_CLEAR_REPORT_VERSION,
            provider: provider.as_str().to_string(),
            channel_id: state.channel_id,
            user_msg_id: state.user_msg_id,
            current_msg_id: state.current_msg_id,
            session_key: state.session_key.clone(),
            dispatch_id: state.dispatch_id.clone(),
            tmux_session_name: state.tmux_session_name.clone(),
            branch: branch.to_string(),
            reason_code: reason_code.to_string(),
            attempts: state.recovery_relay_attempts,
            budget: inflight::RECOVERY_RELAY_RESTART_ATTEMPT_BUDGET,
            started_at: state.started_at.clone(),
            cleared_at: chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
            full_response: best_response.to_string(),
        }
    }
}

/// Persist a [`RecoveryForceClearReport`] under the process runtime root.
/// File names carry a nanosecond suffix so repeated clears of the same
/// channel never overwrite an earlier artifact.
fn persist_force_clear_report(report: &RecoveryForceClearReport) -> Result<PathBuf, String> {
    let Some(root) = discord_recovery_force_clear_root() else {
        return Err("runtime root not resolvable".to_string());
    };
    persist_force_clear_report_in_root(&root, report)
}

/// Root-explicit inner form for unit tests.
fn persist_force_clear_report_in_root(
    root: &Path,
    report: &RecoveryForceClearReport,
) -> Result<PathBuf, String> {
    let dir = root.join(&report.provider);
    fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let path = dir.join(format!(
        "{}-{}-{nanos}.json",
        report.channel_id, report.reason_code
    ));
    let json = serde_json::to_string_pretty(report).map_err(|e| e.to_string())?;
    atomic_write(&path, &json)?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    //! #3297 finding-3 red-green: a force-clear must leave a real on-disk
    //! artifact carrying the FULL response text (the prior implementation
    //! only WARN-logged a 160-char tail and wrote nothing).
    use poise::serenity_prelude::ChannelId;
    use tempfile::TempDir;

    use super::super::super::inflight::GuardedSaveOutcome;
    use super::super::super::inflight::InflightTurnState;
    use super::super::super::outbound::delivery_frontier_probe;
    use super::super::super::outbound::delivery_record;
    use super::super::super::placeholder_sweeper::PlaceholderProbe;
    use super::{
        AnchorRepostOutcome, RecoveryForceClearReport, RecoveryRelayOutcome,
        anchor_panel_channel_matches_turn, anchor_probe_should_repost, anchor_range_matches_turn,
        anchor_record_lookup_channel_ids, anchor_repost_pre_send_refusal,
        committed_anchor_repost_handled, first_repost_anchor_if_all_probed_candidates_gone,
        matching_recovery_anchors, persist_force_clear_report_in_root,
    };
    use crate::services::provider::ProviderKind;
    use std::path::Path;

    struct AgentdeskRootGuard(Option<std::ffi::OsString>);

    impl AgentdeskRootGuard {
        fn set(path: &Path) -> Self {
            let previous = std::env::var_os("AGENTDESK_ROOT_DIR");
            unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", path) };
            Self(previous)
        }
    }

    impl Drop for AgentdeskRootGuard {
        fn drop(&mut self) {
            match self.0.take() {
                Some(value) => unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", value) },
                None => unsafe { std::env::remove_var("AGENTDESK_ROOT_DIR") },
            }
        }
    }

    fn make_state(channel_id: u64) -> InflightTurnState {
        let mut state: InflightTurnState = serde_json::from_value(serde_json::json!({
            "version": 9,
            "provider": "codex",
            "channel_id": channel_id,
            "channel_name": "adk-cdx",
            "request_owner_user_id": 7,
            "user_msg_id": 42,
            "current_msg_id": 43,
            "current_msg_len": 0,
            "user_text": "hello",
            "source": "text",
            "session_id": "session-3293",
            "tmux_session_name": format!("AgentDesk-codex-adk-cdx-{channel_id}"),
            "output_path": "/tmp/out.jsonl",
            "input_fifo_path": "/tmp/in.fifo",
            "last_offset": 0,
            "full_response": "",
            "response_sent_offset": 0,
            "started_at": "2026-06-28 10:00:00",
            "updated_at": "2026-06-28 10:00:00",
            "watcher_owns_live_relay": false
        }))
        .expect("test inflight row should deserialize");
        state.session_key = Some("sk-3293".to_string());
        state.dispatch_id = Some("disp-3293".to_string());
        state.recovery_relay_attempts = 2;
        state
    }

    #[cfg(unix)]
    fn touch_generation_marker(tmux_session_name: &str) -> i64 {
        let generation_path =
            crate::services::tmux_common::session_temp_path(tmux_session_name, "generation");
        if let Some(parent) = Path::new(&generation_path).parent() {
            std::fs::create_dir_all(parent).expect("create runtime sessions dir");
        }
        std::fs::write(&generation_path, "test-generation").expect("touch generation marker");
        crate::services::discord::tmux::read_generation_file_mtime_ns(tmux_session_name)
    }

    #[cfg(unix)]
    fn write_current_generation_anchor(
        provider: &ProviderKind,
        channel_id: u64,
        tmux_session_name: &str,
        range: (u64, u64),
        panel_channel_id: u64,
        panel_msg_id: u64,
    ) {
        let generation_mtime_ns =
            crate::services::discord::tmux::read_generation_file_mtime_ns(tmux_session_name);
        assert_ne!(
            generation_mtime_ns, 0,
            "test generation marker must exist before writing a current anchor"
        );
        delivery_record::write_delivered_frontier(
            provider,
            channel_id,
            delivery_record::DeliveredCommit {
                range,
                generation_mtime_ns,
                attempts: 1,
                panel_msg_id: Some(panel_msg_id),
                panel_channel_id: Some(panel_channel_id),
            },
        )
        .expect("write delivery record anchor");
    }

    /// #3918 (codex round-3): the pre-send bump-gate's REFUSAL must surface as
    /// `RefusedPreserveRow`, NOT `NotReposted`. The caller force-clears the
    /// committed row on `NotReposted`; routing a non-`Saved` bump there would
    /// (a) DROP a committed answer whose bump hit a transient `IoError` (the
    /// deferral is lost — the answer is re-posted by a later boot) and (b) DELETE
    /// a row now owned by a NEWER turn on `IdentityMismatch`. This pins that
    /// EVERY non-`Saved` bump yields the PRESERVE disposition (distinct from the
    /// clear disposition), and that ONLY `Saved` proceeds to the send.
    #[test]
    fn pre_send_refusal_preserves_row_for_every_non_saved_bump() {
        // `Saved` ⇒ the durable attempt record exists ⇒ proceed to send-new
        // (`None` == "no refusal").
        assert_eq!(
            anchor_repost_pre_send_refusal(GuardedSaveOutcome::Saved),
            None
        );

        // Every non-`Saved` outcome ⇒ refuse the send AND preserve the row —
        // never `NotReposted` (which the caller clears on).
        for failure in [
            GuardedSaveOutcome::IoError,
            GuardedSaveOutcome::Missing,
            GuardedSaveOutcome::IdentityMismatch,
        ] {
            assert_eq!(
                anchor_repost_pre_send_refusal(failure),
                Some(AnchorRepostOutcome::RefusedPreserveRow),
                "{failure:?} must refuse the send AND preserve the row (never NotReposted/clear)"
            );
        }

        // The preserve disposition the caller must NOT clear on is distinct from
        // the legacy committed-delivery clear disposition.
        assert_ne!(
            AnchorRepostOutcome::RefusedPreserveRow,
            AnchorRepostOutcome::NotReposted,
            "the refuse-and-preserve disposition must be distinguishable from the clear disposition"
        );
    }

    /// #3918 (codex round-3): the caller-interaction contract. The committed
    /// branch clears the row ONLY when `recover_committed_anchor_repost` returns
    /// `false`. `RefusedPreserveRow` MUST be `handled == true` (preserve the row,
    /// do NOT clear) so a non-`Saved` pre-send bump never drops an IoError-
    /// deferred answer nor deletes a newer turn's row; `Relayed(*)` is also
    /// handled (already disposed); ONLY `NotReposted` falls through to the legacy
    /// clear.
    #[test]
    fn refused_preserve_row_is_handled_so_the_caller_does_not_clear() {
        // The PRESERVE case: handled ⇒ the caller `continue`s WITHOUT clearing.
        assert!(
            committed_anchor_repost_handled(AnchorRepostOutcome::RefusedPreserveRow),
            "RefusedPreserveRow MUST be handled (preserve the row; the caller must NOT clear it)"
        );
        // Every relayed disposition is also handled (the relay was already
        // disposed) — none falls through to the legacy clear.
        for relayed in [
            RecoveryRelayOutcome::Delivered,
            RecoveryRelayOutcome::TransientFailure,
            RecoveryRelayOutcome::PermanentFailure,
        ] {
            assert!(
                committed_anchor_repost_handled(AnchorRepostOutcome::Relayed(relayed)),
                "Relayed({relayed:?}) must be handled (already disposed; no legacy clear)"
            );
        }
        // ONLY the no-repost case falls through to the legacy committed clear.
        assert!(
            !committed_anchor_repost_handled(AnchorRepostOutcome::NotReposted),
            "NotReposted must fall through to the legacy committed-delivery clear"
        );
    }

    #[test]
    fn force_clear_report_preserves_full_response_and_metadata_on_disk() {
        let temp = TempDir::new().unwrap();
        let state = make_state(932_951);
        // Long enough that the legacy 160-char WARN tail provably truncates.
        let full_response = "A".repeat(4000) + " — final answer body";
        let report = RecoveryForceClearReport::from_state(
            &ProviderKind::Codex,
            &state,
            "missing_tmux",
            "recovery_retry_budget_exhausted",
            &full_response,
        );

        let path = persist_force_clear_report_in_root(temp.path(), &report)
            .expect("force-clear report must be written to disk");
        assert!(path.starts_with(temp.path().join("codex")));

        let written = std::fs::read_to_string(&path).expect("artifact must exist on disk");
        let parsed: RecoveryForceClearReport =
            serde_json::from_str(&written).expect("artifact must round-trip");
        assert_eq!(
            parsed.full_response, full_response,
            "FULL text, no truncation"
        );
        assert_eq!(parsed.channel_id, 932_951);
        assert_eq!(parsed.session_key.as_deref(), Some("sk-3293"));
        assert_eq!(parsed.dispatch_id.as_deref(), Some("disp-3293"));
        assert_eq!(parsed.reason_code, "recovery_retry_budget_exhausted");
        assert_eq!(parsed.branch, "missing_tmux");
        assert_eq!(parsed.attempts, 2);
        assert_eq!(
            parsed.tmux_session_name.as_deref(),
            Some("AgentDesk-codex-adk-cdx-932951")
        );
    }

    #[test]
    fn repeated_force_clears_never_overwrite_earlier_artifacts() {
        let temp = TempDir::new().unwrap();
        let state = make_state(932_952);
        let first = RecoveryForceClearReport::from_state(
            &ProviderKind::Codex,
            &state,
            "missing_tmux",
            "recovery_permanent_relay_failure",
            "first response",
        );
        let second = RecoveryForceClearReport::from_state(
            &ProviderKind::Codex,
            &state,
            "missing_tmux",
            "recovery_permanent_relay_failure",
            "second response",
        );
        let path_a = persist_force_clear_report_in_root(temp.path(), &first).unwrap();
        let path_b = persist_force_clear_report_in_root(temp.path(), &second).unwrap();
        assert_ne!(path_a, path_b, "artifacts must be uniquely named");
        assert!(path_a.exists() && path_b.exists());
    }

    // ---- #3610 PR-2 anchor-repost pure guards ---------------------------------

    #[test]
    fn anchor_probe_reposts_only_on_message_gone() {
        // The duplicate-repost defense: a permanently-gone message is the ONLY
        // verdict that reposts. A live message must never be duplicated, and a
        // transient probe error must never be read as "gone".
        assert!(anchor_probe_should_repost(PlaceholderProbe::MessageGone));
        assert!(!anchor_probe_should_repost(
            PlaceholderProbe::StillPlaceholder
        ));
        assert!(!anchor_probe_should_repost(
            PlaceholderProbe::AlreadyDelivered
        ));
        assert!(!anchor_probe_should_repost(PlaceholderProbe::ProbeFailed));
    }

    #[test]
    fn anchor_range_matches_turn_accepts_this_turns_slice() {
        // A real slice whose end reaches the row's last_offset, with the turn
        // start at/below the anchor end → this turn's anchor.
        assert!(anchor_range_matches_turn((10, 443_154), Some(10), 443_154));
        // last_offset slightly below end is fine (commit covered all the row saw).
        assert!(anchor_range_matches_turn((0, 500), Some(0), 480));
        // No turn_start_offset signal (None) → the start check is skipped.
        assert!(anchor_range_matches_turn((0, 500), None, 500));
    }

    #[test]
    fn anchor_range_matches_turn_rejects_mismatched_or_empty() {
        // (a) empty / zero range → reject (blank/sentinel frontier).
        assert!(!anchor_range_matches_turn((0, 0), Some(0), 0));
        assert!(!anchor_range_matches_turn((42, 42), Some(42), 42));
        // (b) anchor end BELOW the row's last_offset → the commit did not cover
        // what this turn last observed (stale/partial anchor) → reject.
        assert!(!anchor_range_matches_turn((0, 100), Some(0), 443_154));
        // (b2) anchor start ABOVE/AT the row's last_offset → this is a later
        // owner-frontier slice, not the recovered row's terminal anchor.
        assert!(!anchor_range_matches_turn((200, 300), Some(0), 100));
        assert!(!anchor_range_matches_turn((100, 300), Some(0), 100));
        // (c) the turn's start is ABOVE the anchor end → a later turn → reject.
        assert!(!anchor_range_matches_turn((0, 100), Some(200), 100));
    }

    #[test]
    fn anchor_panel_channel_must_match_delivery_channel() {
        assert!(anchor_panel_channel_matches_turn(200, 200));
        assert!(!anchor_panel_channel_matches_turn(0, 200));
        assert!(!anchor_panel_channel_matches_turn(300, 200));
    }

    #[test]
    fn anchor_record_lookup_uses_owner_channel_when_persisted() {
        let _lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let temp = TempDir::new().unwrap();
        let _root_guard = AgentdeskRootGuard::set(temp.path());
        let mut state = make_state(200);
        state.logical_channel_id = Some(999);
        state.set_watcher_owner_channel_id(100);

        assert_eq!(
            anchor_record_lookup_channel_ids(&ProviderKind::Codex, &state),
            vec![100]
        );
    }

    #[cfg(unix)]
    #[test]
    fn anchor_record_lookup_uses_sidecar_owner_when_inflight_field_missing() {
        let _lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let temp = TempDir::new().unwrap();
        let _root_guard = AgentdeskRootGuard::set(temp.path());
        let mut state = make_state(200);
        state.watcher_owner_channel_id = None;
        state.logical_channel_id = Some(999);
        let tmux_session_name = state.tmux_session_name.as_deref().unwrap();
        touch_generation_marker(tmux_session_name);
        delivery_record::record_watcher_owner_channel_context(
            &ProviderKind::Codex,
            ChannelId::new(200),
            ChannelId::new(100),
            tmux_session_name,
        )
        .expect("write owner-context sidecar");

        assert_eq!(
            anchor_record_lookup_channel_ids(&ProviderKind::Codex, &state),
            vec![100, 200]
        );
    }

    #[test]
    fn anchor_record_lookup_falls_back_to_delivery_channel_for_legacy_rows() {
        let _lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let temp = TempDir::new().unwrap();
        let _root_guard = AgentdeskRootGuard::set(temp.path());
        let mut state = make_state(200);
        state.watcher_owner_channel_id = None;
        state.logical_channel_id = Some(999);

        assert_eq!(
            anchor_record_lookup_channel_ids(&ProviderKind::Codex, &state),
            vec![200]
        );
    }

    #[cfg(unix)]
    #[test]
    fn anchor_selection_falls_back_when_stale_sidecar_anchor_is_absent() {
        let _lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let temp = TempDir::new().unwrap();
        let _root_guard = AgentdeskRootGuard::set(temp.path());
        let mut state = make_state(200);
        let output_path = temp.path().join("out.jsonl");
        std::fs::write(&output_path, vec![b'x'; 200]).expect("write bounded transcript");
        state.output_path = Some(output_path.to_string_lossy().into_owned());
        state.watcher_owner_channel_id = None;
        state.turn_start_offset = Some(0);
        state.last_offset = 150;
        let tmux_session_name = state.tmux_session_name.as_deref().unwrap();
        let generation_mtime_ns = touch_generation_marker(tmux_session_name);
        assert_ne!(generation_mtime_ns, 0);
        delivery_record::record_watcher_owner_channel_context(
            &ProviderKind::Codex,
            ChannelId::new(200),
            ChannelId::new(100),
            tmux_session_name,
        )
        .expect("write stale owner-context sidecar");
        write_current_generation_anchor(
            &ProviderKind::Codex,
            200,
            tmux_session_name,
            (0, 200),
            200,
            999,
        );

        let anchors = matching_recovery_anchors(&ProviderKind::Codex, &state, tmux_session_name);

        assert_eq!(anchors.len(), 1);
        let (record_channel_id, anchor) = anchors[0];
        assert_eq!(record_channel_id, 200);
        assert_eq!(anchor.panel_channel_id, 200);
        assert_eq!(anchor.panel_msg_id, 999);
    }

    #[cfg(unix)]
    #[test]
    fn anchor_selection_keeps_legacy_candidate_when_sidecar_anchor_also_matches() {
        let _lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let temp = TempDir::new().unwrap();
        let _root_guard = AgentdeskRootGuard::set(temp.path());
        let mut state = make_state(200);
        let output_path = temp.path().join("out.jsonl");
        std::fs::write(&output_path, vec![b'x'; 200]).expect("write bounded transcript");
        state.output_path = Some(output_path.to_string_lossy().into_owned());
        state.watcher_owner_channel_id = None;
        state.turn_start_offset = Some(0);
        state.last_offset = 150;
        let tmux_session_name = state.tmux_session_name.as_deref().unwrap();
        touch_generation_marker(tmux_session_name);
        delivery_record::record_watcher_owner_channel_context(
            &ProviderKind::Codex,
            ChannelId::new(200),
            ChannelId::new(100),
            tmux_session_name,
        )
        .expect("write owner-context sidecar");
        write_current_generation_anchor(
            &ProviderKind::Codex,
            100,
            tmux_session_name,
            (0, 200),
            200,
            888,
        );
        write_current_generation_anchor(
            &ProviderKind::Codex,
            200,
            tmux_session_name,
            (0, 200),
            200,
            999,
        );

        let anchors = matching_recovery_anchors(&ProviderKind::Codex, &state, tmux_session_name);

        assert_eq!(
            anchors
                .iter()
                .map(|(record_channel_id, anchor)| (*record_channel_id, anchor.panel_msg_id))
                .collect::<Vec<_>>(),
            vec![(100, 888), (200, 999)]
        );
    }

    #[test]
    fn anchor_probe_sequence_reposts_only_when_all_candidates_are_gone() {
        let anchor = delivery_frontier_probe::CurrentGenerationAnchor {
            panel_msg_id: 888,
            panel_channel_id: 200,
            range: (0, 200),
        };
        assert_eq!(
            first_repost_anchor_if_all_probed_candidates_gone(&[(
                100,
                anchor,
                PlaceholderProbe::MessageGone,
            )]),
            Some((100, anchor))
        );
        assert_eq!(
            first_repost_anchor_if_all_probed_candidates_gone(&[
                (100, anchor, PlaceholderProbe::MessageGone),
                (200, anchor, PlaceholderProbe::AlreadyDelivered),
            ]),
            None
        );
        assert_eq!(
            first_repost_anchor_if_all_probed_candidates_gone(&[
                (100, anchor, PlaceholderProbe::MessageGone),
                (200, anchor, PlaceholderProbe::ProbeFailed),
            ]),
            None
        );
    }
}
