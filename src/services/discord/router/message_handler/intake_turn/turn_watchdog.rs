use super::*;

/// #3837 decomposition: the per-turn watchdog spawn lifted verbatim from
/// `handle_text_message`. Behavior-preserving — computes the initial/ceiling
/// deadline, marks the cancel token async-managed, and spawns the
/// `text_turn_watchdog` observed task exactly as the inline `{ ... }` block did.
pub(super) fn spawn_text_turn_watchdog(
    cancel_token: &Arc<CancelToken>,
    shared: &Arc<SharedData>,
    http: &Arc<serenity::http::Http>,
    channel_id: ChannelId,
    provider: &ProviderKind,
    provider_label: &str,
) {
    let watchdog_token = cancel_token.clone();
    let watchdog_shared = shared.clone();
    let watchdog_http = http.clone();
    let timeout = crate::services::discord::turn_watchdog_timeout();

    // Set initial deadline. max_deadline tracks the farthest accepted
    // extension for alert context; it is no longer an absolute cap.
    let now_ms = chrono::Utc::now().timestamp_millis();
    let turn_started_ms = now_ms;
    // #3557 (A) Codex-review fix: the per-turn hard ceiling must bind the
    // INITIAL deadline, not only the auto-extend clamp. Previously the
    // initial deadline was always `now + turn_watchdog_timeout()` (6h), and
    // the auto-extend clamp could not lower it because the store is gated by
    // `new_dl > current_dl`. So a Codex turn (4h ceiling) was still cancelled
    // at 6h and the clamp warn never fired. Cap the initial deadline at the
    // provider ceiling here so the tighter Codex bound is honored end to end.
    let ceiling_deadline_ms =
        crate::services::discord::turn_hard_ceiling_deadline_ms(turn_started_ms, &provider);
    let proposed_initial_dl = now_ms + timeout.as_millis() as i64;
    let deadline_ms = std::cmp::min(proposed_initial_dl, ceiling_deadline_ms);
    let max_deadline_ms = deadline_ms;
    // When the ceiling already caps the initial deadline (e.g. Codex 4h <
    // 6h watchdog timeout) the auto-extend clamp warn below never fires
    // (its `current_dl < ceiling_ms` guard is false once the deadline is
    // parked at the ceiling), so surface the bound once here instead.
    if proposed_initial_dl > ceiling_deadline_ms {
        let ts = chrono::Local::now().format("%H:%M:%S");
        let ceiling_min = (ceiling_deadline_ms - now_ms) / 1000 / 60;
        tracing::warn!(
            "  [{ts}] ⛔ WATCHDOG: hard ceiling ({ceiling_min}m) caps initial deadline for channel {} (provider={}) — turn will be reconciled at the ceiling",
            channel_id,
            provider_label
        );
    }
    // claude-e rollout Phase 1 (counter-review round 3 with Codex):
    // mark this token as async-managed so the per-provider sync
    // watchdog (`enforce_watchdog_deadline` in `spawn_cancel_watchdog`)
    // stops short-circuiting on the deadline. The async loop below
    // owns deadline expiry at 30s cadence.
    watchdog_token.mark_async_managed();
    watchdog_token
        .watchdog_deadline_ms
        .store(deadline_ms, std::sync::atomic::Ordering::Relaxed);
    watchdog_token
        .watchdog_max_deadline_ms
        .store(max_deadline_ms, std::sync::atomic::Ordering::Relaxed);

    let watchdog_channel_id_num = channel_id.get();
    let watchdog_provider = provider.clone();
    crate::services::discord::task_supervisor::spawn_observed("text_turn_watchdog", async move {
        const CHECK_INTERVAL: std::time::Duration = std::time::Duration::from_secs(30);
        let mut last_deadlock_prealert_deadline_ms: Option<i64> = None;

        loop {
            tokio::time::sleep(CHECK_INTERVAL).await;

            // Exit early if the turn already completed/cancelled
            if watchdog_token
                .cancelled
                .load(std::sync::atomic::Ordering::Relaxed)
            {
                crate::services::discord::clear_watchdog_deadline_override(watchdog_channel_id_num)
                    .await;
                return;
            }

            // Check for API-based deadline extension
            if let Some(extension) =
                crate::services::discord::take_watchdog_deadline_override(watchdog_channel_id_num)
                    .await
            {
                let effective_deadline =
                    apply_watchdog_deadline_extension(&watchdog_token, extension);
                last_deadlock_prealert_deadline_ms = None;
                let ts = chrono::Local::now().format("%H:%M:%S");
                let remaining_min =
                    (effective_deadline - chrono::Utc::now().timestamp_millis()) / 1000 / 60;
                tracing::info!(
                    "  [{ts}] ⏰ WATCHDOG: deadline extended for channel {} — {remaining_min}m remaining",
                    channel_id
                );
            }

            // Auto-extend based on inflight updated_at: if inflight was updated recently
            // (within last 5 min), push deadline forward by the default timeout
            {
                let current_dl = watchdog_token
                    .watchdog_deadline_ms
                    .load(std::sync::atomic::Ordering::Relaxed);
                let now_ms_check = chrono::Utc::now().timestamp_millis();
                // Only auto-extend when close to deadline (within 2 minutes)
                if now_ms_check > current_dl - 120_000 {
                    if let Some(inflight) = crate::services::discord::inflight::load_inflight_state(
                        &watchdog_provider,
                        watchdog_channel_id_num,
                    ) {
                        if let Ok(updated) = chrono::NaiveDateTime::parse_from_str(
                            &inflight.updated_at,
                            "%Y-%m-%d %H:%M:%S",
                        ) {
                            let updated_ms = updated.and_utc().timestamp_millis();
                            let age_ms = now_ms_check - updated_ms;
                            // If inflight was updated within the last 5 minutes, auto-extend
                            if age_ms < 300_000 {
                                // #3557 (A): clamp the auto-extend so a turn
                                // that keeps inflight warm forever cannot push
                                // the deadline indefinitely. The hard ceiling
                                // is measured from turn start and is tighter
                                // for Codex (the 13125s outlier source).
                                let ceiling_ms =
                                    crate::services::discord::turn_hard_ceiling_deadline_ms(
                                        turn_started_ms,
                                        &watchdog_provider,
                                    );
                                let proposed_dl = now_ms_check + timeout.as_millis() as i64;
                                let (new_dl, clamped) =
                                    crate::services::discord::clamp_auto_extend_deadline_ms(
                                        proposed_dl,
                                        ceiling_ms,
                                    );
                                // Warn exactly once when the ceiling first bites:
                                // `current_dl < ceiling_ms` is only true before
                                // the deadline has been parked at the ceiling. On
                                // later ticks `current_dl == ceiling_ms` so this is
                                // false and the warn does not repeat.
                                if clamped && current_dl < ceiling_ms {
                                    let ts = chrono::Local::now().format("%H:%M:%S");
                                    tracing::warn!(
                                        "  [{ts}] ⛔ WATCHDOG: hard ceiling reached for channel {} — auto-extend clamped, turn will be reconciled at deadline",
                                        channel_id
                                    );
                                }
                                if new_dl > current_dl {
                                    watchdog_token
                                        .watchdog_deadline_ms
                                        .store(new_dl, std::sync::atomic::Ordering::Relaxed);
                                    watchdog_token.watchdog_max_deadline_ms.store(
                                        std::cmp::max(
                                            watchdog_token
                                                .watchdog_max_deadline_ms
                                                .load(std::sync::atomic::Ordering::Relaxed),
                                            new_dl,
                                        ),
                                        std::sync::atomic::Ordering::Relaxed,
                                    );
                                    last_deadlock_prealert_deadline_ms = None;
                                    let ts = chrono::Local::now().format("%H:%M:%S");
                                    let remaining_min = (new_dl - now_ms_check) / 1000 / 60;
                                    tracing::info!(
                                        "  [{ts}] ⏰ WATCHDOG: auto-extended for channel {} (inflight active) — {remaining_min}m remaining",
                                        channel_id
                                    );
                                }
                            }
                        }
                    }
                }
            }

            let current_deadline = watchdog_token
                .watchdog_deadline_ms
                .load(std::sync::atomic::Ordering::Relaxed);
            let now = chrono::Utc::now().timestamp_millis();
            if should_send_watchdog_deadlock_prealert(
                now,
                current_deadline,
                last_deadlock_prealert_deadline_ms,
            ) {
                let is_current_token =
                    crate::services::discord::mailbox_cancel_token(&watchdog_shared, channel_id)
                        .await
                        .is_some_and(|current| std::sync::Arc::ptr_eq(&watchdog_token, &current));
                if !is_current_token {
                    crate::services::discord::clear_watchdog_deadline_override(
                        watchdog_channel_id_num,
                    )
                    .await;
                    return;
                }
                let current_max_deadline = watchdog_token
                    .watchdog_max_deadline_ms
                    .load(std::sync::atomic::Ordering::Relaxed);
                if maybe_send_watchdog_deadlock_prealert(
                    &watchdog_shared,
                    &watchdog_provider,
                    channel_id,
                    now,
                    current_deadline,
                    turn_started_ms,
                    current_max_deadline,
                )
                .await
                {
                    last_deadlock_prealert_deadline_ms = Some(current_deadline);
                }
            }

            if let Some(extension) =
                crate::services::discord::take_watchdog_deadline_override(watchdog_channel_id_num)
                    .await
            {
                apply_watchdog_deadline_extension(&watchdog_token, extension);
                last_deadlock_prealert_deadline_ms = None;
            }
            let current_deadline = watchdog_token
                .watchdog_deadline_ms
                .load(std::sync::atomic::Ordering::Relaxed);
            let now = chrono::Utc::now().timestamp_millis();
            if now < current_deadline {
                continue; // Not yet — deadline may have been extended
            }

            // Deadline reached — fire watchdog through the cancel/reconcile path.
            let disposition = reconcile_watchdog_timeout(
                &watchdog_shared,
                &watchdog_provider,
                channel_id,
                &watchdog_token,
            )
            .await;
            if disposition == WatchdogTimeoutCancelDisposition::Cancelled {
                let elapsed_mins =
                    (now - (current_deadline - timeout.as_millis() as i64)) / 1000 / 60;
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::info!(
                    "  [{ts}] ⏰ WATCHDOG: turn timeout (~{elapsed_mins}m) for channel {}, reconciled via cancel path",
                    channel_id
                );

                // Notify Discord
                let has_queued = crate::services::discord::mailbox_has_pending_soft_queue(
                    &watchdog_shared,
                    &watchdog_provider,
                    channel_id,
                )
                .await
                .has_pending;
                let msg = if has_queued {
                    format!(
                        "⚠️ 턴이 {elapsed_mins}분 타임아웃으로 자동 중단되었습니다. 대기 중인 메시지로 다음 턴을 시작합니다.",
                    )
                } else {
                    format!("⚠️ 턴이 {elapsed_mins}분 타임아웃으로 자동 중단되었습니다.",)
                };
                let _ = channel_id.say(&watchdog_http, msg).await;
            }
            return; // Watchdog done regardless
        }
    });
}
