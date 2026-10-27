//! #3089 Phase B0 — durable delivery-record sidecar store (mixed-binary safe).
//! Phase B gives turn-output delivery a **durable** authority that survives a
//! dcserver restart (AC6). B0 resolves design §4.4's hard blocker:
//! **mixed-binary safety**.
//!
//! ## Why a sidecar (not new `inflight.rs` fields)
//!
//! Old binaries deserialize `InflightTurnState`, then reserialize it with
//! `serde_json::to_string_pretty` (`inflight.rs`), **dropping unknown fields**;
//! standby rewrites the same `{channel_id}.json` on its heartbeat. So a
//! `#[serde(default)]` field added on a new primary would be silently erased by
//! an older standby — `#[serde(default)]` + tolerant deserialize is NOT enough
//! (design §4.4 / risk #4). The resolution (option a, the default) is a
//! **sidecar store old binaries never read or scan**.
//!
//! ## The isolation guarantee (the entire B0 deliverable)
//!
//! Old binaries enumerate the inflight provider dir and **delete malformed**
//! entries: `load_inflight_states_from_root` (`inflight.rs:2577`) does a
//! NON-recursive `fs::read_dir(runtime/discord_inflight/<provider>/)` and, for
//! every entry whose `extension == "json"`, parses it and `fs::remove_file`s the
//! malformed ones (`inflight.rs:2587`+). The two other inflight enumerations
//! (`:800` sweep, `:2251` clear-by-tmux) apply the same `"json"` filter on the
//! same dir.
//!
//! The delivery record store therefore lives in a **dedicated sibling subtree**
//! `runtime/discord_delivery_records/<provider>/<channel_id>.json` — a peer of
//! `runtime/discord_inflight/`, matching the existing sidecar-root convention in
//! `runtime_store.rs` (`discord_status_panel_orphans`,
//! `discord_queued_placeholders`, …). Because the reaper's `read_dir` is
//! non-recursive and only ever opens `discord_inflight/<provider>/`, it never
//! descends into `discord_delivery_records/`. The delivery-channel -> owner-channel
//! context added by #3751 lives in a **separate** sibling subtree,
//! `runtime/discord_delivery_owner_context/<provider>/<channel_id>.json`, because
//! older binaries can still rewrite the known delivery-record JSON object and drop
//! unknown fields. The directory — not the `.json` extension — is the isolator
//! (`sidecar_path_is_outside_inflight_scan_set` pins this). No other component
//! (standby heartbeat rewrites only `{channel_id}.json` via
//! `inflight::save_inflight_state`; the sweeper / recovery read via the inflight
//! loaders) knows these paths exist.
//!
//! Relay consumers use this sidecar for durable delivery state.
//!
//! ## Conservatism (invariants I2 / I3)
//!
//! - `read_record` returns `None` on a missing OR malformed file — never
//!   "assume delivered" (I3). A B1 reader treats `None` as not-yet-delivered.
//! - Only [`write_delivered_frontier`] advances `delivered_frontier` — a lease
//!   acquire/clear never touches it, so an ambiguous (`Unknown`/`NotDelivered`)
//!   commit never advances the durable offset (I2).
//! - `clear_lease_at` (release) clears `delivery_lease` ONLY; `delivered_frontier`
//!   survives (design §4.3).

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::atomic::Ordering;

use poise::serenity_prelude::ChannelId;
use serde::{Deserialize, Serialize};

use super::completed_turn_ledger::{self, append_completed_episode};
use super::turn_output_controller::DeliveryOutcome;
use crate::services::discord::runtime_store;
use crate::services::provider::ProviderKind;

mod content;
mod frontier;
mod owner_context;
mod rollout;
mod store;

use content::record_delivered_content_fingerprint_for_generation;
pub(in crate::services::discord) use content::{
    recent_delivered_content_matches, recent_delivered_content_matches_for_generation,
    record_delivered_content_fingerprint,
};
#[cfg(test)]
use content::{recent_delivered_content_matches_at, record_delivered_content_fingerprint_at};
pub(in crate::services::discord::outbound) use content::{
    recent_fresh_send_content_matches, record_fresh_send_content_fingerprint,
};
pub(in crate::services::discord) use frontier::{
    EditFailureTranscriptIdentity, authority_blocks_backward_inflight_write,
    capture_edit_failure_transcript_identity, committed_floor_for_resend_dedup,
    current_generation_mtime_ns, delivered_frontier_end_current_generation,
    delivered_frontier_exceeding_current_eof, durable_frontier_generation_current,
    effective_committed_offset, range_already_committed, range_committed_after_edit_failure,
    reanchor_current_generation_frontier, resolved_delivered_frontier_end_current_generation,
};
pub(in crate::services::discord::outbound) use frontier::{
    current_generation_durable_frontier_at, current_generation_durable_frontier_end_at,
};
#[cfg(test)]
use frontier::{
    current_generation_frontier_exceeding_eof_at, fuse_committed_offset,
    reanchor_current_generation_frontier_at, stable_edit_failure_frontier_at,
};
use owner_context::pinned_source_receipt;
pub(in crate::services::discord) use owner_context::{
    commit_ordered_jsonl_range, confirmed_delivery_receipt_exists,
    historical_pinned_delivery_exists, record_historical_pinned_delivery,
    record_watcher_owner_channel_context, watcher_owner_channel_for_delivery_channel,
};
#[cfg(test)]
use owner_context::{
    commit_ordered_jsonl_range_at, commit_ordered_jsonl_range_at_with_before_lock,
    watcher_owner_channel_from_context_at, write_watcher_owner_context_at,
};
pub(super) use rollout::delivery_record_rollout_health_json;
pub(in crate::services::discord) use rollout::{
    FlagResolution, FlagSource, delivery_record_authority_enabled, delivery_record_shadow_enabled,
    outcome_is_shadow_delivered,
};
use rollout::{delivered_frontier_end_diverged, should_shadow_mirror};
#[cfg(test)]
use rollout::{
    delivery_record_rollout_health_json_for_flags, resolve_authority_flag, resolve_shadow_flag,
};
#[cfg(all(test, unix))]
pub(in crate::services::discord) use store::write_proven_gone_equal_range_frontier;
#[cfg(all(test, unix))]
use store::write_proven_gone_equal_range_frontier_at_with_before_lock;
pub(in crate::services::discord) use store::{
    ConfirmedDeliveryReceipt, DeliveredCommit, DeliveredContentFingerprint, DeliveryRecord,
    DeliveryRecordLock, ExactJsonlSourceIdentity, RecoveryDeliveryRecordAuthority,
    WatcherDeliveryRecordAuthority, WatcherOwnerContext, delivery_record_path, lock_record_path,
    read_record_at, write_delivered_frontier,
};
#[cfg(test)]
use store::{
    DELIVERY_OWNER_CONTEXT_DIR, FRESH_SEND_RECORDS_DIR, clear_lease_at,
    delivery_owner_context_path_in_root, upsert_lease_at,
    write_confirmed_frontier_guarded_at_with_before_lock,
};
use store::{
    DELIVERY_RECORDS_DIR, EqualRangeAnchorPolicy, RECENT_DELIVERED_CONTENT_LIMIT,
    RECENT_DELIVERED_CONTENT_WINDOW_MS, WatcherFrontierLockAuthority, append_confirmed_receipt,
    delivery_owner_context_path, fresh_send_record_path, mutate_record_at,
    owner_context_path_or_err, record_path_or_err, write_confirmed_delivery_at,
    write_confirmed_delivery_at_with_lock_authority,
    write_confirmed_frontier_guarded_at_with_lock_authority,
    write_delivered_frontier_guarded_at_with_before_lock, write_record_at,
};

/// The transient lease (design §4.3). `deadline_epoch_ms` is ABSOLUTE
/// wall-clock (review r2 H): the in-memory `lease_now_ms()` is process-monotonic
/// and meaningless after a restart. `holder_generation` distinguishes a
/// pre-restart holder (reclaim immediately) from a live one.
#[allow(dead_code)] // #3089 B0: constructed by B1's lease writer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(in crate::services::discord) struct DurableLease {
    /// Stable serialized holder id. The in-memory `LeaseHolder` enum
    /// (Watcher/Sink/Bridge) maps to this in B1's writer — kept a plain `u64`
    /// here so the durable record carries no cross-module / serde-fragile dep.
    pub holder_id: u64,
    pub attempt_id: u64,
    pub range: (u64, u64),
    pub deadline_epoch_ms: u64,
    pub holder_generation: i64,
}

/// `<runtime_root>/discord_delivery_records/<provider>/<channel_id>.json`.
/// Inner form takes the `runtime/` dir explicitly so tests can target an
/// isolated tempdir without the `AGENTDESK_ROOT_DIR` env dance.
#[allow(dead_code)] // #3089 B0: exercised by the store tests; B1 uses the env-resolved form.
fn delivery_record_path_in_root(
    runtime_root: &Path,
    provider: &ProviderKind,
    channel_id: u64,
) -> PathBuf {
    runtime_root
        .join(DELIVERY_RECORDS_DIR)
        .join(provider.as_str())
        .join(format!("{channel_id}.json"))
}

/// Read the durable record. `None` when the runtime root is unavailable, the
/// file is missing, or the content is malformed (I3 conservative).
#[allow(dead_code)] // #3089 B0: read by B1 shadow assert / B2 read-authority.
pub(in crate::services::discord) fn read_record(
    provider: &ProviderKind,
    channel_id: u64,
) -> Option<DeliveryRecord> {
    read_record_at(&delivery_record_path(provider, channel_id)?)
}

/// Raw path writer retained for record-shape tests and narrowly-scoped repair
/// helpers. Production confirmed-delivery entry points must use the
/// generation-guarded writers below.
#[cfg(test)]
fn write_delivered_frontier_at(path: &Path, frontier: DeliveredCommit) -> Result<(), String> {
    mutate_record_at(path, |record| record.delivered_frontier = Some(frontier))
}

/// Atomically persist the monotonic frontier and the exact Discord receipt.
/// This is delivery authority, never rollout telemetry, so callers must invoke
/// it after a confirmed POST regardless of the shadow flag.
pub(in crate::services::discord) fn write_confirmed_delivery(
    provider: &ProviderKind,
    offset_authority_channel_id: u64,
    frontier: DeliveredCommit,
    receipt: ConfirmedDeliveryReceipt,
) -> Result<(), String> {
    if receipt.source.offset_authority_channel_id != offset_authority_channel_id {
        return Err("delivery_record: receipt owner does not match record key".into());
    }
    if receipt.source.provider != provider.as_str() {
        return Err("delivery_record: receipt provider does not match record key".into());
    }
    write_confirmed_delivery_at(
        &record_path_or_err(provider, offset_authority_channel_id)?,
        frontier,
        receipt,
    )
}

#[allow(dead_code)]
pub(in crate::services::discord) fn record_current_pinned_delivery(
    source: &ExactJsonlSourceIdentity,
    message_id: u64,
) -> Result<(), String> {
    let (provider, receipt) = pinned_source_receipt(source, message_id)?;
    let frontier = DeliveredCommit {
        range: source.range,
        generation_mtime_ns: source.generation_mtime_ns,
        attempts: 0,
        panel_msg_id: Some(message_id),
        panel_channel_id: Some(source.delivery_channel_id),
    };
    write_confirmed_delivery_at(
        &record_path_or_err(&provider, source.offset_authority_channel_id)?,
        frontier,
        receipt,
    )
}
pub(in crate::services::discord) fn record_pinned_delivery_metadata(
    source: &ExactJsonlSourceIdentity,
    body: &str,
    user_msg_id: u64,
) {
    let Some(provider) = ProviderKind::from_str(&source.provider) else {
        return;
    };
    // #5264 PR-B: the fingerprint goes under the OFFSET-AUTHORITY (watcher owner) channel
    // and the ledger under the delivery channel, matching
    // `shadow_mirror_delivered_frontier_inner`. Every reader of the fingerprint
    // (`tmux_watcher/turn_identity.rs`, `tmux_watcher/terminal_preflight.rs`) looks it up
    // under the watcher's owner channel, and the record path plus the hash are both keyed
    // by channel, so writing it under the delivery channel strands it whenever owner and
    // destination differ — leaving the duplicate defence inert in exactly the split-channel
    // case it exists for.
    record_delivered_content_fingerprint_for_generation(
        &provider,
        source.offset_authority_channel_id,
        body,
        source.generation_mtime_ns,
    );
    append_completed_episode(
        &provider,
        source.delivery_channel_id,
        user_msg_id,
        Some(source.turn_nonce.as_str()).filter(|nonce| !nonce.is_empty()),
    );
}
/// Remove the whole record (turn-end GC). `true` if a file was removed.
/// Test-only: no production GC path calls it.
#[cfg(test)]
fn delete_record_at(path: &Path) -> bool {
    fs::remove_file(path).is_ok()
}

/// #4130 test seam: a per-thread override of
/// `delivery_record_shadow_enabled()` so default-OFF tests stay deterministic
/// even when the developer shell exports `AGENTDESK_DELIVERY_RECORD_SHADOW=1`.
#[cfg(test)]
pub(in crate::services::discord) mod shadow_test_seam {
    use std::cell::Cell;

    thread_local! {
        static OVERRIDE: Cell<Option<bool>> = const { Cell::new(None) };
    }

    pub(in crate::services::discord) fn current_override() -> Option<bool> {
        OVERRIDE.with(Cell::get)
    }

    #[must_use]
    pub(in crate::services::discord) fn force(value: bool) -> Guard {
        Guard {
            previous: OVERRIDE.with(|cell| cell.replace(Some(value))),
        }
    }

    pub(in crate::services::discord) struct Guard {
        previous: Option<bool>,
    }

    impl Drop for Guard {
        fn drop(&mut self) {
            OVERRIDE.with(|cell| cell.set(self.previous));
        }
    }
}

/// #3933 test seam: a per-thread override of `delivery_record_authority_enabled()`
/// so a unit test can drive the authority-ON enforce path (the release config)
/// deterministically. Housed in a `#[cfg(test)] mod` (not inline `#[cfg(test)]`
/// items) so the seam counts as test — not production — review surface.
#[cfg(test)]
pub(in crate::services::discord) mod authority_test_seam {
    use std::cell::Cell;

    thread_local! {
        /// When `Some`, forces the flag on THIS thread only; `None` (default)
        /// falls through to the env-cached `OnceLock`.
        static OVERRIDE: Cell<Option<bool>> = const { Cell::new(None) };
    }

    pub(in crate::services::discord) fn current_override() -> Option<bool> {
        OVERRIDE.with(Cell::get)
    }

    /// RAII: force `delivery_record_authority_enabled()` to `value` on the
    /// current thread until the returned guard drops (then restore the prior
    /// override). The env-cached `OnceLock` cannot be re-set once initialized,
    /// and mutating the process-global env would race sibling tests, so a
    /// thread-local + RAII keeps the authority-ON honor path order-independent
    /// and leak-free.
    #[must_use]
    pub(in crate::services::discord) fn force(value: bool) -> Guard {
        Guard {
            previous: OVERRIDE.with(|cell| cell.replace(Some(value))),
        }
    }

    pub(in crate::services::discord) struct Guard {
        previous: Option<bool>,
    }

    impl Drop for Guard {
        fn drop(&mut self) {
            OVERRIDE.with(|cell| cell.set(self.previous));
        }
    }
}

/// Path-based core (testable): write the frontier and report whether its END
/// diverged from the in-memory authority. `Err` only when the durable write
/// itself failed. Caller invokes this ONLY for a confirmed `Delivered` (I2).
#[allow(clippy::too_many_arguments)]
#[cfg(test)]
fn record_delivered_frontier_shadow_at(
    path: &Path,
    range: (u64, u64),
    generation_mtime_ns: i64,
    attempts: u32,
    panel_msg_id: Option<u64>,
    panel_channel_id: Option<u64>,
    in_memory_confirmed_end: u64,
) -> Result<bool, String> {
    write_delivered_frontier_at(
        path,
        DeliveredCommit {
            range,
            generation_mtime_ns,
            attempts,
            panel_msg_id,
            panel_channel_id,
        },
    )?;
    Ok(delivered_frontier_end_diverged(
        range.1,
        in_memory_confirmed_end,
    ))
}

fn exact_receipt_from_inflight(
    provider: &ProviderKind,
    fresh: Option<&crate::services::discord::inflight::InflightTurnState>,
    offset_authority_channel: ChannelId,
    delivery_channel: ChannelId,
    message_id: Option<u64>,
    range: (u64, u64),
    generation_mtime_ns: i64,
) -> Option<ConfirmedDeliveryReceipt> {
    let fresh = fresh?;
    let message_id = message_id.filter(|id| *id != 0)?;
    let tmux_session_name = fresh
        .tmux_session_name
        .as_deref()
        .filter(|name| !name.is_empty())?;
    let turn_nonce = fresh
        .turn_nonce
        .as_deref()
        .filter(|nonce| !nonce.is_empty())?;
    if fresh.provider_kind().as_ref() != Some(provider)
        || fresh.channel_id != delivery_channel.get()
        || fresh.delivery_record_owner_channel_id() != offset_authority_channel.get()
        || fresh.turn_start_offset != Some(range.0)
        || fresh.last_offset != range.1
        || range.1 <= range.0
        || generation_mtime_ns == 0
        || current_generation_mtime_ns(tmux_session_name) != generation_mtime_ns
    {
        return None;
    }
    Some(ConfirmedDeliveryReceipt {
        source: ExactJsonlSourceIdentity {
            provider: provider.as_str().to_string(),
            tmux_session_name: tmux_session_name.to_string(),
            turn_nonce: turn_nonce.to_string(),
            range,
            generation_mtime_ns,
            offset_authority_channel_id: offset_authority_channel.get(),
            delivery_channel_id: delivery_channel.get(),
        },
        delivery_channel_id: delivery_channel.get(),
        message_id,
    })
}

fn persist_confirmed_frontier_and_receipt(
    provider: &ProviderKind,
    offset_authority_channel: ChannelId,
    tmux_session_name: Option<&str>,
    frontier: DeliveredCommit,
    receipt: Option<ConfirmedDeliveryReceipt>,
    equal_range_anchor_policy: EqualRangeAnchorPolicy,
    lock_authority: Option<WatcherFrontierLockAuthority<'_>>,
) -> Result<(), String> {
    match receipt {
        Some(receipt) => {
            if receipt.source.offset_authority_channel_id != offset_authority_channel.get() {
                return Err("delivery_record: receipt owner does not match record key".into());
            }
            if receipt.source.provider != provider.as_str() {
                return Err("delivery_record: receipt provider does not match record key".into());
            }
            write_confirmed_delivery_at_with_lock_authority(
                &record_path_or_err(provider, offset_authority_channel.get())?,
                frontier,
                receipt,
                equal_range_anchor_policy,
                lock_authority,
            )
        }
        None => {
            let tmux_session_name = tmux_session_name
                .filter(|name| !name.is_empty())
                .ok_or_else(|| {
                    "delivery_record: receipt-less frontier lacks tmux incarnation identity"
                        .to_string()
                })?;
            write_confirmed_frontier_guarded_at_with_lock_authority(
                &record_path_or_err(provider, offset_authority_channel.get())?,
                tmux_session_name,
                frontier,
                None,
                equal_range_anchor_policy,
                lock_authority,
                || {},
            )
        }
    }
}

/// Integration wrapper for owner `Delivered` arms. A confirmed outcome always
/// persists the frontier, and atomically appends an exact receipt when the
/// delivery-channel inflight still exposes a complete turn identity. The
/// rollout flag controls only post-write divergence telemetry. Non-`Delivered`
/// outcomes never write delivery authority (I2).
///
/// #3610 (Phase B PR-1): `terminal_anchor_msg_id` is the durable TERMINAL ANCHOR
/// the caller resolved — the Discord message id terminal-replace edits in place
/// (= the placeholder/active-slot `current_msg_id`, the assistant response
/// message). It is recorded verbatim into [`DeliveredCommit::panel_msg_id`]. This
/// REPLACES the prior `fresh.status_message_id` read, which was the WRONG datum:
/// `status_message_id` is the status-panel-v2 id (inflight/model.rs: "current_msg_id
/// remains the assistant response"), not the terminal anchor, and is `null` on
/// channels that do not use the status panel — yielding `panel_msg_id = null` on
/// the very incidents PR-2's re-post will key off.
///
/// #3610 (Phase B PR-1b): `terminal_anchor_channel_id` is the channel that anchor
/// message LIVES IN — recorded into [`DeliveredCommit::panel_channel_id`]. The
/// frontier is KEYED by `channel` (the OFFSET-AUTHORITY channel,
/// `watcher_owner_channel_id` for the bridge cutover), which the cross-channel
/// cutover can resolve DIFFERENTLY from the edit-target channel the `msg_id`
/// belongs to. So PR-2 must record the (channel, msg) PAIR, not just the msg.
/// Same-channel callers (sink, watcher) pass `Some(channel.get())`; the bridge
/// cutover passes the edit-target `channel_id` it actually edits. `None`/`None`
/// writes a null anchor pair (unchanged from the absent-status-panel case), so
/// A missing anchor cannot produce an exact receipt, but the generation-scoped
/// frontier remains durable.
#[allow(clippy::too_many_arguments)]
pub(in crate::services::discord) fn shadow_mirror_delivered_frontier(
    shared: &crate::services::discord::SharedData,
    provider: &ProviderKind,
    channel: ChannelId,
    authoritative_tmux_session_name: Option<&str>,
    range: (u64, u64),
    is_delivered: bool,
    terminal_anchor_msg_id: Option<u64>,
    terminal_anchor_channel_id: Option<u64>,
    delivered_body: Option<&str>,
    ledger_user_msg_id: Option<u64>,
) {
    let _ = shadow_mirror_delivered_frontier_inner(
        shared,
        provider,
        channel,
        authoritative_tmux_session_name,
        range,
        is_delivered,
        terminal_anchor_msg_id,
        terminal_anchor_channel_id,
        delivered_body,
        ledger_user_msg_id,
        None,
        None,
    );
}

/// The caller-PINNED half of the ledger-id safety rule: an id the caller bound to
/// THIS delivery — either the watcher's delivery-lease authority or the explicit
/// `ledger_user_msg_id` argument that every production terminal-delivery call site
/// passes from its own turn/inflight snapshot. Unlike the receipt-qualified
/// fresh-row fallback it needs neither an inflight re-read nor an authoritative
/// JSONL generation, so it is the only ledger-id source usable on the
/// unknown-generation path where no durable frontier is written (#5154).
fn pinned_ledger_user_msg_id(
    watcher_authority: Option<WatcherDeliveryRecordAuthority>,
    ledger_user_msg_id: Option<u64>,
) -> Option<u64> {
    watcher_authority
        .and_then(|authority| authority.ledger_user_msg_id)
        .or(ledger_user_msg_id)
        .filter(|user_msg_id| *user_msg_id != 0)
}

#[allow(clippy::too_many_arguments)]
fn shadow_mirror_delivered_frontier_inner(
    shared: &crate::services::discord::SharedData,
    provider: &ProviderKind,
    channel: ChannelId,
    authoritative_tmux_session_name: Option<&str>,
    range: (u64, u64),
    is_delivered: bool,
    terminal_anchor_msg_id: Option<u64>,
    terminal_anchor_channel_id: Option<u64>,
    delivered_body: Option<&str>,
    ledger_user_msg_id: Option<u64>,
    watcher_authority: Option<WatcherDeliveryRecordAuthority>,
    recovery_authority: Option<RecoveryDeliveryRecordAuthority>,
) -> bool {
    let channel_id = channel.get();
    let coord = shared.tmux_relay_coord(channel);
    let generation_mtime_ns = watcher_authority
        .map(|authority| authority.generation_mtime_ns)
        .or_else(|| recovery_authority.map(|authority| authority.generation_mtime_ns))
        .unwrap_or_else(|| {
            coord
                .confirmed_end_generation_mtime_ns
                .load(Ordering::Acquire)
        });
    if !is_delivered {
        return false;
    }
    if generation_mtime_ns == 0 || range.1 <= range.0 {
        // #5154: the durable frontier is generation-scoped, so without an
        // authoritative JSONL incarnation/range it MUST stay unwritten — that guard
        // is unchanged and just as strict. But "this user message was answered" is
        // NOT a fact about a JSONL incarnation, and the single early return that
        // used to live here also skipped the completed-turn ledger append below.
        // `catch_up/settled_ledger_consult.rs` treats that ledger as the ONLY
        // settled-evidence source, so a confirmed, delivered turn was re-collected
        // by catch-up and its prompt re-submitted verbatim. Settlement recording is
        // therefore separated from frontier persistence.
        //
        // Only CALLER-PINNED ids are used here. The receipt-qualified fresh-row
        // fallback further below is deliberately NOT reproduced: it is qualified by
        // an exact receipt, which cannot be constructed without an authoritative
        // generation, and an unqualified current row could let a delayed A settle a
        // newer B.
        //
        // The #4081 delivered-content fingerprint is deliberately NOT recorded on
        // this path. Its match predicate requires exact generation equality
        // (`recent_content_fingerprint_matches`) against a reader-side FRESH marker
        // read (`recent_delivered_content_matches` → `current_generation_mtime_ns`),
        // so a fingerprint stored under the unknown-generation sentinel `0` becomes
        // permanently unmatchable the moment the generation is re-established, while
        // still consuming a slot in the bounded `recent_delivered_contents` ring. It
        // would look like a duplicate defense without being one. Duplicate defense on
        // this path stays with the ledger; the fingerprint is not restored here.
        let settled_ledger_user_msg_id =
            pinned_ledger_user_msg_id(watcher_authority, ledger_user_msg_id);
        if let Some(user_msg_id) = settled_ledger_user_msg_id {
            completed_turn_ledger::append_completed_turn(
                provider,
                terminal_anchor_channel_id.unwrap_or(channel_id),
                user_msg_id,
            );
        }
        tracing::warn!(
            provider = provider.as_str(),
            channel_id,
            range = ?range,
            generation_mtime_ns,
            ?settled_ledger_user_msg_id,
            "confirmed delivery lacks an authoritative JSONL incarnation/range; preserving prior durable frontier (completed-turn settlement still recorded when the caller pinned an inbound id)"
        );
        return false;
    }
    let in_memory_confirmed_end = coord.confirmed_end_offset.load(Ordering::Acquire);
    let delivery_channel_id = terminal_anchor_channel_id.unwrap_or(channel_id);
    let delivery_channel = ChannelId::new(delivery_channel_id);
    // Capture the delivered turn's nonce/session from its DELIVERY-channel row,
    // never from the offset-authority row. In a channel split the latter can be
    // a different, unanswered turn. This synchronous read+write happens before
    // the caller's inflight clear and closes the clear-before-seed-consume race.
    let fresh =
        crate::services::discord::inflight::load_inflight_state(provider, delivery_channel_id);
    let attempts = recovery_authority.map_or_else(
        || {
            fresh
                .as_ref()
                .map(|f| f.recovery_relay_attempts)
                .unwrap_or(0)
        },
        |authority| authority.attempts,
    );
    // The caller already owns the delivery incarnation while committing its
    // lease. Keep that identity independent of the inflight row: the row may be
    // cleared immediately after the confirmed POST. A fresh exact row remains
    // mandatory for receipt construction below; caller authority only supplies
    // the receipt-less frontier fallback and can never synthesize a receipt.
    let tmux_session_name = authoritative_tmux_session_name
        .filter(|name| !name.is_empty())
        .or_else(|| {
            fresh
                .as_ref()
                .and_then(|state| state.tmux_session_name.as_deref())
                .filter(|name| !name.is_empty())
        });
    let frontier = DeliveredCommit {
        range,
        generation_mtime_ns,
        attempts,
        panel_msg_id: terminal_anchor_msg_id,
        panel_channel_id: terminal_anchor_channel_id,
    };
    let receipt = exact_receipt_from_inflight(
        provider,
        fresh.as_ref(),
        channel,
        delivery_channel,
        terminal_anchor_msg_id,
        range,
        generation_mtime_ns,
    );
    // A ledger id is safe only when the caller pinned it to this delivery or
    // the same fresh row produced the exact receipt above. Never guess from an
    // unqualified current row: a delayed A could otherwise settle newer B.
    let safe_ledger_user_msg_id = pinned_ledger_user_msg_id(watcher_authority, ledger_user_msg_id)
        .or_else(|| {
            receipt
                .as_ref()
                .and_then(|_| fresh.as_ref().map(|state| state.user_msg_id))
                .filter(|user_msg_id| *user_msg_id != 0)
        });
    // Only the exact receipt's own row proves its episode nonce.
    let receipt_row = receipt.as_ref().zip(fresh.as_ref());
    let proven = receipt_row.filter(|(_, r)| Some(r.user_msg_id) == safe_ledger_user_msg_id);
    let nonce = proven.map(|(receipt, _)| receipt.source.turn_nonce.clone());
    let lock_authority = watcher_authority.map(|authority| WatcherFrontierLockAuthority {
        shared,
        channel,
        lease_reset_incarnation: authority.lease_reset_incarnation,
        generation_mtime_ns: authority.generation_mtime_ns,
    });
    // #5071 T1 S7: the ONLY policy that is not `PreserveExisting` is the
    // recovery re-anchor Discord has proved GONE. It is selected by the caller's
    // authority rather than by a separate raw writer, so the equal-range CAS and
    // the monotonic merge now sit on one path through this funnel.
    let equal_range_anchor_policy = recovery_authority
        .and_then(|authority| authority.expected_gone_anchor)
        .map_or(
            EqualRangeAnchorPolicy::PreserveExisting,
            |(expected_panel_channel_id, expected_panel_msg_id)| {
                EqualRangeAnchorPolicy::ReplaceProvenGone {
                    expected_panel_channel_id,
                    expected_panel_msg_id,
                }
            },
        );
    if let Err(error) = persist_confirmed_frontier_and_receipt(
        provider,
        channel,
        tmux_session_name,
        frontier,
        receipt,
        equal_range_anchor_policy,
        lock_authority,
    ) {
        tracing::error!(
            provider = provider.as_str(),
            channel_id,
            error = %error,
            "confirmed delivery frontier/receipt write failed"
        );
        return false;
    }

    // Retry evidence and completed-turn settlement become visible only after
    // the guarded frontier/receipt mutation succeeds.
    if let Some(body) = delivered_body {
        record_delivered_content_fingerprint_for_generation(
            provider,
            channel_id,
            body,
            generation_mtime_ns,
        );
    }
    if let Some(user_msg_id) = safe_ledger_user_msg_id {
        append_completed_episode(provider, delivery_channel_id, user_msg_id, nonce.as_deref());
    }

    // The flag controls only divergence telemetry. The confirmed frontier and
    // exact receipt above are delivery authority and are always persisted.
    if should_shadow_mirror(true, delivery_record_shadow_enabled())
        && delivered_frontier_end_diverged(range.1, in_memory_confirmed_end)
    {
        tracing::error!(
            provider = provider.as_str(),
            channel_id,
            durable_end = range.1,
            in_memory_confirmed_end,
            generation_mtime_ns,
            "#3089 B1: delivered_frontier END diverged from in-memory confirmed_end_offset (observe-only)"
        );
    }
    true
}

pub(in crate::services::discord) fn record_delivered_frontier_with_body(
    shared: &crate::services::discord::SharedData,
    provider: &ProviderKind,
    channel: ChannelId,
    tmux_session_name: Option<&str>,
    range: (u64, u64),
    terminal_anchor_msg_id: u64,
    terminal_anchor_channel_id: u64,
    body: &str,
    ledger_user_msg_id: Option<u64>,
) {
    shadow_mirror_delivered_frontier(
        shared,
        provider,
        channel,
        tmux_session_name,
        range,
        true,
        Some(terminal_anchor_msg_id),
        Some(terminal_anchor_channel_id),
        Some(body),
        ledger_user_msg_id,
    );
}

/// #3610 PR-1c: record the durable terminal anchor for the BRIDGE long-chunk arm
/// (`turn_bridge/mod.rs` site 4 — `send_ordered_long_terminal_response`). PR-1/1b
/// instrumented only the short-replace sites (sink/watcher) and the bridge cutover,
/// so a LONG (`len > DISCORD_MSG_LIMIT`) terminal answer — which routes through the
/// send-new-chunks + placeholder-delete path — recorded NO anchor.
///
/// The caller invokes this ONLY on the FULL-COMMIT `Ok` arm of
/// `send_ordered_long_terminal_response` (the send is all-or-nothing — a partial
/// chunk failure rolls back and returns `Err`, never `Ok`) AND ONLY when
/// `lease.commit_and_advance(.., Delivered)` returned `true` — i.e. the in-memory
/// `confirmed_end_offset` actually advanced. That commit-success gate is the M4
/// invariant: a non-Leased / identity-mismatch / reclaimed cell makes
/// `commit_and_advance` return `false` WITHOUT advancing the offset, and recording
/// `delivered_frontier.range = end` in that case would leave the durable frontier
/// END ahead of `confirmed_end_offset` (M4 violation). With both gates satisfied
/// `is_delivered = true` is correct here (mirrors the cutover's
/// `outcome_is_shadow_delivered` gate, which for this arm is unconditionally
/// `Delivered`).
///
/// Channel split (same as the cutover): the frontier is KEYED by
/// `watcher_owner_channel_id` (the OFFSET-AUTHORITY channel where
/// `confirmed_end_offset` advanced — unchanged), while the recorded anchor PAIR is
/// `(panel_channel_id = delivery_channel_id, panel_msg_id = last_chunk_anchor_msg_id)`
/// — the channel/message the tail chunk actually lives in. These differ for a
/// recovered/reused-watcher bridge. `range` is the SAME `(start, end)` the bridge
/// delivery lease acquired/committed (offset-space consistent — never mix spaces).
/// `last_chunk_anchor_msg_id = None` (empty chunk Vec — impossible on the `Ok`
/// path, but type-honest) records the range with a null anchor, identical to the
/// absent-status-panel case. The delivered frontier is flag-independent; an
/// exact receipt additionally requires a real tail anchor and complete inflight
/// identity. The #4081 recent-content fingerprint remains non-authoritative
/// retry metadata and is never used for restored-seed removal.
pub(in crate::services::discord) fn record_long_chunk_terminal_delivery(
    shared: &crate::services::discord::SharedData,
    provider: &ProviderKind,
    watcher_owner_channel_id: ChannelId,
    delivery_channel_id: ChannelId,
    tmux_session_name: Option<&str>,
    range: (u64, u64),
    last_chunk_anchor_msg_id: Option<u64>,
    delivered_body: &str,
    ledger_user_msg_id: Option<u64>,
) {
    shadow_mirror_delivered_frontier(
        shared,
        provider,
        watcher_owner_channel_id,
        tmux_session_name,
        range,
        true,
        last_chunk_anchor_msg_id,
        Some(delivery_channel_id.get()),
        Some(delivered_body),
        ledger_user_msg_id,
    );
}

/// Watcher-only terminal-delivery funnel. The caller captures generation and turn
/// identity with its delivery lease, while the wrapper holds the matching relay
/// frontier mutation guard through this durable mutation.
pub(in crate::services::discord) fn record_watcher_terminal_delivery(
    shared: &crate::services::discord::SharedData,
    provider: &ProviderKind,
    channel_id: ChannelId,
    tmux_session_name: &str,
    authority: WatcherDeliveryRecordAuthority,
    range: (u64, u64),
    last_chunk_anchor_msg_id: Option<u64>,
    delivered_body: &str,
) -> bool {
    shadow_mirror_delivered_frontier_inner(
        shared,
        provider,
        channel_id,
        Some(tmux_session_name),
        range,
        true,
        last_chunk_anchor_msg_id,
        Some(channel_id.get()),
        Some(delivered_body),
        None,
        Some(authority),
        None,
    )
}

/// Recovery-only terminal-delivery funnel (#5071 T1 S7).
///
/// WHAT THIS REPLACES, EXACTLY. Until S7 the recovery path did not reach this
/// funnel at all: `recovery_engine/terminal_text_idempotency.rs` called
/// `write_delivered_frontier` / `write_proven_gone_equal_range_frontier`
/// directly and appended the completed-turn ledger ITSELF, BEFORE either write.
/// Those three call sites are gone; this is the one that took their place. It is
/// a claim about those three writes, not about "every recovery write" — the
/// dormant `turn_output_controller/fresh_send.rs` sites are untouched by S7.
///
/// WHAT JOINING BUYS, MEASURED RATHER THAN ASSERTED:
///   * the delivered-content fingerprint (#4081) is now recorded for a recovery
///     delivery; before S7 nothing on this path wrote one;
///   * the completed-turn ledger append happens AFTER a successful frontier
///     persist on the recordable path, and on the unknown-generation path it
///     still happens — which is the #4564 guarantee the pre-S7 unconditional
///     leading append existed for, now expressed by the funnel's own rule;
///   * an exact `confirmed_deliveries` receipt is CONSTRUCTED THROUGH THE SAME
///     PREDICATE as every other caller (`exact_receipt_from_inflight`). It is
///     NOT thereby produced: that predicate requires the fresh inflight row's
///     `turn_start_offset` to equal `range.0`, and the recovery caller builds
///     its range as `(state.last_offset, confirmed_end)`. Recovery reaching the
///     receipt-less arm is therefore the expected outcome today, exactly as
///     before S7. Joining moved the decision into the funnel; it did not create
///     a receipt.
///
/// WHAT IT DOES NOT BUY. No `WatcherFrontierLockAuthority`. That authority
/// re-validates `coord.confirmed_end_generation_mtime_ns` against the caller's
/// generation under the record flock, and the recovery path never advances the
/// in-memory watermark that stores it, so supplying one would REFUSE precisely
/// the startup-recovery writes this path exists for. That asymmetry is owned by
/// #5071 T1 S7b together with the in-memory advance itself.
pub(in crate::services::discord) fn record_recovery_terminal_delivery(
    shared: &crate::services::discord::SharedData,
    provider: &ProviderKind,
    offset_authority_channel: ChannelId,
    tmux_session_name: Option<&str>,
    authority: RecoveryDeliveryRecordAuthority,
    delivered_body: &str,
) -> bool {
    let (terminal_anchor_channel_id, terminal_anchor_msg_id) = authority.terminal_anchor;
    shadow_mirror_delivered_frontier_inner(
        shared,
        provider,
        offset_authority_channel,
        tmux_session_name,
        authority.range,
        true,
        Some(terminal_anchor_msg_id),
        Some(terminal_anchor_channel_id),
        Some(delivered_body),
        authority.ledger_user_msg_id,
        None,
        Some(authority),
    )
}

#[cfg(test)]
mod relay_state_contract_refs {
    #[test]
    fn contract_symbols_exist() {
        let _symbol = super::range_committed_after_edit_failure;
    }
}

#[cfg(test)]
mod tests {
    use super::super::delivery_frontier_probe::{
        CurrentGenerationAnchor, current_generation_delivered_anchor_at,
    };
    use super::*;

    fn sample_lease() -> DurableLease {
        DurableLease {
            holder_id: 7,
            attempt_id: 2,
            range: (10, 42),
            deadline_epoch_ms: 1_700_000_000_000,
            holder_generation: 3,
        }
    }

    fn sample_frontier() -> DeliveredCommit {
        DeliveredCommit {
            range: (0, 42),
            generation_mtime_ns: 123_456_789,
            attempts: 1,
            panel_msg_id: Some(999),
            panel_channel_id: Some(1234),
        }
    }

    fn set_phase_a_generation(tmux_session_name: &str, unix_secs: i64) -> i64 {
        let path = crate::services::tmux_common::session_temp_path(tmux_session_name, "generation");
        if let Some(parent) = Path::new(&path).parent() {
            fs::create_dir_all(parent).expect("create generation parent");
        }
        fs::write(&path, "phase-a-r2").expect("write generation marker");
        filetime::set_file_mtime(&path, filetime::FileTime::from_unix_time(unix_secs, 123))
            .expect("set deterministic generation mtime");
        current_generation_mtime_ns(tmux_session_name)
    }

    fn exact_delivery_fixture(
        provider: &ProviderKind,
        tmux_session_name: &str,
        turn_nonce: &str,
        range: (u64, u64),
        generation_mtime_ns: i64,
        channels: (u64, u64),
        message_id: u64,
    ) -> (DeliveredCommit, ConfirmedDeliveryReceipt) {
        let (owner_channel_id, delivery_channel_id) = channels;
        (
            DeliveredCommit {
                range,
                generation_mtime_ns,
                attempts: 1,
                panel_msg_id: Some(message_id),
                panel_channel_id: Some(delivery_channel_id),
            },
            ConfirmedDeliveryReceipt {
                source: ExactJsonlSourceIdentity {
                    provider: provider.as_str().to_string(),
                    tmux_session_name: tmux_session_name.to_string(),
                    turn_nonce: turn_nonce.to_string(),
                    range,
                    generation_mtime_ns,
                    offset_authority_channel_id: owner_channel_id,
                    delivery_channel_id,
                },
                delivery_channel_id,
                message_id,
            },
        )
    }

    #[cfg(not(unix))]
    #[test]
    fn non_unix_generation_is_absent_so_durable_frontier_is_neither_written_nor_trusted() {
        let _root = IsolatedRoot::new();
        let (provider, channel, tmux) = (ProviderKind::Claude, 6_105_001, "AgentDesk-claude-6105");
        let marker = crate::services::tmux_common::session_temp_path(tmux, "generation");
        std::fs::create_dir_all(Path::new(&marker).parent().unwrap()).unwrap();
        std::fs::write(&marker, b"1").unwrap();
        assert_eq!(current_generation_mtime_ns(tmux), 0);

        let commit = DeliveredCommit {
            range: (0, 42),
            generation_mtime_ns: 100,
            attempts: 1,
            panel_msg_id: Some(999),
            panel_channel_id: Some(channel),
        };
        assert!(write_delivered_frontier(&provider, channel, tmux, commit.clone()).is_err());

        let path = record_path_or_err(&provider, channel).unwrap();
        mutate_record_at(&path, |record| record.delivered_frontier = Some(commit)).unwrap();
        assert_eq!(
            delivered_frontier_end_current_generation(
                &provider,
                ChannelId::new(channel),
                tmux,
                Some(100)
            ),
            0
        );
    }

    // .generation marker 는 Unix 전용 tmux wrapper 만 쓰고 non-unix 의 generation 은 0(불신)이라 generation 에 묶인 durable frontier 경로가 Windows 에는 없다.
    #[cfg(unix)]
    #[test]
    fn codex_admitted_range_receipt_is_exact_and_idempotent_5264() {
        let _root = IsolatedRoot::new();
        let (provider, delivery, owner) = (ProviderKind::Codex, 5_264_001, 5_264_009);
        let tmux = "AgentDesk-codex-5264-pinned";
        let g1 = set_phase_a_generation(tmux, 1_700_526_400);
        #[rustfmt::skip]
        let mut state = crate::services::discord::inflight::InflightTurnState::new(
            provider.clone(), delivery, None, 1, 5_264_003, 5_264_002, "answer".into(),
            None, Some(tmux.into()), None, None, 0);
        (state.last_offset, state.turn_nonce) = (3, Some("turn-nonce".into()));
        #[rustfmt::skip]
        let receipt = |state: &crate::services::discord::inflight::InflightTurnState| exact_receipt_from_inflight(
            &provider, Some(state), ChannelId::new(delivery), ChannelId::new(delivery),
            Some(5_264_002), (0, 3), g1);
        let exact = receipt(&state).expect("exact current receipt");
        state.turn_start_offset = Some(1);
        assert!(receipt(&state).is_none(), "mismatched start is not exact");
        #[rustfmt::skip]
        let (frontier, _) = exact_delivery_fixture(&provider, tmux, "turn-nonce", (0, 3),
            g1, (delivery, delivery), 5_264_002);
        for _ in 0..2 {
            write_confirmed_delivery(&provider, delivery, frontier.clone(), exact.clone()).unwrap();
        }
        assert_eq!(
            read_record(&provider, delivery)
                .unwrap()
                .confirmed_deliveries,
            vec![exact]
        );
        #[rustfmt::skip]
        let (_, historical) = exact_delivery_fixture(&provider, tmux, "old-turn", (3, 6),
            g1, (owner, delivery), 5_264_006);
        let g2 = set_phase_a_generation(tmux, 1_800_526_400);
        #[rustfmt::skip]
        let (current, current_receipt) = exact_delivery_fixture(&provider, tmux, "new-turn", (0, 7),
            g2, (owner, delivery), 5_264_007);
        write_confirmed_delivery(&provider, owner, current.clone(), current_receipt).unwrap();
        record_historical_pinned_delivery(&historical.source, historical.message_id).unwrap();
        let record = read_record(&provider, owner).unwrap();
        assert_eq!(
            record.delivered_frontier,
            Some(current),
            "G2 frontier is preserved"
        );
        assert!(historical_pinned_delivery_exists(
            &historical.source,
            historical.message_id
        ));
        let source = &historical.source;
        let wrong = ExactJsonlSourceIdentity {
            generation_mtime_ns: g2,
            ..source.clone()
        };
        assert!(!historical_pinned_delivery_exists(
            &wrong,
            historical.message_id
        ));
        std::fs::remove_file(crate::services::tmux_common::session_temp_path(
            tmux,
            "generation",
        ))
        .unwrap();
        assert!(historical_pinned_delivery_exists(
            source,
            historical.message_id
        ));
    }

    /// #5264 PR-B: the two Discord-identity guards at the top of
    /// `confirmed_delivery_receipt_exists` now stand IN FRONT of the
    /// `historical_pinned_delivery_exists` fast path, and that fast path keys
    /// itself entirely off the caller-supplied `source` — it never looks at the
    /// `provider`/`delivery_channel` the caller is actually asking about. So
    /// deleting either guard makes the function answer "yes, this exact receipt
    /// is confirmed" for a receipt belonging to a DIFFERENT provider or a
    /// DIFFERENT destination channel, which is precisely the restored-seed
    /// consumption decision the doc comment promises fails closed. Both
    /// deletions were silent against the whole suite before this test.
    #[cfg(unix)]
    #[test]
    fn confirmed_receipt_identity_guards_gate_the_historical_fast_path_5264() {
        let _root = IsolatedRoot::new();
        let (provider, owner, delivery) = (ProviderKind::Codex, 5_264_201, 5_264_202);
        let tmux = "AgentDesk-codex-5264-receipt-guard";
        let generation = set_phase_a_generation(tmux, 1_700_526_402);
        #[rustfmt::skip]
        let (_, historical) = exact_delivery_fixture(&provider, tmux, "guard-turn", (0, 9),
            generation, (owner, delivery), 5_264_203);
        record_historical_pinned_delivery(&historical.source, historical.message_id)
            .expect("seed the exact historical receipt");
        let source = &historical.source;
        assert!(
            confirmed_delivery_receipt_exists(
                &provider,
                ChannelId::new(delivery),
                historical.message_id,
                source,
            ),
            "the matching query must still resolve, or the negatives below are vacuous"
        );
        assert!(
            !confirmed_delivery_receipt_exists(
                &ProviderKind::Claude,
                ChannelId::new(delivery),
                historical.message_id,
                source,
            ),
            "a receipt recorded for codex must not confirm a claude consumption"
        );
        assert!(
            !confirmed_delivery_receipt_exists(
                &provider,
                ChannelId::new(owner),
                historical.message_id,
                source,
            ),
            "a receipt whose destination was the delivery channel must not confirm a \
             consumption asking about the offset-authority channel"
        );
        assert!(
            !confirmed_delivery_receipt_exists(&provider, ChannelId::new(delivery), 0, source,),
            "message id 0 is not a delivery"
        );
    }
    #[cfg(unix)]
    #[test]
    fn ordered_jsonl_commit_is_generation_scoped_and_monotonic() {
        let _root = IsolatedRoot::new();
        let path = record_path_or_err(&ProviderKind::Claude, 2_115).expect("record path");
        let tmux = "AgentDesk-claude-ordered-monotonic";
        let generation_1 = set_phase_a_generation(tmux, 1_700_002_115);

        assert!(commit_ordered_jsonl_range_at(&path, tmux, (100, 200), generation_1).unwrap());
        assert!(commit_ordered_jsonl_range_at(&path, tmux, (150, 180), generation_1).unwrap());
        assert_eq!(
            read_record_at(&path).unwrap().delivered_frontier.unwrap(),
            DeliveredCommit {
                range: (100, 200),
                generation_mtime_ns: generation_1,
                attempts: 1,
                panel_msg_id: None,
                panel_channel_id: None,
            }
        );

        let generation_2 = set_phase_a_generation(tmux, 1_700_002_116);
        assert_ne!(generation_1, generation_2);
        assert!(commit_ordered_jsonl_range_at(&path, tmux, (0, 50), generation_2).unwrap());
        assert_eq!(
            read_record_at(&path)
                .unwrap()
                .delivered_frontier
                .unwrap()
                .range,
            (0, 50),
            "a replacement wrapper starts a new coordinate space"
        );
        assert!(!commit_ordered_jsonl_range_at(&path, tmux, (50, 50), generation_2).unwrap());
        assert!(!commit_ordered_jsonl_range_at(&path, tmux, (50, 60), 0).unwrap());
    }

    #[test]
    fn delivery_record_serde_roundtrip() {
        let record = DeliveryRecord {
            delivery_lease: Some(sample_lease()),
            delivered_frontier: Some(sample_frontier()),
            recent_delivered_contents: Vec::new(),
            confirmed_deliveries: Vec::new(),
        };
        let json = serde_json::to_string_pretty(&record).unwrap();
        let back: DeliveryRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(record, back);
    }

    #[test]
    fn empty_object_deserializes_to_none_none() {
        // #[serde(default)] lets an old/partial `{}` parse to both-None.
        let back: DeliveryRecord = serde_json::from_str("{}").unwrap();
        assert_eq!(back, DeliveryRecord::default());
        assert!(back.delivery_lease.is_none() && back.delivered_frontier.is_none());
    }

    #[test]
    fn atomic_write_then_read_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = delivery_record_path_in_root(dir.path(), &ProviderKind::Claude, 444);
        let record = DeliveryRecord {
            delivery_lease: Some(sample_lease()),
            delivered_frontier: Some(sample_frontier()),
            recent_delivered_contents: Vec::new(),
            confirmed_deliveries: Vec::new(),
        };
        write_record_at(&path, &record).unwrap();
        assert_eq!(read_record_at(&path), Some(record));
    }

    #[test]
    fn degenerate_content_guard_matches_byte_identical_recent_delivery_4081() {
        let dir = tempfile::tempdir().unwrap();
        let path = delivery_record_path_in_root(dir.path(), &ProviderKind::Claude, 4081);
        let generation = 44_081_i64;
        let now = 1_700_000_000_000_u64;

        record_delivered_content_fingerprint_at(&path, 4081, "prior body", generation, now)
            .unwrap();

        assert!(recent_delivered_content_matches_at(
            &path,
            4081,
            "prior body",
            generation,
            now + 1,
        ));
        assert!(!recent_delivered_content_matches_at(
            &path,
            4081,
            "different body",
            generation,
            now + 1,
        ));
        assert!(!recent_delivered_content_matches_at(
            &path,
            4082,
            "prior body",
            generation,
            now + 1,
        ));
        assert!(!recent_delivered_content_matches_at(
            &path,
            4081,
            "prior body",
            generation + 1,
            now + 1,
        ));
    }

    #[test]
    fn recent_delivered_content_ring_is_bounded_4081() {
        let dir = tempfile::tempdir().unwrap();
        let path = delivery_record_path_in_root(dir.path(), &ProviderKind::Claude, 4082);
        let generation = 44_082_i64;
        let now = 1_700_000_000_000_u64;

        for idx in 0..(RECENT_DELIVERED_CONTENT_LIMIT + 4) {
            record_delivered_content_fingerprint_at(
                &path,
                4082,
                &format!("body-{idx}"),
                generation,
                now + idx as u64,
            )
            .unwrap();
        }

        let record = read_record_at(&path).unwrap();
        assert_eq!(
            record.recent_delivered_contents.len(),
            RECENT_DELIVERED_CONTENT_LIMIT
        );
        assert!(!recent_delivered_content_matches_at(
            &path,
            4082,
            "body-0",
            generation,
            now + 100,
        ));
        assert!(recent_delivered_content_matches_at(
            &path,
            4082,
            "body-19",
            generation,
            now + 100,
        ));
    }

    #[test]
    fn recent_delivered_content_window_expires_4081() {
        let dir = tempfile::tempdir().unwrap();
        let path = delivery_record_path_in_root(dir.path(), &ProviderKind::Claude, 4083);
        let generation = 44_083_i64;
        let now = 1_700_000_000_000_u64;

        record_delivered_content_fingerprint_at(&path, 4083, "prior body", generation, now)
            .unwrap();

        assert!(!recent_delivered_content_matches_at(
            &path,
            4083,
            "prior body",
            generation,
            now + RECENT_DELIVERED_CONTENT_WINDOW_MS + 1,
        ));
    }

    #[test]
    fn missing_record_reads_none_conservative() {
        // I3: an absent file is not-yet-delivered, not an error/assume-delivered.
        let dir = tempfile::tempdir().unwrap();
        let path = delivery_record_path_in_root(dir.path(), &ProviderKind::Codex, 5);
        assert_eq!(read_record_at(&path), None);
    }

    #[test]
    fn malformed_record_reads_none_conservative() {
        // I3: a torn/garbage record reads as None (no panic, no Err).
        let dir = tempfile::tempdir().unwrap();
        let path = delivery_record_path_in_root(dir.path(), &ProviderKind::Claude, 6);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "{ not valid json").unwrap();
        assert_eq!(read_record_at(&path), None);
    }

    #[test]
    fn clear_lease_preserves_frontier() {
        // §4.3: release clears the lease ONLY; the frontier survives. Drives the
        // PRODUCTION `clear_lease_at` (not `mutate_record_at`) so a mutation that
        // also cleared the frontier here is caught.
        let dir = tempfile::tempdir().unwrap();
        let path = delivery_record_path_in_root(dir.path(), &ProviderKind::Claude, 7);
        write_delivered_frontier_at(&path, sample_frontier()).unwrap();
        upsert_lease_at(&path, sample_lease()).unwrap();
        clear_lease_at(&path).unwrap();
        let after = read_record_at(&path).unwrap();
        assert!(after.delivery_lease.is_none());
        assert_eq!(after.delivered_frontier, Some(sample_frontier()));
    }

    #[test]
    fn lease_upsert_preserves_existing_frontier() {
        // I2: acquiring a lease after a frontier exists never advances/clears it.
        // Drives the production `write_delivered_frontier_at` / `upsert_lease_at`.
        let dir = tempfile::tempdir().unwrap();
        let path = delivery_record_path_in_root(dir.path(), &ProviderKind::Claude, 8);
        write_delivered_frontier_at(&path, sample_frontier()).unwrap();
        upsert_lease_at(&path, sample_lease()).unwrap();
        let after = read_record_at(&path).unwrap();
        assert_eq!(after.delivery_lease, Some(sample_lease()));
        assert_eq!(after.delivered_frontier, Some(sample_frontier()));
    }

    #[test]
    fn watcher_owner_context_preserves_frontier_and_resolves_current_generation_3751() {
        let dir = tempfile::tempdir().unwrap();
        let record_path = delivery_record_path_in_root(dir.path(), &ProviderKind::Claude, 123);
        let context_path =
            delivery_owner_context_path_in_root(dir.path(), &ProviderKind::Claude, 123);
        write_delivered_frontier_at(&record_path, sample_frontier()).unwrap();
        write_watcher_owner_context_at(&context_path, 456, "AgentDesk-claude-foo", 700).unwrap();

        let after = read_record_at(&record_path).unwrap();
        assert_eq!(after.delivered_frontier, Some(sample_frontier()));
        assert_eq!(
            watcher_owner_channel_from_context_at(&context_path, "AgentDesk-claude-foo", 700),
            Some(456)
        );
    }

    #[test]
    fn watcher_owner_context_rejects_stale_or_wrong_session_3751() {
        let dir = tempfile::tempdir().unwrap();
        let path = delivery_owner_context_path_in_root(dir.path(), &ProviderKind::Claude, 123);
        write_watcher_owner_context_at(&path, 456, "AgentDesk-claude-foo", 700).unwrap();

        assert_eq!(
            watcher_owner_channel_from_context_at(&path, "AgentDesk-claude-bar", 700),
            None
        );
        assert_eq!(
            watcher_owner_channel_from_context_at(&path, "AgentDesk-claude-foo", 701),
            None
        );
    }

    #[test]
    fn watcher_owner_context_survives_delivery_record_rewrite_3751() {
        let dir = tempfile::tempdir().unwrap();
        let record_path = delivery_record_path_in_root(dir.path(), &ProviderKind::Claude, 123);
        let context_path =
            delivery_owner_context_path_in_root(dir.path(), &ProviderKind::Claude, 123);
        write_watcher_owner_context_at(&context_path, 456, "AgentDesk-claude-foo", 700).unwrap();

        // Simulates an old binary that knows only the lease/frontier record shape:
        // it rewrites the delivery record JSON object, but never opens the separate
        // owner-context subtree.
        write_delivered_frontier_at(&record_path, sample_frontier()).unwrap();
        upsert_lease_at(&record_path, sample_lease()).unwrap();
        clear_lease_at(&record_path).unwrap();

        assert_eq!(
            watcher_owner_channel_from_context_at(&context_path, "AgentDesk-claude-foo", 700),
            Some(456)
        );
    }

    #[test]
    fn only_frontier_writer_advances_frontier() {
        // I2: a lease-only sequence (acquire→release) never produces a frontier.
        // Drives the production `upsert_lease_at` / `clear_lease_at`.
        let dir = tempfile::tempdir().unwrap();
        let path = delivery_record_path_in_root(dir.path(), &ProviderKind::Claude, 9);
        upsert_lease_at(&path, sample_lease()).unwrap();
        clear_lease_at(&path).unwrap();
        let after = read_record_at(&path).unwrap();
        assert!(after.delivered_frontier.is_none());
    }

    #[test]
    fn delete_record_removes_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = delivery_record_path_in_root(dir.path(), &ProviderKind::Claude, 10);
        write_record_at(&path, &DeliveryRecord::default()).unwrap();
        assert!(path.exists());
        assert!(delete_record_at(&path)); // production GC helper
        assert!(!path.exists());
    }

    /// THE B0 deliverable: the sidecar path is isolated from the old-binary
    /// reaper by DIRECTORY, not by extension. The reaper does a non-recursive
    /// `read_dir(runtime/discord_inflight/<provider>/)` and reaps `*.json`
    /// there; our record lives under a sibling `discord_delivery_records/`
    /// subtree it never descends into.
    #[test]
    fn sidecar_path_is_outside_inflight_scan_set() {
        let runtime_root = Path::new("/tmp/adk-test-runtime");
        let provider = ProviderKind::Claude;
        let record = delivery_record_path_in_root(runtime_root, &provider, 444);
        let owner_context = delivery_owner_context_path_in_root(runtime_root, &provider, 444);

        // The reaper's scan dir (it builds this exact path; we reconstruct it).
        let inflight_provider_dir = runtime_root
            .join("discord_inflight")
            .join(provider.as_str());

        let record_dir = record.parent().unwrap();

        // 1. The record is NOT inside the scanned inflight provider dir.
        assert_ne!(record_dir, inflight_provider_dir.as_path());
        // 2. And not anywhere beneath the inflight subtree (read_dir is
        //    non-recursive, but assert the stronger property anyway).
        assert!(!record.starts_with(runtime_root.join("discord_inflight")));
        // 3. The isolating segment directly under runtime/ is the dedicated
        //    sidecar dir, not the reaper's target.
        assert!(record.starts_with(runtime_root.join(DELIVERY_RECORDS_DIR)));
        assert!(owner_context.starts_with(runtime_root.join(DELIVERY_OWNER_CONTEXT_DIR)));
        assert_ne!(record.parent(), owner_context.parent());
        assert_ne!(DELIVERY_RECORDS_DIR, "discord_inflight");
        assert_ne!(DELIVERY_OWNER_CONTEXT_DIR, "discord_inflight");
        assert_ne!(DELIVERY_OWNER_CONTEXT_DIR, DELIVERY_RECORDS_DIR);
        // #4046 S1r-1 P3-1: pin the fresh-send sidecar dir out of the reaper's scan set
        // too. The reaper scans `discord_inflight`; a regression that renamed
        // `FRESH_SEND_RECORDS_DIR` to `"discord_inflight"` (or collided it with the
        // delivery-records dir) would make the reaper reap live fresh-send records, and
        // fresh_send_tests' behavioral isolation does NOT cover the dir-name identity.
        // This const pin does.
        assert_ne!(FRESH_SEND_RECORDS_DIR, "discord_inflight");
        assert_ne!(FRESH_SEND_RECORDS_DIR, DELIVERY_RECORDS_DIR);
    }

    // ---- #3089 B1 shadow-write ----------------------------------------------

    #[test]
    fn outcome_is_shadow_delivered_only_for_delivered() {
        // I2: ONLY a confirmed Delivered shadow-writes; every non-advancing
        // outcome (NotDelivered/Transient/Unknown/Skipped) is false. Pins the
        // sink's frozen-file outcome decision (broadening it to NotDelivered now
        // fails here).
        assert!(outcome_is_shadow_delivered(&DeliveryOutcome::Delivered {
            committed_to: 5,
            replace_kind: None,
            new_chunks: None,
        }));
        assert!(!outcome_is_shadow_delivered(
            &DeliveryOutcome::FreshDelivered {
                committed_to: Some(5),
                persistence_recorded: false,
            }
        ));
        assert!(!outcome_is_shadow_delivered(
            &DeliveryOutcome::NotDelivered { committed_from: 5 }
        ));
        assert!(!outcome_is_shadow_delivered(&DeliveryOutcome::Transient {
            retry_from_offset: 0
        }));
        assert!(!outcome_is_shadow_delivered(&DeliveryOutcome::Unknown {
            fell_back: false
        }));
        assert!(!outcome_is_shadow_delivered(&DeliveryOutcome::Skipped));
    }

    #[test]
    fn should_shadow_mirror_requires_delivered_and_enabled() {
        // The legacy flag gates only divergence telemetry. Confirmed authority
        // persistence is pinned independently by the Phase A test below.
        assert!(should_shadow_mirror(true, true));
        assert!(!should_shadow_mirror(false, true));
        assert!(!should_shadow_mirror(true, false));
        assert!(!should_shadow_mirror(false, false));
    }

    #[test]
    fn authority_blocks_backward_inflight_write_truth_table() {
        // #3416 (#3089 B3): ENFORCE only when authority is ON AND a same-turn
        // offset moved backward. OFF → never blocks (observe-only, no-op deploy).
        // authority OFF → false regardless of the monotonic flags. The 4th arg
        // (#3933 is_legitimate_full_reset) is `false` for every genuine-regression
        // row here — it only ever RELAXES, never tightens.
        assert!(!authority_blocks_backward_inflight_write(
            false, false, false, false
        ));
        assert!(!authority_blocks_backward_inflight_write(
            false, true, true, false
        ));
        // authority ON, both monotonic OK → permit the write.
        assert!(!authority_blocks_backward_inflight_write(
            true, true, true, false
        ));
        // authority ON, a backward move on EITHER offset → block (pins the OR).
        assert!(authority_blocks_backward_inflight_write(
            true, false, true, false
        ));
        assert!(authority_blocks_backward_inflight_write(
            true, true, false, false
        ));
        assert!(authority_blocks_backward_inflight_write(
            true, false, false, false
        ));

        // #3933: a legitimate full reset (empty body + rso→0, same turn) must be
        // PERMITTED even under authority-ON with a backward move — otherwise the
        // Gemini/Qwen retry re-stream is dropped. The carve-out only fires for a
        // backward move; a fully-monotonic forward write is unaffected either way.
        assert!(!authority_blocks_backward_inflight_write(
            true, false, true, true
        ));
        assert!(!authority_blocks_backward_inflight_write(
            true, true, false, true
        ));
        assert!(!authority_blocks_backward_inflight_write(
            true, false, false, true
        ));
        // The reset flag never turns a permitted write into a block.
        assert!(!authority_blocks_backward_inflight_write(
            true, true, true, true
        ));
        // Authority OFF stays a no-op regardless of the reset flag.
        assert!(!authority_blocks_backward_inflight_write(
            false, false, false, true
        ));
    }

    #[test]
    fn delivery_record_rollout_health_reports_off_as_observable_warning() {
        let json = delivery_record_rollout_health_json_for_flags(
            FlagResolution::compiled_default(false),
            FlagResolution::compiled_default(false),
        );
        assert_eq!(json["mode"], "off");
        assert_eq!(json["shadow_enabled"], false);
        assert_eq!(json["authority_enabled"], false);
        assert_eq!(json["dedup_authority"], "in_memory_committed_offset");
        assert_eq!(json["same_turn_backward_write_enforcement"], "observe_only");
        assert_eq!(json["warning_count"], 1);
        assert!(
            json["configuration_warnings"]
                .as_array()
                .unwrap()
                .iter()
                .any(|warning| warning
                    .as_str()
                    .unwrap()
                    .starts_with("delivery_record_authority_disabled"))
        );
    }

    #[test]
    fn delivery_record_rollout_unpinned_defaults_report_shadow_warning() {
        let json = delivery_record_rollout_health_json_for_flags(
            resolve_shadow_flag(None),
            resolve_authority_flag(None),
        );

        assert_eq!(json["shadow_enabled"], false);
        assert_eq!(json["authority_enabled"], true);
        assert_eq!(json["mode"], "authority_only");
        assert_eq!(json["dedup_authority"], "durable_delivery_record_frontier");
        assert_eq!(json["same_turn_backward_write_enforcement"], "enforcing");
        assert_eq!(json["flag_source"]["shadow"], "compiled_default");
        assert_eq!(json["flag_source"]["authority"], "compiled_default");
        assert_eq!(json["warning_count"], 1);

        let warnings = json["configuration_warnings"].as_array().unwrap();
        assert_eq!(warnings.len(), 1);
        let only_warning = warnings[0].as_str().unwrap();
        assert!(only_warning.starts_with("delivery_record_shadow_disabled"));
        assert!(!only_warning.starts_with("delivery_record_authority_disabled"));
    }

    #[test]
    fn delivery_record_rollout_health_reports_enforcing_modes() {
        let shadow_only = delivery_record_rollout_health_json_for_flags(
            FlagResolution::compiled_default(true),
            FlagResolution::compiled_default(false),
        );
        assert_eq!(shadow_only["mode"], "shadow_only");
        assert_eq!(shadow_only["warning_count"], 1);

        let authority_only = delivery_record_rollout_health_json_for_flags(
            FlagResolution::compiled_default(false),
            FlagResolution::compiled_default(true),
        );
        assert_eq!(authority_only["mode"], "authority_only");
        assert_eq!(
            authority_only["same_turn_backward_write_enforcement"],
            "enforcing"
        );
        assert_eq!(authority_only["warning_count"], 1);

        let enforcing = delivery_record_rollout_health_json_for_flags(
            FlagResolution::compiled_default(true),
            FlagResolution::compiled_default(true),
        );
        assert_eq!(enforcing["mode"], "shadow_and_authority");
        assert_eq!(
            enforcing["dedup_authority"],
            "durable_delivery_record_frontier"
        );
        assert_eq!(
            enforcing["same_turn_backward_write_enforcement"],
            "enforcing"
        );
        assert_eq!(enforcing["warning_count"], 0);
    }

    #[test]
    fn rollout_flag_resolvers_separate_provenance_from_value() {
        // #5071 T1 S8-1r2 gate (a): absence and an explicit off are the SAME
        // value and DIFFERENT provenance. #5071 T1 S8-2 flipped the authority
        // compiled default ON, so absence and a stale `=1` pin now agree on
        // `enabled` and provenance is the only thing that can still name the pin —
        // which is why gate (a) had to exist before this flip.
        assert_eq!(
            resolve_authority_flag(None),
            FlagResolution::compiled_default(true)
        );
        assert_eq!(
            resolve_authority_flag(Some("1")),
            FlagResolution::env_override(true)
        );
        // The rollback direction: an explicit `=0` still wins over the ON default.
        assert_eq!(
            resolve_authority_flag(Some("0")),
            FlagResolution::env_override(false)
        );

        // The two axes now diverge on absence ONLY: shadow keeps its compiled
        // default OFF, and its present-value parsing is unchanged.
        assert_eq!(
            resolve_shadow_flag(None),
            FlagResolution::compiled_default(false)
        );
        assert_eq!(
            resolve_shadow_flag(Some("1")),
            FlagResolution::env_override(true)
        );
        assert_eq!(
            resolve_shadow_flag(Some("0")),
            FlagResolution::env_override(false)
        );

        // Value parsing is unchanged from the pre-extraction inline reads: trimmed,
        // case-insensitive, and ON only for `1`/`true`.
        assert_eq!(
            resolve_authority_flag(Some(" TRUE ")),
            FlagResolution::env_override(true)
        );
        assert_eq!(
            resolve_authority_flag(Some(" 1 ")),
            FlagResolution::env_override(true)
        );
        // Every other present value is off BUT still an override, including the
        // empty string that an exported-but-blank `launchd.env` line produces.
        // Against the S8-2 ON default this is load-bearing in a way it was not
        // before: such a line is now a silent ROLLBACK, not a no-op, and
        // `flag_source: env_override` is the only signal that names it.
        for raw in ["", "  ", "yes", "false", "2"] {
            assert_eq!(
                resolve_authority_flag(Some(raw)),
                FlagResolution::env_override(false),
                "present value {raw:?} must stay an override"
            );
        }
    }

    #[test]
    fn delivery_record_rollout_health_reports_flag_source_across_truth_table() {
        // #5071 T1 S8-1r2 gate (b): provenance is emitted per axis, independently
        // of the mode the two values combine into.
        for (shadow_enabled, authority_enabled) in
            [(false, false), (true, false), (false, true), (true, true)]
        {
            let compiled = delivery_record_rollout_health_json_for_flags(
                FlagResolution::compiled_default(shadow_enabled),
                FlagResolution::compiled_default(authority_enabled),
            );
            assert_eq!(
                compiled["flag_source"],
                serde_json::json!({"shadow": "compiled_default", "authority": "compiled_default"}),
                "compiled-branch row ({shadow_enabled}, {authority_enabled})"
            );

            let pinned = delivery_record_rollout_health_json_for_flags(
                FlagResolution::env_override(shadow_enabled),
                FlagResolution::env_override(authority_enabled),
            );
            assert_eq!(
                pinned["flag_source"],
                serde_json::json!({"shadow": "env_override", "authority": "env_override"}),
                "pinned row ({shadow_enabled}, {authority_enabled})"
            );

            // The two axes are reported independently, which is what lets health
            // name mac-book's half-recovered state after only one pin is dropped.
            let mixed = delivery_record_rollout_health_json_for_flags(
                FlagResolution::env_override(shadow_enabled),
                FlagResolution::compiled_default(authority_enabled),
            );
            assert_eq!(
                mixed["flag_source"],
                serde_json::json!({"shadow": "env_override", "authority": "compiled_default"}),
                "mixed row ({shadow_enabled}, {authority_enabled})"
            );

            // Provenance never perturbs the five rollout facts it accompanies.
            assert_eq!(compiled["mode"], pinned["mode"]);
            assert_eq!(compiled["dedup_authority"], pinned["dedup_authority"]);
            assert_eq!(
                compiled["same_turn_backward_write_enforcement"],
                pinned["same_turn_backward_write_enforcement"]
            );
            assert_eq!(compiled["warning_count"], pinned["warning_count"]);
        }
    }

    #[test]
    fn delivered_frontier_end_divergence_predicate() {
        // M4: END must equal the in-memory authority.
        assert!(!delivered_frontier_end_diverged(42, 42));
        assert!(delivered_frontier_end_diverged(42, 41));
        assert!(delivered_frontier_end_diverged(0, 7));
    }

    #[test]
    fn shadow_writes_frontier_and_reports_match() {
        // flag-ON Delivered path: writes the exact frontier and reports no
        // divergence when the durable END equals the in-memory authority.
        let dir = tempfile::tempdir().unwrap();
        let path = delivery_record_path_in_root(dir.path(), &ProviderKind::Claude, 11);
        let diverged =
            record_delivered_frontier_shadow_at(&path, (3, 10), 111, 2, Some(9), Some(8), 10)
                .unwrap();
        assert!(!diverged);
        let written = read_record_at(&path).unwrap().delivered_frontier.unwrap();
        assert_eq!(
            written,
            DeliveredCommit {
                range: (3, 10),
                generation_mtime_ns: 111,
                attempts: 2,
                panel_msg_id: Some(9),
                panel_channel_id: Some(8),
            }
        );
    }

    #[test]
    fn replace_mirror_gate_records_only_delivered_committed_3630() {
        let dir = tempfile::tempdir().unwrap();
        let delivered_path =
            delivery_record_path_in_root(dir.path(), &ProviderKind::Claude, 36_300);
        let not_delivered_path =
            delivery_record_path_in_root(dir.path(), &ProviderKind::Claude, 36_301);

        let replace_committed = true;
        let committed = true;
        if replace_committed && committed {
            record_delivered_frontier_shadow_at(
                &delivered_path,
                (7, 77),
                123,
                0,
                Some(555),
                Some(999),
                77,
            )
            .unwrap();
        }
        assert_eq!(
            read_record_at(&delivered_path)
                .unwrap()
                .delivered_frontier
                .unwrap()
                .range,
            (7, 77)
        );

        // `commit_and_advance(.., NotDelivered)` can still return true, but the
        // in-memory confirmed_end_offset did not advance, so M4 requires no record.
        let replace_committed = false;
        let committed = true;
        if replace_committed && committed {
            record_delivered_frontier_shadow_at(
                &not_delivered_path,
                (7, 77),
                123,
                0,
                Some(555),
                Some(999),
                0,
            )
            .unwrap();
        }
        assert!(read_record_at(&not_delivered_path).is_none());
    }

    #[test]
    fn shadow_reports_divergence_but_still_writes_and_never_panics() {
        // A durable-vs-memory END mismatch is observe-only: it reports `true`
        // (logged upstream) and STILL writes the frontier — no panic.
        let dir = tempfile::tempdir().unwrap();
        let path = delivery_record_path_in_root(dir.path(), &ProviderKind::Claude, 12);
        let diverged =
            record_delivered_frontier_shadow_at(&path, (3, 10), 222, 0, None, None, 9).unwrap();
        assert!(diverged); // 10 (durable end) != 9 (in-memory)
        assert_eq!(
            read_record_at(&path)
                .unwrap()
                .delivered_frontier
                .unwrap()
                .range,
            (3, 10)
        );
    }

    #[test]
    fn shadow_write_preserves_existing_lease() {
        // B1 shadow-writes ONLY the frontier (I2): a pre-existing lease survives.
        let dir = tempfile::tempdir().unwrap();
        let path = delivery_record_path_in_root(dir.path(), &ProviderKind::Claude, 13);
        upsert_lease_at(&path, sample_lease()).unwrap();
        record_delivered_frontier_shadow_at(&path, (0, 5), 5, 0, None, None, 5).unwrap();
        let after = read_record_at(&path).unwrap();
        assert_eq!(after.delivery_lease, Some(sample_lease()));
        assert_eq!(after.delivered_frontier.unwrap().range, (0, 5));
    }

    // ---- #3089 B2b authority flip (fusion) ----------------------------------

    #[test]
    fn fuse_committed_offset_conservative_max() {
        // The durable frontier can only RAISE the dedup floor above in-memory,
        // never lower it. A missed durable write (None, I3) → pure in-memory =
        // today's behavior. Mutation target: flipping max→min or dropping the
        // in-memory fusion would surface here.
        assert_eq!(fuse_committed_offset(None, 20), 20); // I3: missing → in-memory
        assert_eq!(fuse_committed_offset(Some(30), 20), 30); // durable raises floor
        assert_eq!(fuse_committed_offset(Some(10), 20), 20); // stale-low durable never lowers
        assert_eq!(fuse_committed_offset(Some(0), 0), 0);
    }

    #[test]
    fn generation_guard_distrusts_prior_and_unknown_generations() {
        // #1270: trust the durable frontier ONLY if its generation_mtime_ns equals
        // the CURRENT wrapper generation. A prior-generation (stale-high) frontier
        // must be distrusted → not fused → no false skip of fresh output.
        assert!(durable_frontier_generation_current(123, 123)); // same generation → trust
        assert!(!durable_frontier_generation_current(100, 123)); // prior gen → distrust
        assert!(!durable_frontier_generation_current(123, 0)); // no .generation file → distrust
        assert!(!durable_frontier_generation_current(0, 0)); // both unknown → distrust
    }

    // ---- #3593 JSONL-space monotonic resend dedup (range_already_committed) ----

    #[test]
    fn range_already_committed_suppresses_synthetic_resume_dup_3593() {
        // The observed #3593 case: a synthetic-resume pass tries to re-relay the
        // prior turn's body whose consumed JSONL range ends at 422855, but the
        // committed/delivered JSONL offset has already advanced to 443154. The
        // range is entirely below the committed floor → already delivered → suppress
        // (no duplicate).
        assert!(range_already_committed(422_855, 443_154));
    }

    #[test]
    fn range_already_committed_allows_new_response_after_resume() {
        // Over-suppression no-regression: a genuinely-NEW response produced AFTER the
        // resume ends past the committed floor (range_end 443200 > committed 443154)
        // → NOT already delivered → must be relayed.
        assert!(!range_already_committed(443_200, 443_154));
    }

    #[test]
    fn range_already_committed_boundary_equal_is_committed() {
        // The boundary: range_end == committed means the committed floor already
        // covers this exact range end → already delivered → suppress (inclusive `<=`,
        // matching the `already_relayed` consumers' `committed >= range_end`).
        assert!(range_already_committed(443_154, 443_154));
    }

    #[test]
    fn range_already_committed_zero_range_never_suppressed() {
        // Empty/zero consumed range (range_end == 0) must NEVER be treated as already
        // committed regardless of the committed value — suppressing it could drop a
        // message; message-loss prevention dominates duplicate-suppression.
        assert!(!range_already_committed(0, 443_154));
        assert!(!range_already_committed(0, 0));
    }

    #[test]
    fn range_already_committed_reset_committed_never_suppresses() {
        // After a generation change / pane reset the committed offset is reset to 0
        // (nothing is known-delivered in the fresh generation). A real range_end then
        // sits ABOVE the 0 floor → NOT already committed → fresh output is relayed,
        // never wrongly suppressed by a stale-high pre-reset value.
        assert!(!range_already_committed(422_855, 0));
        assert!(!range_already_committed(1, 0));
    }

    // ---- #3593 flag-independent current-generation durable frontier reader ----
    // (codex HIGH: the synthetic-resume dedup gate must fuse the durable frontier
    // EVEN when AGENTDESK_DELIVERY_RECORD_AUTHORITY is OFF — `effective_committed_offset`
    // hides it under the flag, so the gate reads this flag-INDEPENDENT path instead.)

    /// The exact #3593 AUTHORITY-OFF regression codex flagged: a restart / synthetic
    /// resume reset the in-memory `committed_relay_offset` to 0, but the durable
    /// frontier still holds the CURRENT-generation high watermark (443154). The
    /// flag-independent reader surfaces it, so the caller's `0.max(443154)=443154`
    /// fusion makes `range_already_committed(422855, 443154)=true` → the already-
    /// delivered body is suppressed (no #3520 new-message-guard regression).
    #[test]
    fn current_gen_durable_frontier_covers_authority_off_resume_dup_3593() {
        let dir = tempfile::tempdir().unwrap();
        let path = delivery_record_path_in_root(dir.path(), &ProviderKind::Claude, 3593);
        let gen_ns = 700_000_000_i64;
        write_delivered_frontier_at(
            &path,
            DeliveredCommit {
                range: (0, 443_154),
                generation_mtime_ns: gen_ns,
                attempts: 1,
                panel_msg_id: Some(1),
                panel_channel_id: None,
            },
        )
        .unwrap();
        // Current generation matches the frontier's → trust the durable END.
        let durable_end = current_generation_durable_frontier_end_at(&path, gen_ns, Some(443_154));
        assert_eq!(durable_end, Some(443_154));
        // In-memory committed reset to 0 (the AUTHORITY-OFF restart hazard); the
        // fused floor rises to the durable value → the synthetic-resume range is
        // recognized as already-delivered.
        let in_memory_committed = 0_u64;
        let fused = in_memory_committed.max(durable_end.unwrap_or(0));
        assert_eq!(fused, 443_154);
        assert!(range_already_committed(422_855, fused));
    }

    /// Over-suppression no-regression: even with the durable frontier fused in, a
    /// genuinely-NEW answer (range_end ABOVE the durable high watermark) is NOT
    /// suppressed — the `max` only raises the floor to the known-delivered offset,
    /// never above a fresh range_end.
    #[test]
    fn current_gen_durable_frontier_does_not_oversuppress_new_answer() {
        let dir = tempfile::tempdir().unwrap();
        let path = delivery_record_path_in_root(dir.path(), &ProviderKind::Claude, 35930);
        let gen_ns = 700_000_000_i64;
        write_delivered_frontier_at(
            &path,
            DeliveredCommit {
                range: (0, 443_154),
                generation_mtime_ns: gen_ns,
                attempts: 1,
                panel_msg_id: None,
                panel_channel_id: None,
            },
        )
        .unwrap();
        let fused = 0_u64.max(
            current_generation_durable_frontier_end_at(&path, gen_ns, Some(443_154)).unwrap_or(0),
        );
        // A new answer produced AFTER the durable high watermark.
        assert!(!range_already_committed(443_200, fused));
    }

    /// #1270 generation gating: a STALE prior-generation durable frontier is
    /// distrusted (→ `None` → fused floor stays at in-memory). This is what keeps
    /// the fusion safe after a pane reset/respawn — a stale-high value can NEVER
    /// over-suppress the new generation's fresh output.
    #[test]
    fn current_gen_durable_frontier_distrusts_stale_generation() {
        let dir = tempfile::tempdir().unwrap();
        let path = delivery_record_path_in_root(dir.path(), &ProviderKind::Claude, 35931);
        write_delivered_frontier_at(
            &path,
            DeliveredCommit {
                range: (0, 443_154),
                generation_mtime_ns: 100, // written by a PRIOR generation
                attempts: 1,
                panel_msg_id: None,
                panel_channel_id: None,
            },
        )
        .unwrap();
        // Current generation differs → distrust → None.
        assert_eq!(
            current_generation_durable_frontier_end_at(&path, 999, Some(443_154)),
            None
        );
        // `current_gen == 0` (no .generation file) → also distrust.
        assert_eq!(
            current_generation_durable_frontier_end_at(&path, 0, Some(443_154)),
            None
        );
        // Fused floor stays at in-memory (here 0) → a real range above 0 is NOT
        // suppressed (the new generation's fresh output is relayed).
        let fused = 0_u64.max(
            current_generation_durable_frontier_end_at(&path, 999, Some(443_154)).unwrap_or(0),
        );
        assert!(!range_already_committed(422_855, fused));
    }

    /// #4188: `/compact` can shrink the transcript without changing the wrapper
    /// `.generation` file. Even with a CURRENT generation, a durable END beyond
    /// the current EOF is physically stale and must not raise the committed floor.
    #[test]
    fn current_gen_durable_frontier_distrusts_end_beyond_current_eof_4188() {
        let dir = tempfile::tempdir().unwrap();
        let path = delivery_record_path_in_root(dir.path(), &ProviderKind::Claude, 4188);
        let gen_ns = 700_000_000_i64;
        write_delivered_frontier_at(
            &path,
            DeliveredCommit {
                range: (0, 20_536_793),
                generation_mtime_ns: gen_ns,
                attempts: 1,
                panel_msg_id: None,
                panel_channel_id: None,
            },
        )
        .unwrap();

        assert_eq!(
            current_generation_durable_frontier_end_at(&path, gen_ns, Some(16_545_127)),
            None
        );
        assert_eq!(
            current_generation_durable_frontier_end_at(&path, gen_ns, Some(20_536_793)),
            Some(20_536_793)
        );
        assert_eq!(
            current_generation_durable_frontier_end_at(&path, gen_ns, None),
            None
        );
    }

    #[test]
    fn current_generation_frontier_eof_regression_signal_is_exact_4549() {
        let dir = tempfile::tempdir().unwrap();
        let path = delivery_record_path_in_root(dir.path(), &ProviderKind::Claude, 4549);
        write_delivered_frontier_at(
            &path,
            DeliveredCommit {
                range: (0, 900),
                generation_mtime_ns: 700,
                attempts: 1,
                panel_msg_id: None,
                panel_channel_id: None,
            },
        )
        .unwrap();

        assert_eq!(
            current_generation_frontier_exceeding_eof_at(&path, 700, 250),
            Some(900)
        );
        assert_eq!(
            current_generation_frontier_exceeding_eof_at(&path, 701, 250),
            None
        );
        assert_eq!(
            current_generation_frontier_exceeding_eof_at(&path, 700, 900),
            None
        );
    }

    #[test]
    fn same_generation_frontier_reanchor_converges_once_4841() {
        let dir = tempfile::tempdir().unwrap();
        let path = delivery_record_path_in_root(dir.path(), &ProviderKind::Claude, 4841);
        write_delivered_frontier_at(
            &path,
            DeliveredCommit {
                range: (80, 900),
                generation_mtime_ns: 700,
                attempts: 3,
                panel_msg_id: Some(42),
                panel_channel_id: Some(84),
            },
        )
        .unwrap();

        assert!(reanchor_current_generation_frontier_at(&path, 700, 900, 250).unwrap());
        let rewritten = read_record_at(&path).unwrap().delivered_frontier.unwrap();
        assert_eq!(rewritten.range, (80, 250));
        assert_eq!(rewritten.generation_mtime_ns, 700);
        assert_eq!(rewritten.attempts, 3);
        assert_eq!(rewritten.panel_msg_id, Some(42));
        assert_eq!(rewritten.panel_channel_id, Some(84));
        assert_eq!(
            current_generation_frontier_exceeding_eof_at(&path, 700, 250),
            None
        );
        assert!(!reanchor_current_generation_frontier_at(&path, 700, 900, 250).unwrap());
    }

    #[test]
    fn frontier_reanchor_defers_when_concurrent_commit_wins_lock_4841() {
        let dir = tempfile::tempdir().unwrap();
        let path = delivery_record_path_in_root(dir.path(), &ProviderKind::Claude, 48_411);
        write_delivered_frontier_at(
            &path,
            DeliveredCommit {
                range: (0, 900),
                generation_mtime_ns: 700,
                attempts: 1,
                panel_msg_id: None,
                panel_channel_id: None,
            },
        )
        .unwrap();
        let observed_frontier =
            current_generation_frontier_exceeding_eof_at(&path, 700, 250).unwrap();

        write_delivered_frontier_at(
            &path,
            DeliveredCommit {
                range: (250, 400),
                generation_mtime_ns: 700,
                attempts: 2,
                panel_msg_id: Some(42),
                panel_channel_id: Some(84),
            },
        )
        .unwrap();

        assert!(
            !reanchor_current_generation_frontier_at(&path, 700, observed_frontier, 250).unwrap()
        );
        let winner = read_record_at(&path).unwrap().delivered_frontier.unwrap();
        assert_eq!(winner.range, (250, 400));
        assert_eq!(winner.attempts, 2);
        assert_eq!(winner.panel_msg_id, Some(42));
    }

    #[test]
    fn frontier_reanchor_rejects_stale_generation_4841() {
        let dir = tempfile::tempdir().unwrap();
        let path = delivery_record_path_in_root(dir.path(), &ProviderKind::Claude, 48_412);
        write_delivered_frontier_at(
            &path,
            DeliveredCommit {
                range: (0, 900),
                generation_mtime_ns: 700,
                attempts: 1,
                panel_msg_id: None,
                panel_channel_id: None,
            },
        )
        .unwrap();

        assert!(!reanchor_current_generation_frontier_at(&path, 701, 900, 250).unwrap());
        assert_eq!(
            read_record_at(&path)
                .unwrap()
                .delivered_frontier
                .unwrap()
                .range,
            (0, 900)
        );
    }

    /// I3 conservatism: an absent or malformed record yields no durable floor
    /// (`None` → fuse contributes 0), so the fusion degrades to pure in-memory —
    /// never an "assume delivered" that would drop a message.
    #[test]
    fn current_gen_durable_frontier_missing_or_malformed_is_none() {
        let dir = tempfile::tempdir().unwrap();
        let missing = delivery_record_path_in_root(dir.path(), &ProviderKind::Codex, 35932);
        assert_eq!(
            current_generation_durable_frontier_end_at(&missing, 5, Some(0)),
            None
        );

        let malformed = delivery_record_path_in_root(dir.path(), &ProviderKind::Claude, 35933);
        fs::create_dir_all(malformed.parent().unwrap()).unwrap();
        fs::write(&malformed, "{ not json").unwrap();
        assert_eq!(
            current_generation_durable_frontier_end_at(&malformed, 5, Some(0)),
            None
        );
    }

    // ---- #3610 PR-2 (anchor-repost stale-anchor guard) -------------------------
    // `current_generation_delivered_anchor_at` is the structural guard that gates
    // the recovery anchor-repost fallback. It must return the anchor ONLY for a
    // CURRENT-generation, fully-populated frontier — funneling through the same
    // #1270 generation gate so a stale prior-generation anchor (from a same-named
    // tmux respawn / a previous turn) can never drive a repost.

    #[test]
    fn current_gen_anchor_returns_populated_current_generation_anchor() {
        let dir = tempfile::tempdir().unwrap();
        let path = delivery_record_path_in_root(dir.path(), &ProviderKind::Claude, 36101);
        write_delivered_frontier_at(
            &path,
            DeliveredCommit {
                range: (10, 443_154),
                generation_mtime_ns: 700,
                attempts: 1,
                panel_msg_id: Some(555),
                panel_channel_id: Some(777),
            },
        )
        .unwrap();
        // Matching generation + fully-populated anchor → Some.
        assert_eq!(
            current_generation_delivered_anchor_at(&path, 700, Some(443_154)),
            Some(CurrentGenerationAnchor {
                panel_msg_id: 555,
                panel_channel_id: 777,
                range: (10, 443_154),
            })
        );
    }

    #[test]
    fn current_gen_anchor_distrusts_stale_generation() {
        let dir = tempfile::tempdir().unwrap();
        let path = delivery_record_path_in_root(dir.path(), &ProviderKind::Claude, 36102);
        write_delivered_frontier_at(
            &path,
            DeliveredCommit {
                range: (0, 100),
                generation_mtime_ns: 100, // PRIOR generation
                attempts: 1,
                panel_msg_id: Some(555),
                panel_channel_id: Some(777),
            },
        )
        .unwrap();
        // Current generation differs → distrust → None (no repost of a stale turn).
        assert_eq!(
            current_generation_delivered_anchor_at(&path, 999, Some(100)),
            None
        );
        // No `.generation` file (current_gen == 0) → also distrust.
        assert_eq!(
            current_generation_delivered_anchor_at(&path, 0, Some(100)),
            None
        );
    }

    #[test]
    fn current_gen_anchor_rejects_zero_or_missing_anchor_pair() {
        let dir = tempfile::tempdir().unwrap();
        // panel_msg_id == 0 (un-anchored / TUI-direct sentinel) → None.
        let zero_msg = delivery_record_path_in_root(dir.path(), &ProviderKind::Claude, 36103);
        write_delivered_frontier_at(
            &zero_msg,
            DeliveredCommit {
                range: (0, 42),
                generation_mtime_ns: 700,
                attempts: 1,
                panel_msg_id: Some(0),
                panel_channel_id: Some(777),
            },
        )
        .unwrap();
        assert_eq!(
            current_generation_delivered_anchor_at(&zero_msg, 700, Some(42)),
            None
        );

        // panel_channel_id == 0 → None.
        let zero_ch = delivery_record_path_in_root(dir.path(), &ProviderKind::Claude, 36104);
        write_delivered_frontier_at(
            &zero_ch,
            DeliveredCommit {
                range: (0, 42),
                generation_mtime_ns: 700,
                attempts: 1,
                panel_msg_id: Some(555),
                panel_channel_id: Some(0),
            },
        )
        .unwrap();
        assert_eq!(
            current_generation_delivered_anchor_at(&zero_ch, 700, Some(42)),
            None
        );

        // Absent anchor pair (legacy short-replace frontier w/o anchor) → None.
        let no_anchor = delivery_record_path_in_root(dir.path(), &ProviderKind::Claude, 36105);
        write_delivered_frontier_at(
            &no_anchor,
            DeliveredCommit {
                range: (0, 42),
                generation_mtime_ns: 700,
                attempts: 1,
                panel_msg_id: None,
                panel_channel_id: None,
            },
        )
        .unwrap();
        assert_eq!(
            current_generation_delivered_anchor_at(&no_anchor, 700, Some(42)),
            None
        );
    }

    #[test]
    fn current_gen_anchor_missing_or_malformed_is_none() {
        let dir = tempfile::tempdir().unwrap();
        let missing = delivery_record_path_in_root(dir.path(), &ProviderKind::Codex, 36106);
        assert_eq!(
            current_generation_delivered_anchor_at(&missing, 5, Some(0)),
            None
        );

        let malformed = delivery_record_path_in_root(dir.path(), &ProviderKind::Claude, 36107);
        fs::create_dir_all(malformed.parent().unwrap()).unwrap();
        fs::write(&malformed, "{ not json").unwrap();
        assert_eq!(
            current_generation_delivered_anchor_at(&malformed, 5, Some(0)),
            None
        );
    }

    // ---- #3610 (Phase B PR-1 / PR-1b) terminal-anchor recording ----------------
    // `shadow_mirror_delivered_frontier` forwards the caller's
    // `terminal_anchor_msg_id` + `terminal_anchor_channel_id` verbatim as the
    // `panel_msg_id` / `panel_channel_id` arguments to
    // `record_delivered_frontier_shadow_at` (the path-based testable core), so
    // these exercise that core with the anchor semantics the three call sites use:
    //   - sink/watcher (same-channel) → (Some(channel), Some(current_msg_id))
    //   - bridge cutover (PR-1b, cross-channel) → (Some(edit channel), Some(msg))
    // The status-panel id (`status_message_id`) is no longer read; the recorded
    // anchor is now the terminal `current_msg_id` the replace edits in place, and
    // PR-1b also records WHICH channel that anchor message lives in.

    #[test]
    fn anchor_msg_id_recorded_as_panel_msg_id_3610() {
        // The sink/watcher path: the caller resolves the terminal anchor =
        // `current_msg_id` (the active-slot / PlaceholderEdit-target message) and
        // passes it as the `panel_msg_id` argument, with `panel_channel_id` = the
        // (same) channel. Both land in `DeliveredCommit` verbatim — NOT the
        // status-panel id.
        let dir = tempfile::tempdir().unwrap();
        let path = delivery_record_path_in_root(dir.path(), &ProviderKind::Claude, 36100);
        // Anchor (current_msg_id) is a DIFFERENT value than any status-panel id
        // would be — pinning that the recorded datum is the terminal anchor.
        let terminal_anchor: u64 = 555_111_222;
        let anchor_channel: u64 = 444_000_111;
        record_delivered_frontier_shadow_at(
            &path,
            (0, 100),
            700,
            0,
            Some(terminal_anchor),
            Some(anchor_channel),
            100,
        )
        .unwrap();
        let written = read_record_at(&path).unwrap().delivered_frontier.unwrap();
        assert_eq!(written.panel_msg_id, Some(terminal_anchor));
        assert_eq!(written.panel_channel_id, Some(anchor_channel));
        assert_eq!(written.range, (0, 100));
    }

    #[test]
    fn cutover_records_cross_channel_anchor_pair_3610b() {
        // #3610 PR-1b — the REAL prod/incident terminal path (bridge cutover). The
        // frontier is KEYED by the offset-authority channel (`watcher_owner_channel_id`),
        // but the anchor message lives in the (possibly DIFFERENT) edit-target channel.
        // The cutover therefore records the (edit channel, edit msg) PAIR while the
        // frontier key stays the owner channel — so the recorded `panel_msg_id` is NO
        // LONGER null (the bug PR-1 left), and `panel_channel_id` tells PR-2 which
        // channel to edit. This pins that the pair is recorded verbatim and non-null.
        let dir = tempfile::tempdir().unwrap();
        let path = delivery_record_path_in_root(dir.path(), &ProviderKind::Claude, 36104);
        let edit_msg: u64 = 700_111_222; // current_msg_id (edit target)
        let edit_channel: u64 = 800_333_444; // delivery channel != owner channel
        record_delivered_frontier_shadow_at(
            &path,
            (0, 100),
            700,
            0,
            Some(edit_msg),
            Some(edit_channel),
            100,
        )
        .unwrap();
        let written = read_record_at(&path).unwrap().delivered_frontier.unwrap();
        assert_eq!(written.panel_msg_id, Some(edit_msg)); // NOT null (PR-1's bug)
        assert_eq!(written.panel_channel_id, Some(edit_channel));
        assert_eq!(written.range, (0, 100));
    }

    #[test]
    fn anchor_none_records_null_panel_pair_3610() {
        // The defensive `None`/`None` path (no resolvable anchor): the durable anchor
        // pair stays null (unchanged from the absent-status-panel case), and the
        // frontier still advances normally.
        let dir = tempfile::tempdir().unwrap();
        let path = delivery_record_path_in_root(dir.path(), &ProviderKind::Claude, 36101);
        record_delivered_frontier_shadow_at(&path, (0, 100), 700, 0, None, None, 100).unwrap();
        let written = read_record_at(&path).unwrap().delivered_frontier.unwrap();
        assert_eq!(written.panel_msg_id, None);
        assert_eq!(written.panel_channel_id, None);
        assert_eq!(written.range, (0, 100));
    }

    #[test]
    fn panel_channel_id_backward_compatible_serde_default_3610b() {
        // #3610 PR-1b backward compat: a PR-1 record (or any pre-#3610b record) has
        // NO `panel_channel_id` field. `#[serde(default)]` must deserialize it to
        // `None` without error, leaving the rest of the frontier intact.
        let legacy_json = r#"{
            "delivery_lease": null,
            "delivered_frontier": {
                "range": [0, 443154],
                "generation_mtime_ns": 700000000,
                "attempts": 1,
                "panel_msg_id": 555111222
            }
        }"#;
        let record: DeliveryRecord = serde_json::from_str(legacy_json).unwrap();
        let frontier = record.delivered_frontier.unwrap();
        assert_eq!(frontier.panel_msg_id, Some(555_111_222));
        assert_eq!(frontier.panel_channel_id, None); // serde default — no field present
        assert_eq!(frontier.range, (0, 443_154));
    }

    #[test]
    fn anchor_value_is_dedup_byte_invariant_3610() {
        // ★ #3593 byte-impact 0: the recorded anchor PAIR (`panel_msg_id` +
        // #3610b `panel_channel_id`) is NEVER read by the offset-dedup path — the
        // single durable-frontier reader (`current_generation_durable_frontier_end_at`)
        // reads only `.range.1`. So two records with the SAME frontier END but
        // DIFFERENT anchor pairs ((Some(msg), Some(channel)) vs (None, None), the old
        // status_message_id=null incident case) MUST yield byte-identical dedup
        // decisions.
        let dir = tempfile::tempdir().unwrap();
        let gen_ns = 700_000_000_i64;

        let path_with_anchor =
            delivery_record_path_in_root(dir.path(), &ProviderKind::Claude, 36102);
        write_delivered_frontier_at(
            &path_with_anchor,
            DeliveredCommit {
                range: (0, 443_154),
                generation_mtime_ns: gen_ns,
                attempts: 1,
                panel_msg_id: Some(999_888_777), // #3610: terminal anchor present
                panel_channel_id: Some(111_222_333), // #3610b: anchor channel present
            },
        )
        .unwrap();

        let path_null_anchor =
            delivery_record_path_in_root(dir.path(), &ProviderKind::Claude, 36103);
        write_delivered_frontier_at(
            &path_null_anchor,
            DeliveredCommit {
                range: (0, 443_154),
                generation_mtime_ns: gen_ns,
                attempts: 1,
                panel_msg_id: None, // pre-fix incident shape (status_message_id=null)
                panel_channel_id: None,
            },
        )
        .unwrap();

        // Same durable frontier END read out regardless of anchor.
        let end_with =
            current_generation_durable_frontier_end_at(&path_with_anchor, gen_ns, Some(443_154));
        let end_null =
            current_generation_durable_frontier_end_at(&path_null_anchor, gen_ns, Some(443_154));
        assert_eq!(end_with, end_null);
        assert_eq!(end_with, Some(443_154));

        // Same dedup decision regardless of anchor (the #3593 suppress + the
        // over-suppression no-regression both unchanged).
        let fused_with = 0_u64.max(end_with.unwrap_or(0));
        let fused_null = 0_u64.max(end_null.unwrap_or(0));
        assert_eq!(fused_with, fused_null);
        assert_eq!(
            range_already_committed(422_855, fused_with),
            range_already_committed(422_855, fused_null)
        );
        assert!(range_already_committed(422_855, fused_with)); // suppressed
        assert_eq!(
            range_already_committed(443_200, fused_with),
            range_already_committed(443_200, fused_null)
        );
        assert!(!range_already_committed(443_200, fused_with)); // new answer relayed
    }

    #[test]
    fn shadow_off_is_anchor_noop_3610() {
        // Phase A: the old gate now controls divergence telemetry only. It must
        // remain false under OFF, but confirmed delivery authority is written by
        // the flag-independent funnel tested below.
        assert!(!should_shadow_mirror(true, false)); // no divergence telemetry
        assert!(!should_shadow_mirror(false, true)); // not delivered → no anchor write (I2)
    }

    #[cfg(unix)]
    #[test]
    fn shadow_off_confirmed_delivery_writes_exact_split_channel_authority_4911() {
        let root = IsolatedRoot::new();
        let _shadow_off = shadow_test_seam::force(false);
        let provider = ProviderKind::Claude;
        let owner = ChannelId::new(4_911_001);
        let delivery = ChannelId::new(4_911_002);
        let message_id = 4_911_003;
        let tmux = "AgentDesk-claude-phase-a-flag-off";
        let gen_path = crate::services::tmux_common::session_temp_path(tmux, "generation");
        if let Some(parent) = Path::new(&gen_path).parent() {
            fs::create_dir_all(parent).expect("create generation parent");
        }
        fs::write(&gen_path, "phase-a").expect("write generation marker");
        let generation = current_generation_mtime_ns(tmux);
        assert_ne!(generation, 0);

        let mut state = crate::services::discord::inflight::InflightTurnState::new(
            provider.clone(),
            delivery.get(),
            Some("phase-a".into()),
            7,
            49_110,
            message_id,
            "flag independent".into(),
            Some("session-4911".into()),
            Some(tmux.into()),
            Some("/tmp/phase-a-flag-off.jsonl".into()),
            None,
            100,
        );
        state.last_offset = 200;
        state.turn_nonce = Some("flag-off-turn-nonce".into());
        state.set_watcher_owner_channel_id(owner.get());
        crate::services::discord::inflight::save_inflight_state(&state)
            .expect("save delivered turn identity");

        let shared = crate::services::discord::make_shared_data_for_tests();
        let coord = shared.tmux_relay_coord(owner);
        coord.confirmed_end_offset.store(200, Ordering::Release);
        coord
            .confirmed_end_generation_mtime_ns
            .store(generation, Ordering::Release);
        shadow_mirror_delivered_frontier(
            &shared,
            &provider,
            owner,
            Some(tmux),
            (100, 200),
            true,
            Some(message_id),
            Some(delivery.get()),
            Some("same body is not authority"),
            Some(49_110),
        );

        let record = read_record(&provider, owner.get()).expect("flag-off record");
        assert_eq!(record.delivered_frontier.unwrap().range, (100, 200));
        assert_eq!(record.confirmed_deliveries.len(), 1);
        let source = &record.confirmed_deliveries[0].source;
        assert_eq!(source.offset_authority_channel_id, owner.get());
        assert_eq!(source.delivery_channel_id, delivery.get());
        assert!(confirmed_delivery_receipt_exists(
            &provider, delivery, message_id, source,
        ));
        assert!(
            delivery_record_path(&provider, owner.get())
                .expect("owner record path")
                .starts_with(root.path())
        );
    }

    #[cfg(unix)]
    #[test]
    fn confirmed_receipts_reverse_order_merge_without_frontier_regression_4911() {
        let _root = IsolatedRoot::new();
        let provider = ProviderKind::Claude;
        let owner = 4_911_101;
        let delivery = 4_911_102;
        let tmux = "AgentDesk-claude-phase-a-r2-reverse";
        let generation = set_phase_a_generation(tmux, 1_700_491_101);

        let (newer_frontier, newer_receipt) = exact_delivery_fixture(
            &provider,
            tmux,
            "newer-turn",
            (100, 300),
            generation,
            (owner, delivery),
            4_911_103,
        );
        write_confirmed_delivery(&provider, owner, newer_frontier, newer_receipt)
            .expect("write newer receipt first");

        let (older_frontier, older_receipt) = exact_delivery_fixture(
            &provider,
            tmux,
            "delayed-older-turn",
            (0, 200),
            generation,
            (owner, delivery),
            4_911_104,
        );
        write_confirmed_delivery(&provider, owner, older_frontier, older_receipt)
            .expect("append delayed lower receipt without regressing frontier");

        let (equal_frontier, equal_receipt) = exact_delivery_fixture(
            &provider,
            tmux,
            "delayed-equal-end-turn",
            (50, 300),
            generation,
            (owner, delivery),
            4_911_105,
        );
        write_confirmed_delivery(&provider, owner, equal_frontier, equal_receipt)
            .expect("append delayed equal-end receipt without replacing frontier metadata");

        let record = read_record(&provider, owner).expect("merged record");
        let frontier = record.delivered_frontier.expect("monotonic frontier");
        assert_eq!(frontier.range, (100, 300));
        assert_eq!(frontier.panel_msg_id, Some(4_911_103));
        assert_eq!(record.confirmed_deliveries.len(), 3);
        assert_eq!(
            record
                .confirmed_deliveries
                .iter()
                .map(|receipt| receipt.message_id)
                .collect::<Vec<_>>(),
            vec![4_911_103, 4_911_104, 4_911_105]
        );
    }

    #[cfg(unix)]
    #[test]
    fn receiptless_confirmed_frontiers_reverse_order_keep_winner_identity_4911() {
        let _root = IsolatedRoot::new();
        let provider = ProviderKind::Claude;
        let owner = ChannelId::new(4_911_401);
        let tmux = "AgentDesk-claude-phase-a-r3-receiptless-reverse";
        let generation = set_phase_a_generation(tmux, 1_700_491_401);
        let mut state = crate::services::discord::inflight::InflightTurnState::new(
            provider.clone(),
            owner.get(),
            Some("phase-a-r3".into()),
            7,
            49_114,
            4_911_402,
            "receipt unavailable".into(),
            Some("session-r3".into()),
            Some(tmux.into()),
            Some("/tmp/phase-a-r3-receiptless.jsonl".into()),
            None,
            100,
        );
        state.last_offset = 300;
        state.turn_nonce = None;
        crate::services::discord::inflight::save_inflight_state(&state)
            .expect("persist receipt-less delivery incarnation");
        let shared = crate::services::discord::make_shared_data_for_tests();
        let coord = shared.tmux_relay_coord(owner);
        coord.confirmed_end_offset.store(300, Ordering::Release);
        coord
            .confirmed_end_generation_mtime_ns
            .store(generation, Ordering::Release);
        let newer = DeliveredCommit {
            range: (100, 300),
            generation_mtime_ns: generation,
            attempts: 0,
            panel_msg_id: Some(4_911_402),
            panel_channel_id: Some(owner.get()),
        };
        shadow_mirror_delivered_frontier(
            &shared,
            &provider,
            owner,
            Some(tmux),
            newer.range,
            true,
            newer.panel_msg_id,
            newer.panel_channel_id,
            None,
            Some(state.user_msg_id),
        );

        for (range, message_id) in [((0, 200), 4_911_404), ((50, 300), 4_911_406)] {
            shadow_mirror_delivered_frontier(
                &shared,
                &provider,
                owner,
                Some(tmux),
                range,
                true,
                Some(message_id),
                Some(owner.get()),
                None,
                Some(state.user_msg_id),
            );
        }

        let record = read_record(&provider, owner.get()).expect("receipt-less record");
        assert_eq!(record.delivered_frontier, Some(newer));
        assert!(record.confirmed_deliveries.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn cleared_inflight_current_generation_persists_frontier_without_receipt_4911() {
        let _root = IsolatedRoot::new();
        let provider = ProviderKind::Claude;
        let owner = ChannelId::new(4_911_601);
        let delivery = ChannelId::new(4_911_602);
        let tmux = "AgentDesk-claude-phase-a-r4-cleared-current";
        let generation = set_phase_a_generation(tmux, 1_700_491_601);
        let shared = crate::services::discord::make_shared_data_for_tests();
        let coord = shared.tmux_relay_coord(owner);
        coord.confirmed_end_offset.store(240, Ordering::Release);
        coord
            .confirmed_end_generation_mtime_ns
            .store(generation, Ordering::Release);

        // No inflight row exists: the caller-held incarnation is sufficient for
        // the frontier, but cannot manufacture exact turn/nonce receipt proof.
        shadow_mirror_delivered_frontier(
            &shared,
            &provider,
            owner,
            Some(tmux),
            (120, 240),
            true,
            Some(4_911_603),
            Some(delivery.get()),
            Some("confirmed after inflight clear"),
            Some(49_116),
        );

        let record = read_record(&provider, owner.get()).expect("receipt-less frontier record");
        assert_eq!(
            record.delivered_frontier,
            Some(DeliveredCommit {
                range: (120, 240),
                generation_mtime_ns: generation,
                attempts: 0,
                panel_msg_id: Some(4_911_603),
                panel_channel_id: Some(delivery.get()),
            })
        );
        assert!(
            record.confirmed_deliveries.is_empty(),
            "caller-held session identity must not synthesize an exact receipt"
        );
    }

    #[cfg(unix)]
    #[test]
    fn cleared_inflight_stale_caller_generation_rejects_frontier_4911() {
        let _root = IsolatedRoot::new();
        let provider = ProviderKind::Codex;
        let owner = ChannelId::new(4_911_611);
        let tmux = "AgentDesk-codex-phase-a-r4-cleared-stale";
        let stale_generation = set_phase_a_generation(tmux, 1_700_491_611);
        let shared = crate::services::discord::make_shared_data_for_tests();
        let coord = shared.tmux_relay_coord(owner);
        coord.confirmed_end_offset.store(200, Ordering::Release);
        coord
            .confirmed_end_generation_mtime_ns
            .store(stale_generation, Ordering::Release);
        let current_generation = set_phase_a_generation(tmux, 1_700_491_612);
        assert_ne!(stale_generation, current_generation);

        shadow_mirror_delivered_frontier(
            &shared,
            &provider,
            owner,
            Some(tmux),
            (0, 200),
            true,
            Some(4_911_612),
            Some(owner.get()),
            None,
            Some(49_117),
        );

        assert!(
            read_record(&provider, owner.get()).is_none(),
            "post-lock generation validation must reject stale caller authority"
        );
    }

    #[cfg(unix)]
    #[test]
    fn legacy_frontier_reverse_order_split_channel_keeps_winner_whole_4911() {
        let _root = IsolatedRoot::new();
        let provider = ProviderKind::Claude;
        let owner = 4_911_621;
        let tmux = "AgentDesk-claude-phase-a-r4-legacy-reverse";
        let generation = set_phase_a_generation(tmux, 1_700_491_621);
        let winner = DeliveredCommit {
            range: (100, 300),
            generation_mtime_ns: generation,
            attempts: 9,
            panel_msg_id: Some(4_911_622),
            panel_channel_id: Some(4_911_623),
        };
        write_delivered_frontier(&provider, owner, tmux, winner.clone())
            .expect("write winning split-channel frontier");

        for frontier in [
            DeliveredCommit {
                range: (0, 200),
                generation_mtime_ns: generation,
                attempts: 2,
                panel_msg_id: Some(4_911_624),
                panel_channel_id: Some(4_911_625),
            },
            DeliveredCommit {
                range: (50, 300),
                generation_mtime_ns: generation,
                attempts: 3,
                panel_msg_id: Some(4_911_626),
                panel_channel_id: Some(4_911_627),
            },
        ] {
            write_delivered_frontier(&provider, owner, tmux, frontier)
                .expect("merge delayed legacy frontier");
        }

        assert_eq!(
            read_record(&provider, owner)
                .expect("legacy record")
                .delivered_frontier,
            Some(winner),
            "lower/equal legacy writes must not union START or replace split-channel anchor"
        );
    }

    #[cfg(unix)]
    #[test]
    fn ordered_jsonl_reverse_order_preserves_split_channel_anchor_4911() {
        let _root = IsolatedRoot::new();
        let provider = ProviderKind::Codex;
        let owner = ChannelId::new(4_911_631);
        let tmux = "AgentDesk-codex-phase-a-r4-ordered-reverse";
        let generation = set_phase_a_generation(tmux, 1_700_491_631);
        let winner = DeliveredCommit {
            range: (100, 300),
            generation_mtime_ns: generation,
            attempts: 7,
            panel_msg_id: Some(4_911_632),
            panel_channel_id: Some(4_911_633),
        };
        write_delivered_frontier(&provider, owner.get(), tmux, winner.clone())
            .expect("seed split-channel winner");

        for range in [(0, 200), (50, 300)] {
            assert!(
                commit_ordered_jsonl_range(&provider, owner, tmux, range, generation)
                    .expect("merge delayed ordered range")
            );
        }

        assert_eq!(
            read_record(&provider, owner.get())
                .expect("ordered record")
                .delivered_frontier,
            Some(winner),
            "ordered commits must not union ranges or clear an existing anchor"
        );
    }

    #[cfg(unix)]
    #[test]
    fn legacy_frontier_blocked_writer_revalidates_after_rotation_4911() {
        let _root = IsolatedRoot::new();
        let provider = ProviderKind::Claude;
        let owner = 4_911_641;
        let tmux = "AgentDesk-claude-phase-a-r4-legacy-race";
        let stale_generation = set_phase_a_generation(tmux, 1_700_491_641);
        let path = record_path_or_err(&provider, owner).expect("record path");
        let held_lock = lock_record_path(&path).expect("hold record lock");
        let stale_path = path.clone();
        let stale_tmux = tmux.to_string();
        let (about_to_lock_tx, about_to_lock_rx) = std::sync::mpsc::channel();
        let stale_writer = std::thread::spawn(move || {
            write_delivered_frontier_guarded_at_with_before_lock(
                &stale_path,
                &stale_tmux,
                DeliveredCommit {
                    range: (0, 900),
                    generation_mtime_ns: stale_generation,
                    attempts: 5,
                    panel_msg_id: Some(4_911_642),
                    panel_channel_id: Some(4_911_643),
                },
                || about_to_lock_tx.send(()).expect("signal lock attempt"),
            )
        });
        about_to_lock_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("legacy writer reached record lock");
        std::thread::sleep(std::time::Duration::from_millis(50));
        assert!(!stale_writer.is_finished(), "legacy writer must be blocked");

        let current_generation = set_phase_a_generation(tmux, 1_700_491_642);
        let current_record = DeliveryRecord {
            delivered_frontier: Some(DeliveredCommit {
                range: (100, 200),
                generation_mtime_ns: current_generation,
                attempts: 1,
                panel_msg_id: Some(4_911_644),
                panel_channel_id: Some(4_911_645),
            }),
            ..DeliveryRecord::default()
        };
        write_record_at(&path, &current_record).expect("install rotated record");
        drop(held_lock);

        assert!(stale_writer.join().expect("join legacy writer").is_err());
        assert_eq!(read_record_at(&path), Some(current_record));
    }

    #[cfg(unix)]
    #[test]
    fn proven_gone_reanchor_blocked_writer_preserves_concurrent_anchor_swap_4911() {
        let _root = IsolatedRoot::new();
        let provider = ProviderKind::Claude;
        let owner = 4_911_661;
        let tmux = "AgentDesk-claude-phase-a-r5-reanchor-race";
        let generation = set_phase_a_generation(tmux, 1_700_491_661);
        let path = record_path_or_err(&provider, owner).expect("record path");
        let range = (100, 300);
        let gone_anchor = (4_911_662, 4_911_663);
        write_delivered_frontier(
            &provider,
            owner,
            tmux,
            DeliveredCommit {
                range,
                generation_mtime_ns: generation,
                attempts: 1,
                panel_channel_id: Some(gone_anchor.0),
                panel_msg_id: Some(gone_anchor.1),
            },
        )
        .expect("seed anchor later proved gone");

        let held_lock = lock_record_path(&path).expect("hold record lock");
        let stale_path = path.clone();
        let stale_tmux = tmux.to_string();
        let (about_to_lock_tx, about_to_lock_rx) = std::sync::mpsc::channel();
        let stale_writer = std::thread::spawn(move || {
            write_proven_gone_equal_range_frontier_at_with_before_lock(
                &stale_path,
                &stale_tmux,
                gone_anchor,
                DeliveredCommit {
                    range,
                    generation_mtime_ns: generation,
                    attempts: 2,
                    panel_channel_id: Some(4_911_664),
                    panel_msg_id: Some(4_911_665),
                },
                || about_to_lock_tx.send(()).expect("signal lock attempt"),
            )
        });
        about_to_lock_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("re-anchor writer reached record lock");
        std::thread::sleep(std::time::Duration::from_millis(50));
        assert!(!stale_writer.is_finished(), "re-anchor writer must block");

        let swapped_frontier = DeliveredCommit {
            range,
            generation_mtime_ns: generation,
            attempts: 3,
            panel_channel_id: Some(4_911_666),
            panel_msg_id: Some(4_911_667),
        };
        let swapped_record = DeliveryRecord {
            delivered_frontier: Some(swapped_frontier),
            ..DeliveryRecord::default()
        };
        write_record_at(&path, &swapped_record).expect("install concurrent anchor swap");
        drop(held_lock);

        stale_writer
            .join()
            .expect("join re-anchor writer")
            .expect("anchor mismatch is a conservative no-op");
        assert_eq!(
            read_record_at(&path),
            Some(swapped_record),
            "the under-lock anchor CAS must preserve the concurrent whole commit"
        );
    }

    #[cfg(unix)]
    #[test]
    fn proven_gone_reanchor_missing_frontier_is_conservative_noop_4911() {
        let _root = IsolatedRoot::new();
        let provider = ProviderKind::Claude;
        let owner = 4_911_681;
        let tmux = "AgentDesk-claude-phase-a-r6-reanchor-missing";
        let generation = set_phase_a_generation(tmux, 1_700_491_681);
        let path = record_path_or_err(&provider, owner).expect("record path");
        let record_without_frontier = DeliveryRecord::default();
        write_record_at(&path, &record_without_frontier).expect("seed record without frontier");

        write_proven_gone_equal_range_frontier(
            &provider,
            owner,
            tmux,
            (4_911_682, 4_911_683),
            DeliveredCommit {
                range: (100, 300),
                generation_mtime_ns: generation,
                attempts: 1,
                panel_channel_id: Some(4_911_684),
                panel_msg_id: Some(4_911_685),
            },
        )
        .expect("missing frontier remains a diagnosed conservative no-op");
        assert_eq!(read_record_at(&path), Some(record_without_frontier));
    }

    #[cfg(unix)]
    #[test]
    fn ordered_jsonl_blocked_writer_revalidates_after_rotation_4911() {
        let _root = IsolatedRoot::new();
        let provider = ProviderKind::Codex;
        let owner = 4_911_651;
        let tmux = "AgentDesk-codex-phase-a-r4-ordered-race";
        let stale_generation = set_phase_a_generation(tmux, 1_700_491_651);
        let path = record_path_or_err(&provider, owner).expect("record path");
        let held_lock = lock_record_path(&path).expect("hold record lock");
        let stale_path = path.clone();
        let stale_tmux = tmux.to_string();
        let (about_to_lock_tx, about_to_lock_rx) = std::sync::mpsc::channel();
        let stale_writer = std::thread::spawn(move || {
            commit_ordered_jsonl_range_at_with_before_lock(
                &stale_path,
                &stale_tmux,
                (0, 900),
                stale_generation,
                || about_to_lock_tx.send(()).expect("signal lock attempt"),
            )
        });
        about_to_lock_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("ordered writer reached record lock");
        std::thread::sleep(std::time::Duration::from_millis(50));
        assert!(
            !stale_writer.is_finished(),
            "ordered writer must be blocked"
        );

        let current_generation = set_phase_a_generation(tmux, 1_700_491_652);
        let current_record = DeliveryRecord {
            delivered_frontier: Some(DeliveredCommit {
                range: (100, 200),
                generation_mtime_ns: current_generation,
                attempts: 1,
                panel_msg_id: Some(4_911_652),
                panel_channel_id: Some(4_911_653),
            }),
            ..DeliveryRecord::default()
        };
        write_record_at(&path, &current_record).expect("install rotated record");
        drop(held_lock);

        assert!(stale_writer.join().expect("join ordered writer").is_err());
        assert_eq!(read_record_at(&path), Some(current_record));
    }

    #[cfg(unix)]
    #[test]
    fn receiptless_blocked_writer_revalidates_after_rotation_4911() {
        let _root = IsolatedRoot::new();
        let provider = ProviderKind::Codex;
        let owner = 4_911_501;
        let delivery = 4_911_502;
        let tmux = "AgentDesk-codex-phase-a-r3-receiptless-race";
        let old_generation = set_phase_a_generation(tmux, 1_700_491_501);
        let path = record_path_or_err(&provider, owner).expect("record path");
        let held_lock = lock_record_path(&path).expect("hold record lock");
        let stale_frontier = DeliveredCommit {
            range: (0, 900),
            generation_mtime_ns: old_generation,
            attempts: 7,
            panel_msg_id: Some(4_911_503),
            panel_channel_id: Some(delivery),
        };
        let stale_path = path.clone();
        let stale_tmux = tmux.to_string();
        let (about_to_lock_tx, about_to_lock_rx) = std::sync::mpsc::channel();
        let stale_writer = std::thread::spawn(move || {
            write_confirmed_frontier_guarded_at_with_before_lock(
                &stale_path,
                &stale_tmux,
                stale_frontier,
                None,
                EqualRangeAnchorPolicy::PreserveExisting,
                || about_to_lock_tx.send(()).expect("signal lock attempt"),
            )
        });
        about_to_lock_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("stale writer reached the record lock");
        std::thread::sleep(std::time::Duration::from_millis(50));
        assert!(
            !stale_writer.is_finished(),
            "stale writer must be lock-blocked"
        );

        let current_generation = set_phase_a_generation(tmux, 1_700_491_502);
        let (current_frontier, current_receipt) = exact_delivery_fixture(
            &provider,
            tmux,
            "current-after-rotation",
            (100, 200),
            current_generation,
            (owner, delivery),
            4_911_504,
        );
        let current_record = DeliveryRecord {
            delivered_frontier: Some(current_frontier),
            confirmed_deliveries: vec![current_receipt],
            ..DeliveryRecord::default()
        };
        write_record_at(&path, &current_record).expect("install current incarnation under lock");
        drop(held_lock);

        assert!(
            stale_writer.join().expect("join stale writer").is_err(),
            "stale receipt-less writer must reject after acquiring the lock"
        );
        assert_eq!(read_record_at(&path), Some(current_record));
    }

    #[cfg(unix)]
    #[test]
    fn rotate_after_receipt_validation_preserves_current_incarnation_4911() {
        let _root = IsolatedRoot::new();
        let provider = ProviderKind::Codex;
        let owner = 4_911_201;
        let delivery = 4_911_202;
        let tmux = "AgentDesk-codex-phase-a-r2-rotate";
        let old_generation = set_phase_a_generation(tmux, 1_700_491_201);
        let (delayed_frontier, delayed_receipt) = exact_delivery_fixture(
            &provider,
            tmux,
            "validated-before-rotate",
            (0, 400),
            old_generation,
            (owner, delivery),
            4_911_203,
        );
        assert!(delayed_receipt.is_authoritative());

        let current_generation = set_phase_a_generation(tmux, 1_700_491_202);
        assert_ne!(old_generation, current_generation);
        let (current_frontier, current_receipt) = exact_delivery_fixture(
            &provider,
            tmux,
            "current-incarnation",
            (0, 100),
            current_generation,
            (owner, delivery),
            4_911_204,
        );
        write_confirmed_delivery(&provider, owner, current_frontier, current_receipt)
            .expect("write current incarnation");
        let before = read_record(&provider, owner).expect("current record");

        assert!(
            write_confirmed_delivery(&provider, owner, delayed_frontier, delayed_receipt).is_err(),
            "the guarded writer must recheck generation after receipt construction"
        );
        assert_eq!(read_record(&provider, owner), Some(before));
    }

    #[test]
    fn confirmed_receipt_write_rejects_destination_owner_and_provider_mismatch_4911() {
        let _root = IsolatedRoot::new();
        let provider = ProviderKind::Claude;
        let owner = 4_911_301;
        let delivery = 4_911_302;
        let tmux = "AgentDesk-claude-phase-a-r2-mismatch";
        let generation = set_phase_a_generation(tmux, 1_700_491_301);
        let (frontier, receipt) = exact_delivery_fixture(
            &provider,
            tmux,
            "mismatch-turn",
            (0, 100),
            generation,
            (owner, delivery),
            4_911_303,
        );

        let mut wrong_destination = receipt.clone();
        wrong_destination.delivery_channel_id = delivery + 1;
        assert!(
            write_confirmed_delivery(&provider, owner, frontier.clone(), wrong_destination,)
                .is_err()
        );
        let mut missing_anchor_channel = frontier.clone();
        missing_anchor_channel.panel_channel_id = None;
        assert!(
            write_confirmed_delivery(&provider, owner, missing_anchor_channel, receipt.clone(),)
                .is_err()
        );
        assert!(
            write_confirmed_delivery(&provider, owner + 1, frontier.clone(), receipt.clone())
                .is_err()
        );
        assert!(write_confirmed_delivery(&ProviderKind::Codex, owner, frontier, receipt).is_err());
        assert!(read_record(&provider, owner).is_none());
    }

    // ---- #3610 PR-1c: long-chunk terminal arm anchor recording -----------------
    // `record_long_chunk_terminal_delivery` is the BRIDGE long-chunk arm's anchor
    // helper (turn_bridge/mod.rs site 4). It is a thin pass-through to
    // `shadow_mirror_delivered_frontier` with `is_delivered = true` hardcoded
    // (the caller invokes it ONLY on the full-commit `Ok` arm of
    // `send_ordered_long_terminal_response`, which is all-or-nothing — a partial
    // chunk failure rolls back and returns `Err`, so an `Ok` means every chunk
    // committed). These pin the recorded shape via the path-based core the helper
    // funnels into (same approach as the PR-1/PR-1b tests above), since the full
    // helper's flag (OnceLock) + runtime-root resolution are env-global.

    #[test]
    fn long_chunk_full_commit_records_last_chunk_anchor_3610c() {
        // The long-chunk arm deletes the placeholder, so the LAST sent chunk's
        // message id is the only stable terminal anchor. On full commit the helper
        // records (panel_msg_id = last_chunk, panel_channel_id = delivery channel)
        // — proving panel_msg_id is NON-null on the long-message path PR-1/1b left
        // uncovered. The frontier END is the lease range's `.1`.
        let dir = tempfile::tempdir().unwrap();
        let path = delivery_record_path_in_root(dir.path(), &ProviderKind::Claude, 36110);
        let last_chunk_anchor: u64 = 912_345_678; // tail chunk msg id (not the first)
        let delivery_channel: u64 = 444_555_666;
        // Mirrors the args record_long_chunk_terminal_delivery forwards: range =
        // lease (start,end), panel_msg_id = last chunk, panel_channel_id = delivery.
        record_delivered_frontier_shadow_at(
            &path,
            (0, 4096),
            700,
            0,
            Some(last_chunk_anchor),
            Some(delivery_channel),
            4096,
        )
        .unwrap();
        let written = read_record_at(&path).unwrap().delivered_frontier.unwrap();
        assert_eq!(written.panel_msg_id, Some(last_chunk_anchor)); // NON-null
        assert_eq!(written.panel_channel_id, Some(delivery_channel));
        assert_eq!(written.range, (0, 4096));
    }

    #[test]
    fn long_chunk_cross_channel_separates_owner_and_delivery_3610c() {
        // Gate (C): the frontier is KEYED by `watcher_owner_channel_id` (offset
        // authority — unchanged), while the recorded anchor PAIR points at the
        // delivery `channel_id` (where the tail chunk lives). For a reused-watcher
        // bridge these DIFFER; the helper must NOT swap them. The frontier key is the
        // record PATH's channel (owner); the recorded `panel_channel_id` is delivery.
        let dir = tempfile::tempdir().unwrap();
        let owner_channel: u64 = 100_200_300; // watcher_owner_channel_id = frontier key
        let delivery_channel: u64 = 900_800_700; // edit/delivery channel = anchor home
        assert_ne!(owner_channel, delivery_channel);
        // The record lives under the OWNER channel (the frontier key) ...
        let owner_path =
            delivery_record_path_in_root(dir.path(), &ProviderKind::Claude, owner_channel);
        // ... and its recorded anchor channel is the DELIVERY channel.
        record_delivered_frontier_shadow_at(
            &owner_path,
            (10, 8192),
            700,
            0,
            Some(777_111_222),
            Some(delivery_channel),
            8192,
        )
        .unwrap();
        let written = read_record_at(&owner_path)
            .unwrap()
            .delivered_frontier
            .unwrap();
        assert_eq!(written.panel_channel_id, Some(delivery_channel)); // delivery, not owner
        assert_eq!(written.range, (10, 8192));
        // No record was written under the DELIVERY channel (the frontier key is owner).
        let delivery_path =
            delivery_record_path_in_root(dir.path(), &ProviderKind::Claude, delivery_channel);
        assert_eq!(read_record_at(&delivery_path), None);
    }

    #[test]
    fn long_chunk_anchor_none_records_range_only_3610c() {
        // Gate (D): an empty chunk Vec (impossible on the full-commit `Ok` path, but
        // type-honest) → last = None → the helper records the range with a null
        // anchor, identical to the absent-status-panel case. The frontier still
        // advances (END = range.1) so the dedup floor is correct.
        let dir = tempfile::tempdir().unwrap();
        let path = delivery_record_path_in_root(dir.path(), &ProviderKind::Claude, 36111);
        record_delivered_frontier_shadow_at(&path, (0, 2048), 700, 0, None, Some(444), 2048)
            .unwrap();
        let written = read_record_at(&path).unwrap().delivered_frontier.unwrap();
        assert_eq!(written.panel_msg_id, None);
        assert_eq!(written.range, (0, 2048));
    }

    #[test]
    fn long_chunk_partial_or_err_never_records_anchor_3610c() {
        // Gate (A): the helper hardcodes `is_delivered = true` because the mod.rs
        // call site lives ONLY inside the full-commit `Ok` arm — the long-chunk send
        // (`send_long_message_with_rollback`) rolls back and returns `Err` on ANY
        // chunk failure, so a partial delivery NEVER reaches the helper. The shadow
        // outcome (modelled here as `is_delivered = false`) can never reach either
        // persistence or divergence telemetry.
        assert!(!should_shadow_mirror(false, true));
    }

    #[test]
    fn record_long_chunk_terminal_delivery_off_is_noop_3610c() {
        // Legacy test name retained: the synthetic coord has no generation, so
        // Phase A must fail closed without overwriting a prior authoritative
        // frontier. The shadow flag itself controls only telemetry.
        // The scoped root holds the crate-wide test-env lock, so both the helper and
        // the assertions stay inside one throw-away tree and can never inspect the
        // operator's runtime.
        let root = IsolatedRoot::new();
        let _shadow_off = shadow_test_seam::force(false);
        let shared = crate::services::discord::make_shared_data_for_tests();
        // Does not panic; missing generation → writes nothing.
        super::record_long_chunk_terminal_delivery(
            &shared,
            &ProviderKind::Claude,
            ChannelId::new(100_200_300), // owner (frontier key)
            ChannelId::new(900_800_700), // delivery (anchor home)
            None,
            (0, 4096),
            Some(912_345_678),
            "",
            None,
        );
        // No durable record was created for either channel under the test root.
        for channel_id in [100_200_300, 900_800_700] {
            let path = delivery_record_path(&ProviderKind::Claude, channel_id)
                .expect("isolated delivery-record path");
            assert!(
                path.starts_with(root.path()),
                "delivery-record path escaped isolated root: {}",
                path.display()
            );
            assert!(read_record(&ProviderKind::Claude, channel_id).is_none());
        }
    }

    #[test]
    fn ledger_append_keys_by_delivery_channel_not_watcher_owner_4564() {
        // #4564 channel-split silent-loss regression (counter-model finding). A
        // recovered/reused-watcher bridge delivers channel Y's answered turn while
        // reusing channel X's tmux session, so the funnel is invoked with
        // `channel = watcher_owner_channel_id = X` even though the delivered turn
        // belongs to Y. X still holds a PRESERVED, UNANSWERED inflight row
        // (user_msg_id = B). The completed-turn ledger MUST key by the delivery
        // channel Y and record Y's EXPLICIT user_msg_id (A) — it must NOT reload X's
        // row and false-Settle B (which would make catch-up skip B's recovery on the
        // next restart = silent loss, the #4600 class re-introduced by a new vector).
        let _root = IsolatedRoot::new();
        let _shadow_off = shadow_test_seam::force(false);
        let shared = crate::services::discord::make_shared_data_for_tests();
        let provider = ProviderKind::Codex;
        let watcher_owner_x = ChannelId::new(4_564_100); // offset-authority / session owner
        let delivery_y = ChannelId::new(4_564_200); // where the answered user msg lives
        let unanswered_b: u64 = 88_000_001; // X's preserved, un-delivered turn
        let answered_a: u64 = 99_000_002; // Y's delivered turn

        // X carries a preserved UNANSWERED inflight row for B.
        let mut x_state = crate::services::discord::inflight::InflightTurnState::new(
            provider.clone(),
            watcher_owner_x.get(),
            Some("adk-test".to_string()),
            343_742_347_365_974_026,
            unanswered_b,
            0,
            "미답변 메시지".to_string(),
            Some("session-x".to_string()),
            Some("AgentDesk-codex-adk-test".to_string()),
            Some("/tmp/4564-split.jsonl".to_string()),
            None,
            128,
        );
        x_state.turn_start_offset = Some(0);
        crate::services::discord::inflight::save_inflight_state(&x_state).expect("save X inflight");
        let tmux_session_name = x_state.tmux_session_name.as_deref().expect("tmux session");
        let generation_mtime_ns = set_phase_a_generation(tmux_session_name, 1_700_456_400);
        let coord = shared.tmux_relay_coord(watcher_owner_x);
        coord
            .confirmed_end_generation_mtime_ns
            .store(generation_mtime_ns, Ordering::Release);
        coord.confirmed_end_offset.store(128, Ordering::Release);

        // Y's answered turn commits terminally while reusing X's session.
        shadow_mirror_delivered_frontier(
            &shared,
            &provider,
            watcher_owner_x, // funnel `channel` = offset-authority X
            Some(tmux_session_name),
            (0, 128),
            true, // is_delivered
            Some(77_001),
            Some(delivery_y.get()), // terminal_anchor_channel_id = delivery channel Y
            Some("answer"),
            Some(answered_a), // explicit inbound turn id from Y's snapshot
        );

        // (b) Y's answered turn is recorded under Y.
        assert!(
            completed_turn_ledger::settled_user_msg_ids(&provider, delivery_y.get())
                .contains(&answered_a),
            "the answered turn's user_msg_id must be recorded under the delivery channel Y"
        );
        // (a) X's UNANSWERED turn must NOT be false-Settled.
        let x_settled =
            completed_turn_ledger::settled_user_msg_ids(&provider, watcher_owner_x.get());
        assert!(
            !x_settled.contains(&unanswered_b),
            "MUTATION: reverting to a commit-time reload of `channel`=X would append X's \
             unanswered B here — silently losing B on restart (#4600 class)"
        );
        assert!(
            x_settled.is_empty(),
            "no ledger entry may be written under the offset-authority channel X"
        );
    }

    /// #5154 regression. A confirmed terminal delivery whose JSONL incarnation is
    /// UNKNOWN (`generation_mtime_ns == 0`) must still leave settled evidence that
    /// the user message was answered, while the generation-scoped durable frontier
    /// stays unwritten.
    ///
    /// Production shape being reproduced: the turn-bridge funnel
    /// (`shadow_mirror_delivered_frontier`) passes `watcher_authority = None`, so the
    /// incarnation is read from `coord.confirmed_end_generation_mtime_ns`. A stale
    /// tmux-watermark reset leaves that atomic at its initial `0` and the only
    /// re-establishment site refuses to write a `0`, so confirmed deliveries kept
    /// arriving with an unknown incarnation. The old single early return dropped the
    /// completed-turn ledger append along with the frontier, and
    /// `catch_up/settled_ledger_consult.rs` treats that ledger as the ONLY
    /// settled-evidence source — so catch-up re-collected already-answered messages
    /// and re-submitted their prompts verbatim.
    ///
    /// The `range`/`user_msg_id`/`channel_id` values below are the ones observed in
    /// the reported incident (issue #5154 WARN line at 20:53:32.847Z, channel
    /// 1479671298497183835, `range=(0, 42675) generation_mtime_ns=0`).
    #[test]
    fn unknown_generation_confirmed_delivery_settles_ledger_without_frontier_5154() {
        let _root = IsolatedRoot::new();
        let provider = ProviderKind::Claude;
        let owner = ChannelId::new(1_479_671_298_497_183_835);
        let delivery = ChannelId::new(1_479_671_298_497_183_835);
        let tmux = "AgentDesk-claude-5154-unknown-incarnation";
        let pinned_user_msg_id = 1_534_665_637_186_765_043_u64;

        let shared = crate::services::discord::make_shared_data_for_tests();
        let coord = shared.tmux_relay_coord(owner);
        // The stale-watermark reset left the incarnation atomic at its initial `0`
        // and no writer re-established it. Deliberately NOT stored: `0` IS the
        // initial value, and the test must exercise exactly that state.
        assert_eq!(
            coord
                .confirmed_end_generation_mtime_ns
                .load(Ordering::Acquire),
            0,
            "precondition: the unknown-incarnation state under test"
        );
        coord.confirmed_end_offset.store(42_675, Ordering::Release);

        // Confirmed delivery: `is_delivered == true`, range strictly growing
        // (42675 > 0), incarnation unknown. Only the third condition is degenerate.
        shadow_mirror_delivered_frontier(
            &shared,
            &provider,
            owner,
            Some(tmux),
            (0, 42_675),
            true,
            Some(1_534_665_637_186_765_044),
            Some(delivery.get()),
            Some("confirmed terminal answer"),
            Some(pinned_user_msg_id),
        );

        let record = read_record(&provider, owner.get());
        // The frontier guard is UNCHANGED and just as strict: a frontier without an
        // authoritative JSONL incarnation is meaningless and must never be durable.
        assert!(
            record
                .as_ref()
                .and_then(|record| record.delivered_frontier.as_ref())
                .is_none(),
            "#5154: an unknown JSONL incarnation must never produce a durable frontier"
        );
        // The #4081 content fingerprint is generation-scoped by its match predicate
        // and is deliberately NOT recorded under the unknown-incarnation sentinel.
        assert!(
            record
                .as_ref()
                .map(|record| record.recent_delivered_contents.is_empty())
                .unwrap_or(true),
            "#5154: no fingerprint may be stored under the unknown-incarnation sentinel"
        );

        // MUTATION TARGET: "this user message was answered" is not a fact about a
        // JSONL incarnation. Collapsing the settlement append back into the frontier
        // guard's early return fails HERE, on this assertion.
        assert!(
            completed_turn_ledger::settled_user_msg_ids(&provider, delivery.get())
                .contains(&pinned_user_msg_id),
            "#5154 MUTATION: the answered user message must remain settled evidence \
             even when the JSONL incarnation is unknown — otherwise catch-up \
             re-collects it and re-submits the same turn_id verbatim"
        );
    }

    /// #5154 companion: the settlement carve-out must NOT weaken the ledger-id
    /// safety rule. With no caller-pinned inbound id there is nothing safe to
    /// settle — the receipt-qualified fresh-row fallback cannot run without an
    /// authoritative incarnation, and guessing from an unqualified current row
    /// would let a delayed A settle a newer B. Nothing is written at all.
    #[test]
    fn unknown_generation_without_pinned_inbound_id_settles_nothing_5154() {
        let _root = IsolatedRoot::new();
        let provider = ProviderKind::Claude;
        let owner = ChannelId::new(5_154_201);
        let delivery = ChannelId::new(5_154_202);
        let tmux = "AgentDesk-claude-5154-unpinned";

        let shared = crate::services::discord::make_shared_data_for_tests();

        shadow_mirror_delivered_frontier(
            &shared,
            &provider,
            owner,
            Some(tmux),
            (0, 42_675),
            true,
            Some(5_154_203),
            Some(delivery.get()),
            Some("confirmed terminal answer"),
            None, // caller pinned nothing
        );

        assert!(
            completed_turn_ledger::settled_user_msg_ids(&provider, delivery.get()).is_empty(),
            "#5154: an unpinned delivery must never invent a settled user_msg_id"
        );
        assert!(
            read_record(&provider, owner.get())
                .and_then(|record| record.delivered_frontier)
                .is_none(),
            "#5154: the frontier guard still holds"
        );
    }

    /// #5154: `is_delivered == false` is still an absolute bar. The settlement
    /// carve-out sits BELOW the `!is_delivered` return, so a non-delivered outcome
    /// records nothing even with a pinned inbound id.
    #[test]
    fn not_delivered_never_settles_the_ledger_5154() {
        let _root = IsolatedRoot::new();
        let provider = ProviderKind::Claude;
        let channel = ChannelId::new(5_154_301);
        let pinned_user_msg_id = 5_154_300_u64;

        let shared = crate::services::discord::make_shared_data_for_tests();

        shadow_mirror_delivered_frontier(
            &shared,
            &provider,
            channel,
            Some("AgentDesk-claude-5154-not-delivered"),
            (0, 42_675),
            false, // NOT delivered
            Some(5_154_302),
            Some(channel.get()),
            Some("undelivered body"),
            Some(pinned_user_msg_id),
        );

        assert!(
            completed_turn_ledger::settled_user_msg_ids(&provider, channel.get()).is_empty(),
            "#5154: a non-delivered outcome must never produce settled evidence"
        );
    }

    // ---- #3610 PR-1d: WATCHER legacy long-chunk arm (same-channel) --------------
    // The watcher long-chunk fallback arm (tmux_watcher.rs — the
    // `watcher_should_send_ordered_new_chunks_for_terminal_fallback` branch) is the
    // watcher-owned counterpart of the bridge arm above. Its sibling helper
    // `terminal_long_chunks::record_watcher_terminal_delivery` is SAME-CHANNEL:
    // it preserves `watcher_owner_channel_id == delivery_channel_id == channel_id`.
    // So the frontier key (record path)
    // and the recorded `panel_channel_id` are the SAME channel — UNLIKE the bridge's
    // cross-channel `long_chunk_cross_channel_separates_owner_and_delivery_3610c`.
    // Pinned here via the path-core (the helper's flag is env-global OnceLock).

    #[test]
    fn watcher_long_chunk_same_channel_anchor_pair_3610d() {
        // Gate (C) for the watcher arm: same-channel ⇒ panel_channel_id equals the
        // frontier-key channel (the record path's channel), and panel_msg_id is the
        // NON-null last-chunk anchor. Mirrors the args the watcher helper forwards:
        // range = (watcher_lease_start, watcher_lease_end), both channels = channel_id.
        let dir = tempfile::tempdir().unwrap();
        let watcher_channel: u64 = 778_899_001; // owner + delivery (same for watcher)
        let last_chunk_anchor: u64 = 654_321_987; // tail chunk msg id
        let path = delivery_record_path_in_root(dir.path(), &ProviderKind::Claude, watcher_channel);
        // The watcher wrapper passes channel_id as BOTH the path channel (frontier
        // key) and panel_channel_id; the path-core models that with the same value.
        record_delivered_frontier_shadow_at(
            &path,
            (10, 16384),
            700,
            0,
            Some(last_chunk_anchor),
            Some(watcher_channel), // == the record path channel (same-channel)
            16384,
        )
        .unwrap();
        let written = read_record_at(&path).unwrap().delivered_frontier.unwrap();
        assert_eq!(written.panel_msg_id, Some(last_chunk_anchor)); // NON-null
        // The recorded anchor channel IS the frontier-key channel (same-channel).
        assert_eq!(written.panel_channel_id, Some(watcher_channel));
        assert_eq!(written.range, (10, 16384));
    }

    #[test]
    fn watcher_long_chunk_partial_or_unadvanced_never_records_3610d() {
        // Gates (A)+(M4) for the watcher arm: the helper is invoked ONLY inside the
        // `if committed && Delivered { advance(); … }` block in tmux_watcher.rs, AND
        // only when the anchor is `Some` (the full-commit `Ok` arm of
        // `send_long_message_raw_with_rollback`, which is all-or-nothing — a partial
        // chunk failure rolls back and returns `Err`). So a non-advanced commit
        // (modelled as `is_delivered = false`) NEVER reaches the durable write.
        assert!(!should_shadow_mirror(false, true));
    }

    // ---- #3933 item 1: read-authority (authority-ON) end-to-end wiring --------
    // The pure helpers above (fuse / #1270 generation gate / range_already_committed)
    // are covered, but no test drove the ENV-RESOLVED public gates
    // (`effective_committed_offset` / `committed_floor_for_resend_dedup`) with the
    // flag FORCED ON — at the time, the release config
    // (AGENTDESK_DELIVERY_RECORD_AUTHORITY=1) that the then-OFF compiled default
    // never exercised; since #5071 T1 S8-2 the compiled default supplies it too.
    // These tests keep forcing it ON via the #3993 per-thread seam (deterministic
    // against a developer-shell `=0`) and verify the whole wiring end-to-end (not by
    // hand-computing the fusion): the dedup floor is `max(durable, in_memory)` so it
    // never over-suppresses, the #1270 gate distrusts a stale generation, and the
    // #3871 / #3885 duplicate-relay scenarios are correctly suppressed. This closes
    // #3933 prerequisite #2 (independent read-authority verification).

    /// RAII: point `AGENTDESK_ROOT_DIR` at an isolated tempdir for a test
    /// (restoring the prior value on drop) while holding the process-global env
    /// lock. BOTH the delivery-record path and the tmux `.generation` marker path
    /// resolve through this root, so the whole read-authority wiring runs against a
    /// throw-away tree with zero cross-test interference.
    struct IsolatedRoot {
        _env: crate::config::TestEnvVarGuard,
        _dir: tempfile::TempDir,
    }

    impl IsolatedRoot {
        fn new() -> Self {
            let dir = tempfile::tempdir().expect("isolated runtime root");
            let env = crate::config::TestEnvVarGuard::set_path("AGENTDESK_ROOT_DIR", dir.path());
            Self {
                _env: env,
                _dir: dir,
            }
        }

        fn path(&self) -> &Path {
            self._dir.path()
        }
    }

    /// Seed a CURRENT-generation durable frontier for `(provider, channel)` under
    /// the isolated root plus its matching `.generation` marker, and return the
    /// generation mtime the record was stamped with. The record's
    /// `generation_mtime_ns` is set to the marker's REAL on-disk mtime so the #1270
    /// generation gate TRUSTS it — mirroring the production write/read parity.
    #[cfg(unix)]
    fn seed_current_generation_frontier(
        provider: &ProviderKind,
        channel: ChannelId,
        tmux_session_name: &str,
        durable_end: u64,
    ) -> i64 {
        let gen_path =
            crate::services::tmux_common::session_temp_path(tmux_session_name, "generation");
        if let Some(parent) = Path::new(&gen_path).parent() {
            fs::create_dir_all(parent).expect("create sessions dir");
        }
        fs::write(&gen_path, b"1").expect("write generation marker");
        let gen_ns = current_generation_mtime_ns(tmux_session_name);
        assert_ne!(
            gen_ns, 0,
            "seeded .generation marker must have a readable mtime"
        );
        let record_path =
            delivery_record_path(provider, channel.get()).expect("env-resolved record path");
        write_delivered_frontier_at(
            &record_path,
            DeliveredCommit {
                range: (0, durable_end),
                generation_mtime_ns: gen_ns,
                attempts: 1,
                panel_msg_id: Some(1),
                panel_channel_id: None,
            },
        )
        .expect("seed durable frontier");
        gen_ns
    }

    #[cfg(unix)]
    fn seed_edit_failure_transcript(
        root: &IsolatedRoot,
        tmux_session_name: &str,
        generation_mtime_ns: i64,
        eof: u64,
    ) -> EditFailureTranscriptIdentity {
        let output_path = root.path().join(format!("{tmux_session_name}.jsonl"));
        fs::write(&output_path, vec![b'x'; eof as usize]).expect("seed transcript");
        EditFailureTranscriptIdentity {
            output_path,
            generation_mtime_ns,
        }
    }

    #[cfg(unix)]
    fn stable_edit_failure_committed_at(
        provider: &ProviderKind,
        channel: ChannelId,
        expected: &EditFailureTranscriptIdentity,
        range_end: u64,
    ) -> bool {
        let record_path =
            delivery_record_path(provider, channel.get()).expect("delivery record path");
        let frontier = stable_edit_failure_frontier_at(
            &record_path,
            expected,
            || Some(expected.output_path.clone()),
            || expected.generation_mtime_ns,
            || {},
        );
        range_already_committed(
            range_end,
            frontier.map(|frontier| frontier.range.1).unwrap_or(0),
        )
    }

    #[cfg(unix)]
    fn shared_with_committed(
        channel: ChannelId,
        in_memory: u64,
    ) -> std::sync::Arc<crate::services::discord::SharedData> {
        let shared = crate::services::discord::make_shared_data_for_tests();
        shared
            .tmux_relay_coord(channel)
            .confirmed_end_offset
            .store(in_memory, Ordering::Release);
        shared
    }

    /// authority-ON fuses the CURRENT-generation durable frontier into the dedup
    /// floor (`max(durable, in_memory)`) through the ENV-RESOLVED public reader,
    /// while authority-OFF returns the in-memory value verbatim (deploy no-op).
    /// This proves the flag actually gates the wiring — not just the pure `fuse`
    /// arithmetic already covered above.
    #[cfg(unix)]
    #[test]
    fn effective_committed_offset_authority_on_fuses_durable_3933() {
        let _root = IsolatedRoot::new();
        let provider = ProviderKind::Claude;
        let channel = ChannelId::new(39_330_401);
        let tmux = "AgentDesk-claude-3933fuse";
        let durable_end = 443_154_u64;
        let in_memory = 100_u64;
        seed_current_generation_frontier(&provider, channel, tmux, durable_end);
        let shared = shared_with_committed(channel, in_memory);

        {
            // authority ON → durable frontier RAISES the floor above in-memory.
            let _authority = authority_test_seam::force(true);
            assert_eq!(
                effective_committed_offset(
                    shared.as_ref(),
                    &provider,
                    channel,
                    tmux,
                    Some(u64::MAX)
                ),
                durable_end.max(in_memory),
            );
            // `committed_floor_for_resend_dedup` = effective.max(flag-independent
            // durable) → the same fused value (floor never drops below in-memory).
            assert_eq!(
                committed_floor_for_resend_dedup(
                    shared.as_ref(),
                    &provider,
                    channel,
                    tmux,
                    Some(u64::MAX)
                ),
                durable_end.max(in_memory),
            );
        }
        {
            // authority OFF (forced) → in-memory verbatim; the durable frontier is
            // hidden by the flag. Pins the flag-gated branch.
            let _authority = authority_test_seam::force(false);
            assert_eq!(
                effective_committed_offset(
                    shared.as_ref(),
                    &provider,
                    channel,
                    tmux,
                    Some(u64::MAX)
                ),
                in_memory,
            );
        }
    }

    /// #3871 (rollover dup relay): after a JSONL rollover a re-observing pass would
    /// re-relay the frozen PREFIX range (ends BELOW the durable committed floor →
    /// already delivered → suppressed), while a genuinely-NEW tail produced after
    /// the rollover ends ABOVE the floor → NOT suppressed → relayed. The floor is
    /// `max(durable, in_memory)`, so raising it to the known-delivered watermark
    /// never over-suppresses fresh output. Rides the in-memory=0 restart hazard.
    #[cfg(unix)]
    #[test]
    fn committed_floor_authority_on_does_not_oversuppress_rollover_resend_3871() {
        let _root = IsolatedRoot::new();
        let provider = ProviderKind::Claude;
        let channel = ChannelId::new(39_330_402);
        let tmux = "AgentDesk-claude-3871";
        let durable_end = 443_154_u64;
        seed_current_generation_frontier(&provider, channel, tmux, durable_end);
        // In-memory reset to 0 (the restart / synthetic-resume hazard #3871 rides).
        let shared = shared_with_committed(channel, 0);

        let _authority = authority_test_seam::force(true);
        let floor = committed_floor_for_resend_dedup(
            shared.as_ref(),
            &provider,
            channel,
            tmux,
            Some(u64::MAX),
        );
        assert_eq!(
            floor, durable_end,
            "durable frontier must lift the reset in-memory floor"
        );
        // Frozen prefix already delivered → suppressed (no dup relay).
        assert!(range_already_committed(422_855, floor));
        // New tail past the durable high watermark → relayed (no over-suppression).
        assert!(!range_already_committed(443_500, floor));
    }

    /// #3885 (no-response watchdog re-relay): the streaming-aware watchdog can
    /// trigger a re-observing pass over an ALREADY-delivered range. Under
    /// authority-ON the durable frontier makes the committed floor recognize that
    /// range as delivered (`range_already_committed == true`) → the watchdog
    /// re-relay is suppressed (no duplicate) even though the in-memory offset was
    /// reset. The boundary (range_end == floor) is inclusive.
    #[cfg(unix)]
    #[test]
    fn committed_floor_authority_on_suppresses_watchdog_rerelay_3885() {
        let _root = IsolatedRoot::new();
        let provider = ProviderKind::Codex;
        let channel = ChannelId::new(39_330_403);
        let tmux = "AgentDesk-codex-3885";
        let durable_end = 512_000_u64;
        seed_current_generation_frontier(&provider, channel, tmux, durable_end);
        let shared = shared_with_committed(channel, 0); // watchdog fires post in-memory reset

        let _authority = authority_test_seam::force(true);
        let floor = committed_floor_for_resend_dedup(
            shared.as_ref(),
            &provider,
            channel,
            tmux,
            Some(u64::MAX),
        );
        assert_eq!(floor, durable_end);
        // Watchdog re-relay of the delivered body → suppressed (dup guard).
        assert!(range_already_committed(500_000, floor));
        assert!(range_already_committed(durable_end, floor)); // inclusive boundary
    }

    #[cfg(unix)]
    #[test]
    fn edit_failure_recheck_suppresses_only_stable_fresh_bounded_commit_4508() {
        let root = IsolatedRoot::new();
        let provider = ProviderKind::Claude;
        let channel = ChannelId::new(45_080_001);
        let tmux = "AgentDesk-claude-4508";
        let committed_end = 900_u64;
        let generation = seed_current_generation_frontier(&provider, channel, tmux, committed_end);
        let expected = seed_edit_failure_transcript(&root, tmux, generation, committed_end);

        assert!(stable_edit_failure_committed_at(
            &provider,
            channel,
            &expected,
            committed_end,
        ));
        assert!(!stable_edit_failure_committed_at(
            &provider,
            channel,
            &expected,
            committed_end + 1,
        ));

        fs::write(
            &expected.output_path,
            vec![b'x'; (committed_end - 1) as usize],
        )
        .expect("truncate transcript");
        assert!(!stable_edit_failure_committed_at(
            &provider,
            channel,
            &expected,
            committed_end,
        ));
    }

    #[cfg(unix)]
    #[test]
    fn edit_failure_recheck_distrusts_prior_generation_4508() {
        let root = IsolatedRoot::new();
        let provider = ProviderKind::Claude;
        let channel = ChannelId::new(45_080_002);
        let tmux = "AgentDesk-claude-4508-stale";
        let committed_end = 900_u64;
        let generation = seed_current_generation_frontier(&provider, channel, tmux, committed_end);
        let expected = seed_edit_failure_transcript(&root, tmux, generation, committed_end);
        let record_path = delivery_record_path(&provider, channel.get()).unwrap();
        write_delivered_frontier_at(
            &record_path,
            DeliveredCommit {
                range: (0, committed_end),
                generation_mtime_ns: generation.saturating_add(1),
                attempts: 1,
                panel_msg_id: None,
                panel_channel_id: None,
            },
        )
        .unwrap();

        assert!(!stable_edit_failure_committed_at(
            &provider,
            channel,
            &expected,
            committed_end,
        ));
    }

    #[cfg(unix)]
    #[test]
    fn edit_failure_recheck_requires_durable_proof_despite_high_memory_floor_4508() {
        let root = IsolatedRoot::new();
        let provider = ProviderKind::Claude;
        let channel = ChannelId::new(45_080_003);
        let tmux = "AgentDesk-claude-4508-unverifiable";
        let committed_end = 900_u64;
        let shared = shared_with_committed(channel, committed_end);

        assert!(!range_committed_after_edit_failure(
            shared.as_ref(),
            &provider,
            channel,
            tmux,
            None,
            committed_end,
        ));

        let generation = seed_current_generation_frontier(&provider, channel, tmux, committed_end);
        let missing_output = EditFailureTranscriptIdentity {
            output_path: root.path().join("missing.jsonl"),
            generation_mtime_ns: generation,
        };
        assert!(!stable_edit_failure_committed_at(
            &provider,
            channel,
            &missing_output,
            committed_end,
        ));
    }

    #[cfg(unix)]
    #[test]
    fn edit_failure_recheck_detects_rotate_between_eof_and_frontier_4508() {
        let root = IsolatedRoot::new();
        let provider = ProviderKind::Claude;
        let channel = ChannelId::new(45_080_004);
        let tmux = "AgentDesk-claude-4508-rotate";
        let committed_end = 900_u64;
        let old_generation =
            seed_current_generation_frontier(&provider, channel, tmux, committed_end);
        let expected = seed_edit_failure_transcript(&root, tmux, old_generation, committed_end);
        let new_path = root.path().join("replacement.jsonl");
        fs::write(&new_path, vec![b'n'; committed_end as usize]).expect("seed replacement");
        let record_path = delivery_record_path(&provider, channel.get()).unwrap();
        let current_path = std::cell::RefCell::new(expected.output_path.clone());
        let current_generation = std::cell::Cell::new(old_generation);

        let frontier = stable_edit_failure_frontier_at(
            &record_path,
            &expected,
            || Some(current_path.borrow().clone()),
            || current_generation.get(),
            || {
                current_path.replace(new_path.clone());
                current_generation.set(old_generation.saturating_add(1));
                write_record_at(
                    &record_path,
                    &DeliveryRecord {
                        delivery_lease: None,
                        delivered_frontier: Some(DeliveredCommit {
                            range: (0, committed_end),
                            generation_mtime_ns: old_generation.saturating_add(1),
                            attempts: 1,
                            panel_msg_id: None,
                            panel_channel_id: None,
                        }),
                        recent_delivered_contents: Vec::new(),
                        confirmed_deliveries: Vec::new(),
                    },
                )
                .expect("seed unrelated new frontier");
            },
        );
        assert!(
            frontier.is_none(),
            "wrapper rotation after EOF sampling must fail open for fallback delivery"
        );
    }

    /// Same-name transcript truncate/replace WITHOUT a generation-marker bump:
    /// the durable frontier still carries the expected generation and stays
    /// within the pre-truncate EOF, so ONLY the post-read file-identity check
    /// can detect the race. Mutation-sensitive for the second snapshot
    /// comparison in `stable_edit_failure_frontier_at`.
    #[cfg(unix)]
    #[test]
    fn edit_failure_recheck_detects_same_generation_truncate_during_snapshot_4508() {
        let root = IsolatedRoot::new();
        let provider = ProviderKind::Claude;
        let channel = ChannelId::new(45_080_005);
        let tmux = "AgentDesk-claude-4508-truncate-race";
        let committed_end = 900_u64;
        let generation = seed_current_generation_frontier(&provider, channel, tmux, committed_end);
        let expected = seed_edit_failure_transcript(&root, tmux, generation, committed_end);
        let record_path = delivery_record_path(&provider, channel.get()).unwrap();

        let frontier = stable_edit_failure_frontier_at(
            &record_path,
            &expected,
            || Some(expected.output_path.clone()),
            || expected.generation_mtime_ns,
            || {
                fs::write(
                    &expected.output_path,
                    vec![b't'; (committed_end - 1) as usize],
                )
                .expect("truncate transcript during snapshot");
            },
        );
        assert!(
            frontier.is_none(),
            "same-generation transcript truncation during the snapshot must fail open"
        );
    }

    /// Even with authority ON, a STALE prior-generation durable frontier is
    /// distrusted (#1270 gate) → it does NOT raise the floor → a genuinely-new
    /// answer after a pane reset / same-named respawn is never over-suppressed.
    /// Proves the generation gate is honored through the ENV-RESOLVED wiring, not
    /// only in the pure helper.
    #[cfg(unix)]
    #[test]
    fn effective_committed_offset_authority_on_distrusts_stale_generation_3933() {
        let _root = IsolatedRoot::new();
        let provider = ProviderKind::Claude;
        let channel = ChannelId::new(39_330_404);
        let tmux = "AgentDesk-claude-3933stale";
        // Seed marker + a current-gen frontier, then OVERWRITE the record with a
        // PRIOR-generation stamp (mtime 1 ≠ the marker's real nanosecond mtime).
        seed_current_generation_frontier(&provider, channel, tmux, 443_154);
        let record_path = delivery_record_path(&provider, channel.get()).unwrap();
        write_delivered_frontier_at(
            &record_path,
            DeliveredCommit {
                range: (0, 443_154),
                generation_mtime_ns: 1, // PRIOR generation → distrusted
                attempts: 1,
                panel_msg_id: None,
                panel_channel_id: None,
            },
        )
        .unwrap();
        let shared = shared_with_committed(channel, 0);

        let _authority = authority_test_seam::force(true);
        // Stale frontier distrusted → floor stays at the in-memory value (0).
        assert_eq!(
            effective_committed_offset(shared.as_ref(), &provider, channel, tmux, Some(u64::MAX)),
            0,
        );
        assert_eq!(
            committed_floor_for_resend_dedup(
                shared.as_ref(),
                &provider,
                channel,
                tmux,
                Some(u64::MAX)
            ),
            0,
        );
        // A fresh answer above 0 is NOT over-suppressed.
        assert!(!range_already_committed(422_855, 0));
    }

    /// One channel whose delivery-channel row is `user`'s episode `"ep-n"`,
    /// spanning `(start, 64)` under a current generation.
    #[cfg(unix)]
    fn nonce_provenance_fixture(
        channel: u64,
        user: u64,
        start: u64,
    ) -> (
        std::sync::Arc<crate::services::discord::SharedData>,
        &'static str,
        i64,
    ) {
        let tmux = "AgentDesk-claude-6035-nonce";
        let generation = set_phase_a_generation(tmux, 1_700_603_500);
        #[rustfmt::skip]
        let mut row = crate::services::discord::inflight::InflightTurnState::new(
            ProviderKind::Claude, channel, None, 1, user, 0, "q".into(),
            None, Some(tmux.into()), None, None, 0);
        (row.turn_start_offset, row.last_offset) = (Some(start), 64);
        row.turn_nonce = Some("ep-n".into());
        crate::services::discord::inflight::save_inflight_state(&row).expect("save row");
        let shared = crate::services::discord::make_shared_data_for_tests();
        let coord = shared.tmux_relay_coord(ChannelId::new(channel));
        coord
            .confirmed_end_generation_mtime_ns
            .store(generation, Ordering::Release);
        coord.confirmed_end_offset.store(64, Ordering::Release);
        (shared, tmux, generation)
    }

    #[cfg(unix)]
    fn ledger_rows(channel: u64) -> Vec<(u64, Option<String>)> {
        let ledger = completed_turn_ledger::read_ledger(&ProviderKind::Claude, channel);
        let entries = ledger.map(|ledger| ledger.entries).unwrap_or_default();
        let rows = entries.into_iter();
        rows.map(|entry| (entry.user_msg_id, entry.turn_nonce))
            .collect()
    }

    #[cfg(unix)]
    fn bridge_delivery(shared: &crate::services::discord::SharedData, channel: u64, pin: u64) {
        let tmux = Some("AgentDesk-claude-6035-nonce");
        let (id, anchor) = (ChannelId::new(channel), Some(channel));
        #[rustfmt::skip]
        shadow_mirror_delivered_frontier(shared, &ProviderKind::Claude, id, tmux, (0, 64),
            true, Some(603_599), anchor, Some("answer"), Some(pin));
    }

    /// The exact receipt pins the fresh row to this range, so the row's own id
    /// is recorded with its episode nonce, bridge and watcher alike.
    #[cfg(unix)]
    #[test]
    fn exact_receipt_row_records_its_episode_nonce_6035() {
        let _root = IsolatedRoot::new();
        let (bridge, watcher, user) = (6_035_301, 6_035_302, 6_035_300);
        let (shared, _, _) = nonce_provenance_fixture(bridge, user, 0);
        bridge_delivery(&shared, bridge, user);
        assert_eq!(ledger_rows(bridge), [(user, Some("ep-n".to_string()))]);

        let (shared, tmux, generation_mtime_ns) = nonce_provenance_fixture(watcher, user, 0);
        let channel = ChannelId::new(watcher);
        let authority = WatcherDeliveryRecordAuthority {
            lease_reset_incarnation: shared.relay_frontier_token(channel).reset_incarnation,
            generation_mtime_ns,
            ledger_user_msg_id: Some(user),
        };
        #[rustfmt::skip]
        assert!(record_watcher_terminal_delivery(&shared, &ProviderKind::Claude, channel, tmux,
            authority, (0, 64), Some(603_598), "answer"));
        assert_eq!(ledger_rows(watcher), [(user, Some("ep-n".to_string()))]);
    }

    /// Without exact same-episode provenance the nonce is `None`: another turn's pin,
    /// a row on another range, or an unknown generation. A pinned source proves its nonce.
    #[cfg(unix)]
    #[test]
    fn unproven_provenance_records_no_nonce_6035() {
        let _root = IsolatedRoot::new();
        let (other_pin, stale_range, unknown, sink) = (6_035_311, 6_035_312, 6_035_313, 6_035_314);
        let (user, pinned) = (6_035_310, 6_035_319);
        let (shared, _, _) = nonce_provenance_fixture(other_pin, user, 0);
        bridge_delivery(&shared, other_pin, pinned);
        assert_eq!(
            ledger_rows(other_pin),
            [(pinned, None)],
            "pinned id is not the row's"
        );

        let (shared, _, _) = nonce_provenance_fixture(stale_range, user, 32);
        bridge_delivery(&shared, stale_range, user);
        assert_eq!(
            ledger_rows(stale_range),
            [(user, None)],
            "row is a later episode"
        );

        let (shared, _, _) = nonce_provenance_fixture(unknown, user, 0);
        let coord = shared.tmux_relay_coord(ChannelId::new(unknown));
        coord
            .confirmed_end_generation_mtime_ns
            .store(0, Ordering::Release);
        bridge_delivery(&shared, unknown, user);
        assert_eq!(ledger_rows(unknown), [(user, None)], "unknown generation");

        let (_, tmux, generation) = nonce_provenance_fixture(sink, user, 0);
        #[rustfmt::skip]
        let (_, receipt) = exact_delivery_fixture(&ProviderKind::Claude, tmux, "ep-n", (0, 64),
            generation, (sink, sink), 603_597);
        record_pinned_delivery_metadata(&receipt.source, "answer", user);
        assert_eq!(
            ledger_rows(sink),
            [(user, Some("ep-n".to_string()))],
            "pinned source"
        );
    }
}
