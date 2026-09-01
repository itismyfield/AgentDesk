//! Transport execution, failure classification, and final commit.

use super::*;

pub(super) async fn commit_and_finalize<G, L>(
    gateway: &G,
    ctx: &TurnOutputCtx<'_, L>,
    start: u64,
    end: u64,
    lease_guard: Option<&mut ControllerLeaseGuard<'_, L>>,
    replace_kind: Option<ReplaceDeliveryKind>,
    new_chunks: Option<NewChunksDelivery>,
) -> DeliveryOutcome
where
    G: TurnGateway + ?Sized,
    L: DeliveryLease + ?Sized,
{
    // A2a capability 2: delegate the advance decision. `None` → A1 semantics
    // (unconditional advance: owners with no identity gate). `Some(cb)` → the
    // owner's identity gate runs HERE, synchronously, BEFORE the post-send awaits
    // below, so a post-send await can never land before the advance (#3143).
    // This mirrors the sink running `advance_after_confirmed_post` inline before
    // the marker clears (`session_relay_sink.rs:560-577`).
    let advanced = match ctx.advance {
        Some(advance) => advance((start, end)),
        None => true,
    };

    // commit() verifies the full (holder, key, range) identity and records the
    // outcome. On the advanced arm the committed frontier moves to `end`; on the
    // refused arm we commit `NotDelivered` so the owner's committed-offset
    // reconciliation re-sends (the sink's `advanced == false` arm). On the
    // markerless path there is no lease to commit — the advance bool alone
    // decides the outcome. Runs synchronously here, BEFORE the post-send awaits.
    if let Some(guard) = lease_guard.as_ref() {
        let outcome = if advanced {
            LeaseOutcome::Delivered
        } else {
            LeaseOutcome::NotDelivered
        };
        let committed = ctx
            .lease
            .commit(ctx.holder, guard.lease_key(), start, end, outcome);
        debug_assert!(committed, "confirmed commit must match the acquired lease");
    }

    // ---- post-send work (AFTER the inline commit) -----------------------
    post_send_finalize(gateway, ctx).await;
    if let Some(guard) = lease_guard {
        // I1 release ordering: AFTER the post-send finalize. Disarms so the
        // guard's Drop (cancel/panic safety net) cannot double-release.
        guard.release_and_disarm();
    }

    if advanced {
        DeliveryOutcome::Delivered {
            committed_to: end,
            // Surface the replace identity so the owner mirrors the legacy
            // per-variant cleanup (#3089 A4 r2). `None` on a refused advance
            // (`NotDelivered` has no replace identity to carry) and for non-replace
            // plans. The advance refusal drops `replace_kind` on the floor because
            // the body was NOT committed — there is no delivered original to footer.
            replace_kind,
            new_chunks,
        }
    } else {
        DeliveryOutcome::NotDelivered {
            committed_from: start,
        }
    }
}

/// Commit a clean non-delivery result for rollback-aware long-chunk sends. This
/// mirrors the legacy terminal long-chunk arms: failed send ⇒ commit
/// `NotDelivered`, do not advance, do not delete the placeholder anchor.
pub(super) fn commit_not_delivered_and_release<L>(
    ctx: &TurnOutputCtx<'_, L>,
    start: u64,
    end: u64,
    lease_guard: Option<&mut ControllerLeaseGuard<'_, L>>,
) -> DeliveryOutcome
where
    L: DeliveryLease + ?Sized,
{
    if let Some(guard) = lease_guard.as_ref() {
        let committed = ctx.lease.commit(
            ctx.holder,
            guard.lease_key(),
            start,
            end,
            LeaseOutcome::NotDelivered,
        );
        debug_assert!(
            committed,
            "confirmed NotDelivered commit must match the acquired lease"
        );
    }
    if let Some(guard) = lease_guard {
        guard.release_and_disarm();
    }
    DeliveryOutcome::NotDelivered {
        committed_from: start,
    }
}

/// Internal three-way transport result, before any lease commit.
///
/// A1's conservative classifier only ever produced `Delivered`/`Unknown`; the
/// extra arms are used by owners that bring a real transport-error taxonomy.
#[allow(dead_code)] // #3089 A1: Transient arm dormant until A2 transport taxonomy.
pub(super) enum TransportResult {
    /// Confirmed delivered. `Option<ReplaceDeliveryKind>` carries the replace
    /// identity (edit-in-place vs fresh fallback) for `Replace` plans so the
    /// owner write-back can mirror the legacy per-variant cleanup (#3089 A4 r2);
    /// `None` for `NewSend` / chunked / NoOp. `new_chunks` carries the tail
    /// chunk + anchor-delete metadata for confirmed long-chunk sends.
    Delivered {
        replace_kind: Option<ReplaceDeliveryKind>,
        new_chunks: Option<NewChunksDelivery>,
    },
    /// Clean non-delivery for rollback-aware long-chunk sends. The owner wants a
    /// committed `NotDelivered` lease result (retryable, no advance), not an
    /// ambiguous `Unknown`.
    NotDelivered,
    Transient,
    PermanentFailure,
    /// Ambiguous, never advance (I2). `fell_back` (#3089 A5): see
    /// [`DeliveryOutcome::Unknown`] — true only on NoCommitOnFallback fresh-fallback.
    Unknown {
        fell_back: bool,
    },
    /// The edit failed, then a fresh authority check proved the range had already
    /// committed. No POST occurred; commit/advance must remain untouched.
    AlreadyCommittedAfterEditFailure {
        edit_error: String,
    },
}

/// Drive the gateway transport for the plan. Returns ONLY the transport
/// outcome — it never touches the lease, so the inline commit in the caller is
/// the single advance authority (I1).
pub(super) async fn drive_transport<G, L>(
    gateway: &G,
    ctx: &TurnOutputCtx<'_, L>,
    chunk_count: usize,
    fallback_revalidation: Option<&(dyn Fn() -> bool + Send + Sync)>,
) -> TransportResult
where
    G: TurnGateway + ?Sized,
    L: DeliveryLease + ?Sized,
{
    match (&ctx.plan, &ctx.placeholder) {
        (OutputPlan::Replace { .. }, PlaceholderSlot::Active { message_id, .. }) => {
            match gateway
                .replace_message_deferred(ctx.channel_id, *message_id, ctx.body)
                .await
            {
                Ok(crate::services::discord::formatting::DeferredReplaceLongMessageOutcome::Edited(
                    outcome,
                )) => classify_replace_outcome(&outcome, &ctx.fallback_commit_policy),
                Ok(crate::services::discord::formatting::DeferredReplaceLongMessageOutcome::EditFailed {
                    edit_error,
                }) => {
                    if fallback_revalidation.is_some_and(|revalidate| revalidate()) {
                        TransportResult::AlreadyCommittedAfterEditFailure { edit_error }
                    } else {
                        match gateway
                            .send_long_message_with_rollback(ctx.channel_id, *message_id, ctx.body)
                            .await
                        {
                            Ok(message_ids) => classify_replace_outcome(
                                &crate::services::discord::formatting::ReplaceLongMessageOutcome::SentFallbackAfterEditFailure {
                                    edit_error,
                                    replacement_anchor: message_ids.first().copied(),
                                },
                                &ctx.fallback_commit_policy,
                            ),
                            Err(error) => classify_transport_failure(ctx, &error),
                        }
                    }
                }
                Err(error) => classify_transport_failure(ctx, &error),
            }
        }
        // Replace requested but no live placeholder to edit → fall back to a
        // fresh send of the single inline body. No replace identity to surface
        // (there was no original placeholder to edit) → `None`.
        (OutputPlan::Replace { .. }, PlaceholderSlot::None) => {
            match gateway.send_message(ctx.channel_id, ctx.body).await {
                Ok(_) => TransportResult::Delivered {
                    replace_kind: None,
                    new_chunks: None,
                },
                Err(error) => classify_transport_failure(ctx, &error),
            }
        }
        (OutputPlan::SendNewChunks { delete_anchor, .. }, slot) => {
            let anchor = match slot {
                PlaceholderSlot::Active { message_id, .. } => *message_id,
                PlaceholderSlot::None => MessageId::new(1),
            };
            match gateway
                .send_long_message_with_rollback(ctx.channel_id, anchor, ctx.body)
                .await
            {
                // A Split body MUST land all `chunk_count` messages to be
                // Delivered. A short write (fewer IDs than chunks) is a PARTIAL
                // send — ambiguous — and must NEVER advance (I2, review-fix H1).
                // `chunk_count` is always >= 1 (exact-or-more contract). Chunked
                // sends carry no replace identity → `None`.
                Ok(ids) if ids.len() >= chunk_count => {
                    let anchor_delete_error = if *delete_anchor {
                        delete_active_anchor_after_chunks(gateway, ctx, slot).await
                    } else {
                        None
                    };
                    TransportResult::Delivered {
                        replace_kind: None,
                        new_chunks: Some(NewChunksDelivery {
                            first_message_id: ids.first().copied(),
                            tail_message_id: ids.last().copied(),
                            anchor_delete_error,
                        }),
                    }
                }
                // Short chunked write: ambiguous, nothing fell back (#3089 A5).
                Ok(_) => TransportResult::Unknown { fell_back: false },
                Err(_) if *delete_anchor => TransportResult::NotDelivered,
                Err(error) => classify_transport_failure(ctx, &error),
            }
        }
        (OutputPlan::SendFresh { .. }, _) => {
            unreachable!("fresh-send transport is owned by the child module")
        }
        (OutputPlan::NoOp, _) => TransportResult::Delivered {
            replace_kind: None,
            new_chunks: None,
        },
    }
}

pub(super) async fn delete_active_anchor_after_chunks<G, L>(
    gateway: &G,
    ctx: &TurnOutputCtx<'_, L>,
    slot: &PlaceholderSlot,
) -> Option<String>
where
    G: TurnGateway + ?Sized,
    L: DeliveryLease + ?Sized,
{
    if let PlaceholderSlot::Active { message_id, .. } = slot {
        if let Err(error) = gateway.delete_message(ctx.channel_id, *message_id).await {
            tracing::warn!(
                channel_id = ctx.channel_id.get(),
                message_id = message_id.get(),
                error = %error,
                "long chunk delivery succeeded but anchor delete failed; proceeding as delivered"
            );
            return Some(error);
        }
    }
    None
}

/// Map a `replace_message_with_outcome` success into the controller's transport
/// classification, mirroring the EXACT semantics each owner gives each
/// `ReplaceLongMessageOutcome` variant (review-fix H2 — the catch-all
/// `Ok(_) => Delivered` was wrong: `PartialContinuationFailure` never advances).
///
/// Owner-mapping evidence:
/// - `EditedOriginal` → delivered for EVERY owner:
///   `session_relay_sink.rs:863`, `standby_relay.rs:653`,
///   `turn_bridge/terminal_delivery.rs:131` (committed = true) + predicate `:42`,
///   `formatting.rs:1785` (`Ok(())`).
/// - `SentFallbackAfterEditFailure` → owner-SPECIFIC (review-fix H1 r3): the sink
///   advances (`session_relay_sink.rs:905`) and standby advances
///   (`standby_relay.rs:662`), but turn_bridge does NOT
///   (`terminal_delivery.rs:143` returns `committed = false`; predicate `:42`
///   commits `EditedOriginal` only). The controller consults the owner-passed
///   `FallbackCommitPolicy`: `CommitOnFallback` → `Delivered`,
///   `NoCommitOnFallback` → `Unknown { fell_back: true }` (#3089 A5).
/// - `PartialContinuationFailure` → ambiguous, NEVER advance (I2):
///   `session_relay_sink.rs:956`, `standby_relay.rs:678`,
///   `turn_bridge/terminal_delivery.rs:155` (committed = false), `formatting.rs:1787`.
pub(super) fn classify_replace_outcome(
    outcome: &crate::services::discord::formatting::ReplaceLongMessageOutcome,
    fallback_commit_policy: &FallbackCommitPolicy,
) -> TransportResult {
    use crate::services::discord::formatting::ReplaceLongMessageOutcome;
    match outcome {
        // Edited in place → carry the `EditedOriginal` replace identity so the
        // owner takes its delivered side-effects (footer register, Succeeded).
        ReplaceLongMessageOutcome::EditedOriginal => TransportResult::Delivered {
            replace_kind: Some(ReplaceDeliveryKind::EditedOriginal),
            new_chunks: None,
        },
        // Owner-specific (H1 r3): the edit failed but a fallback POST carried the
        // body. Honour the owner's `FallbackCommitPolicy` (sink/standby advance;
        // turn_bridge does not). On the committing arm carry the
        // `FreshFallbackAfterEditFailure { edit_error, replacement_anchor }`
        // identity (#3089 A4 r2 + D1) so the watcher mirrors the legacy fallback
        // cleanup and recovery can durably bind a stale-anchor fallback POST.
        ReplaceLongMessageOutcome::SentFallbackAfterEditFailure {
            edit_error,
            replacement_anchor,
        } => {
            match fallback_commit_policy {
                FallbackCommitPolicy::CommitOnFallback => TransportResult::Delivered {
                    replace_kind: Some(ReplaceDeliveryKind::FreshFallbackAfterEditFailure {
                        edit_error: edit_error.clone(),
                        replacement_anchor: *replacement_anchor,
                    }),
                    new_chunks: None,
                },
                // #3089 A5: edit FAILED but fallback POST landed the body → no
                // advance + `fell_back = true` (see `DeliveryOutcome::Unknown`).
                FallbackCommitPolicy::NoCommitOnFallback => {
                    TransportResult::Unknown { fell_back: true }
                }
            }
        }
        // Partial continuation failure: never advance (I2); `fell_back = false`
        // (nothing landed → no bump, #3089 A5).
        ReplaceLongMessageOutcome::PartialContinuationFailure { .. } => {
            TransportResult::Unknown { fell_back: false }
        }
    }
}

/// Classify a transport error into the ambiguous halves. A1 keeps the rule
/// conservative (design I3): anything we cannot prove transient is `Unknown` so
/// the offset never advances (the edit-fail policy only affects post-send cleanup).
pub(super) fn classify_transport_failure<L: DeliveryLease + ?Sized>(
    ctx: &TurnOutputCtx<'_, L>,
    error: &str,
) -> TransportResult {
    let class = classify_watcher_send_failure_message(error);
    if ctx.owner == RelayOwnerKind::Watcher
        && matches!(
            class,
            WatcherSendFailureClass::Permanent | WatcherSendFailureClass::RollbackIncomplete
        )
    {
        let display_error = strip_watcher_send_failure_class_marker(error);
        tracing::warn!(
            channel_id = ctx.channel_id.get(),
            owner = ?ctx.owner,
            failure_class = class.as_str(),
            error = %display_error,
            "turn-output controller: permanent watcher transport failure will not retry"
        );
        return TransportResult::PermanentFailure;
    }
    // Unknown keeps the existing retry/no-advance owner behavior for transient
    // watcher transport failures and for non-watcher owners.
    TransportResult::Unknown { fell_back: false }
}

/// Post-send finalization: placeholder terminal transition + edit-fail
/// fallback cleanup. Runs ONLY after the inline commit (I1). Best-effort —
/// failures here never un-advance the already-committed offset.
///
/// This is an `async` step with a real post-send await
/// (`PlaceholderController.transition`, which internally awaits an
/// `edit_message`) — the very kind of await I1 forbids the commit from landing
/// AFTER. The controller calls it only once the inline commit above has already
/// advanced the offset, so this await can never re-open #3143.
///
/// Design §5 A1 ("Wires `PlaceholderController.transition`"): the card is driven
/// to its terminal state through the shared `PlaceholderController` FSM /
/// edit-coalescer, NOT a raw `edit_message`, so A2+ owners do not have to redo
/// this API. `EditFailPlaceholderPolicy` governs the #2757 fence on
/// `EditFailed`.
pub(super) async fn post_send_finalize<G, L>(gateway: &G, ctx: &TurnOutputCtx<'_, L>)
where
    G: TurnGateway + ?Sized,
    L: DeliveryLease + ?Sized,
{
    if let (OutputPlan::Replace { lifecycle }, PlaceholderSlot::Active { message_id, key }) =
        (&ctx.plan, &ctx.placeholder)
    {
        // Only terminal targets are valid `transition` inputs; a non-terminal
        // `lifecycle` (e.g. Active) is left untouched here.
        if !matches!(
            lifecycle,
            PlaceholderLifecycle::Completed
                | PlaceholderLifecycle::TimedOut
                | PlaceholderLifecycle::Aborted
        ) {
            return;
        }

        // Drive the card to its terminal state through the shared controller
        // FSM. `transition` performs the post-send PATCH (with the controller's
        // own bounded edit-retry) and reports the lifecycle-aware outcome.
        let outcome = ctx
            .placeholder_controller
            .transition(gateway, key.clone(), *lifecycle)
            .await;

        // Only a hard `EditFailed` (Discord PATCH attempted and failed) engages
        // the #2757 fence. `Edited` / `Coalesced` / `AlreadyTerminal` /
        // `Rejected` are all non-failure terminations (no live PATCH error), so
        // they never delete the original.
        if matches!(outcome, PlaceholderControllerOutcome::EditFailed) {
            match ctx.edit_fail_policy {
                EditFailPlaceholderPolicy::DeleteIfProvenStale => {
                    // Watcher's conditional-delete arm: the edit failed, so the
                    // original placeholder may be stale; delete it.
                    let _ = gateway.delete_message(ctx.channel_id, *message_id).await;
                }
                EditFailPlaceholderPolicy::PreserveAlways => {
                    // #2757: sink/standby preserve the original — a transient
                    // edit failure must never remove already-streamed body.
                }
            }
        }
    }
}
