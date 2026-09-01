//! Session-bound terminal delivery orchestration.

use super::*;

impl SessionBoundDiscordRelaySink {
    pub(in crate::services::discord) fn new(health_registry: Arc<HealthRegistry>) -> Self {
        Self {
            health_registry,
            frames_total: AtomicU64::new(0),
            delivered_total: AtomicU64::new(0),
            by_session: Mutex::new(HashMap::new()),
            journal: journal::process_observer(),
            #[cfg(test)]
            lease_test_probe: None,
            #[cfg(test)]
            test_gateway: None,
            #[cfg(test)]
            test_replace_anchor: None,
            #[cfg(test)]
            test_delivery_outcomes: None,
            #[cfg(test)]
            test_force_legacy_replace: false,
        }
    }

    #[cfg(test)]
    pub(super) fn with_lease_test_probe(
        health_registry: Arc<HealthRegistry>,
        lease_test_probe: Arc<SinkLeaseTestProbe>,
    ) -> Self {
        let mut sink = Self::new(health_registry);
        sink.lease_test_probe = Some(lease_test_probe);
        sink
    }

    pub(super) fn ingest_frame(&self, frame: &StreamFrame) -> Vec<SessionRelayDelivery> {
        self.frames_total.fetch_add(1, Ordering::AcqRel);
        let Ok(mut sessions) = self.by_session.lock() else {
            return Vec::new();
        };
        sessions
            .entry(frame.session_name.clone())
            .or_default()
            .ingest_frame(frame)
    }

    /// Commit a confirmed idle/catch-up delivery in the frame's ordered JSONL
    /// coordinate space. The wrapper generation and current EOF are rechecked
    /// after transport before either durable or in-memory authority advances.
    pub(in crate::services::discord) fn advance_idle_range_after_confirmed_post(
        &self,
        shared: &super::super::SharedData,
        provider: &ProviderKind,
        channel_id: u64,
        session_name: &str,
        delivery: &SessionRelayDelivery,
    ) -> bool {
        let Some((start, end)) = delivery.relay_range.filter(|(start, end)| end > start) else {
            return false;
        };
        let Some(frame_generation) = delivery
            .relay_generation_mtime_ns
            .filter(|generation| *generation != 0)
        else {
            return false;
        };
        let current_generation = dr::current_generation_mtime_ns(session_name);
        let current_eof = idle_jsonl_current_eof(provider, session_name);
        if current_generation == 0
            || current_generation != frame_generation
            || current_eof.is_none_or(|eof| end > eof)
        {
            return false;
        }
        if !matches!(
            dr::commit_ordered_jsonl_range(
                provider,
                ChannelId::new(channel_id),
                session_name,
                (start, end),
                frame_generation,
            ),
            Ok(true)
        ) {
            return false;
        }
        super::super::tmux::advance_watcher_confirmed_end(
            shared,
            provider,
            ChannelId::new(channel_id),
            session_name,
            end,
            "src/services/discord/session_relay_sink.rs:idle_range_confirmed_advance",
        );
        dr::record_delivered_content_fingerprint(
            provider,
            ChannelId::new(channel_id),
            session_name,
            &delivery.response_text,
        );
        true
    }

    pub(in crate::services::discord) fn idle_range_already_committed_before_transport(
        &self,
        shared: &super::super::SharedData,
        provider: &ProviderKind,
        channel_id: u64,
        session_name: &str,
        delivery: &SessionRelayDelivery,
    ) -> bool {
        let Some((_, end)) = delivery.relay_range else {
            return false;
        };
        let Some(frame_generation) = delivery.relay_generation_mtime_ns else {
            return false;
        };
        let current_generation = dr::current_generation_mtime_ns(session_name);
        let current_eof = idle_jsonl_current_eof(provider, session_name);
        if frame_generation == 0 || current_generation != frame_generation || current_eof.is_none()
        {
            return false;
        }
        dr::effective_committed_offset(
            shared,
            provider,
            ChannelId::new(channel_id),
            session_name,
            current_eof,
        )
        .max(dr::delivered_frontier_end_current_generation(
            provider,
            ChannelId::new(channel_id),
            session_name,
            current_eof,
        )) >= end
    }

    pub(super) fn advance_after_confirmed_post(
        &self,
        shared: &super::super::SharedData,
        provider: &ProviderKind,
        channel_id: u64,
        session_name: &str,
        delivery: &SessionRelayDelivery,
        sink_lease_guard: Option<&SinkDeliveryLeaseGuard>,
    ) {
        let advanced = if delivery.relay_range.is_some() {
            self.advance_idle_range_after_confirmed_post(
                shared,
                provider,
                channel_id,
                session_name,
                delivery,
            )
        } else {
            let fresh_inflight = super::super::inflight::load_inflight_state(provider, channel_id);
            self.advance_offset_for_confirmed_delegated_terminal(
                shared,
                provider,
                channel_id,
                session_name,
                delivery,
                fresh_inflight.as_ref(),
            )
        };
        // #3151 CLEAR: advance committed FIRST, THEN commit the marker. #3159 BUG 1: the
        // commit outcome MUST reflect whether the advance fired — see
        // `SinkDeliveryLeaseGuard::commit` (a refused-advance `Delivered` is a black-hole).
        if let Some(guard) = sink_lease_guard {
            guard.commit(if advanced {
                super::super::LeaseOutcome::Delivered
            } else {
                super::super::LeaseOutcome::NotDelivered
            });
        }
    }

    pub(super) async fn deliver_response(
        &self,
        delivery: SessionRelayDelivery,
    ) -> Result<SessionRelayDeliveryOutcome, RelaySinkError> {
        #[cfg(test)]
        let gateway: Option<&dyn super::super::gateway::TurnGateway> = self.test_gateway.as_deref();
        #[cfg(not(test))]
        let gateway: Option<&dyn super::super::gateway::TurnGateway> = None;
        let channel_id = delivery.channel_id;
        let provider = delivery.provider.clone();
        let inflight = super::super::inflight::load_inflight_state(&provider, channel_id);
        // #3041 P1-3 (Part a, B1 — frame-carried): this pre-POST `inflight` is for the
        // route + trace ONLY; the advance gate re-loads FRESH after the POST
        // (`advance_after_confirmed_post`, codex P1-3 issue 3) so a turn cleared/replaced
        // during the POST can't authorize a wrong-turn advance.
        let trace = session_relay_trace_context(
            &provider,
            channel_id,
            &delivery.session_name,
            inflight.as_ref(),
        );
        // #3041 P1-4 codex (TOCTOU close): read the external-input lease ONCE and
        // thread the SAME snapshot into both the route decision and the RAII release
        // guard (guard generation == lease the route observed; no `.await` between).
        let observed_external_lease =
            crate::services::tui_prompt_dedupe::external_input_relay_lease(
                provider.as_str(),
                &delivery.session_name,
                channel_id,
            );
        let route = match session_bound_terminal_delivery_route_decision_with_lease(
            inflight.as_ref(),
            &delivery.session_name,
            &provider,
            channel_id,
            observed_external_lease.as_ref(),
        ) {
            SessionBoundTerminalDeliveryRouteDecision::Route(route) => route,
            SessionBoundTerminalDeliveryRouteDecision::Skipped => {
                tracing::debug!(
                    provider = provider.as_str(),
                    channel_id,
                    tmux_session = %delivery.session_name,
                    turn_id = trace.turn_id().unwrap_or(""),
                    dispatch_id = trace.dispatch_id().unwrap_or(""),
                    session_key = trace.session_key().unwrap_or(""),
                    relay_owner = trace.relay_owner(),
                    runtime_kind = trace.runtime_kind(),
                    "session-bound relay sink skipped bridge-owned or mismatched inflight"
                );
                crate::services::observability::emit_relay_delivery(
                    provider.as_str(),
                    channel_id,
                    trace.dispatch_id(),
                    trace.session_key(),
                    trace.turn_id(),
                    None,
                    "session_relay_sink",
                    "skip",
                    None,
                    None,
                    false,
                    Some("bridge-owned or mismatched inflight"),
                );
                // #3041 P1-5: the SOLE sink-local decline (foreign-owner block or
                // bridge-owned/mismatched inflight). `NotDelivered`, NOT `Unknown` (the
                // sink KNOWS it did not post) → §3.2 reconciliation SendFull if uncommitted.
                return Ok(SessionRelayDeliveryOutcome::NotDelivered);
            }
        };
        // #3041 P1-4 (§4-④): arm the RAII release-on-all-paths guard now this sink owns
        // the delivery — see `SessionBoundExternalInputLeaseGuard`.
        let _external_input_lease_guard =
            SessionBoundExternalInputLeaseGuard::arm_with_observed_lease(
                &provider,
                channel_id,
                &delivery.session_name,
                observed_external_lease.as_ref(),
            );
        let shared = self
            .health_registry
            .shared_for_provider(&provider)
            .await
            .ok_or_else(|| {
                RelaySinkError::Transient(format!(
                    "discord shared state unavailable for provider {}",
                    provider.as_str()
                ))
            })?;
        let (raw_response_text, relay_text) =
            relay_format::session_bound_relay_bodies(&shared, &provider, &delivery);
        let channel = ChannelId::new(channel_id);

        let cutover_range = match (
            delivery.frame_turn_start_offset,
            delivery.terminal_consumed_end,
        ) {
            (Some(start), Some(end)) if end > start => Some((start, end)),
            _ => None,
        };
        // Task-context delivery may confirm a card and then promote the response
        // route from PlaceholderEdit to NewMessage. Keep that entire operation on
        // the outer lease so both the card transport and the final answer remain
        // guarded even though the route mutates inside `ensure_card_and_route`.
        let cutover_short_replace = delivery.task_notification_context.is_none()
            && cutover_range.is_some()
            && !relay_text.is_empty()
            && matches!(route, SessionBoundTerminalDeliveryRoute::PlaceholderEdit(_))
            && !session_bound_should_send_new_chunks_for_placeholder(&relay_text)
            && {
                #[cfg(test)]
                {
                    !self.test_force_legacy_replace
                }
                #[cfg(not(test))]
                {
                    true
                }
            };
        let (lease_range, lease_fallback_start) = sink_delivery_lease_coordinate(&delivery);
        let sink_lease_key = delivery_lease_key_for_frame(
            channel,
            shared.restart.current_generation,
            &delivery,
            lease_fallback_start,
        );
        let sink_delivery_authority = delivery_frontier::capture_sink_delivery_authority(
            &shared,
            channel,
            &delivery,
            &sink_lease_key,
            lease_range,
        );
        let sink_delivery_ctx = delivery_frontier::SinkDeliveryCtx {
            shared: &shared,
            provider: &provider,
            channel,
            delivery: &delivery,
            authority: sink_delivery_authority,
        };
        let sink_lease_guard = if cutover_short_replace {
            None
        } else {
            let cell = shared.delivery_lease(channel);
            let Some(guard) = SinkDeliveryLeaseGuard::acquire(
                &cell,
                sink_lease_key.clone(),
                lease_range.0,
                lease_range.1,
            ) else {
                tracing::debug!(
                    provider = provider.as_str(),
                    channel_id,
                    tmux_session = %delivery.session_name,
                    lease_start = lease_range.0,
                    lease_end = lease_range.1,
                    "session-bound relay sink deferred terminal delivery because another owner holds the lease"
                );
                return Ok(SessionRelayDeliveryOutcome::NotDelivered);
            };
            #[cfg(test)]
            if let Some(probe) = &self.lease_test_probe {
                probe.acquired.notify_one();
                probe.release.notified().await;
            }
            Some(guard)
        };

        if delivery.relay_range.is_some() {
            if self.idle_range_already_committed_before_transport(
                &shared,
                &provider,
                channel_id,
                &delivery.session_name,
                &delivery,
            ) {
                if let Some(guard) = sink_lease_guard.as_ref() {
                    guard.commit(super::super::LeaseOutcome::Delivered);
                }
                return Ok(SessionRelayDeliveryOutcome::Delivered);
            }
            let frame_generation = delivery.relay_generation_mtime_ns.unwrap_or(0);
            let current_generation = dr::current_generation_mtime_ns(&delivery.session_name);
            let current_eof = idle_jsonl_current_eof(&provider, &delivery.session_name);
            if frame_generation == 0
                || current_generation != frame_generation
                || current_eof.is_none_or(|eof| lease_range.1 > eof)
            {
                return Ok(SessionRelayDeliveryOutcome::NotDelivered);
            }
        }

        let http = if gateway.is_some() {
            Arc::new(serenity::http::Http::new("test-gateway"))
        } else {
            shared.serenity_http_or_token_fallback().ok_or_else(|| {
                RelaySinkError::Transient(format!(
                    "discord http unavailable for provider {}",
                    provider.as_str()
                ))
            })?
        };
        let (route, task_card_message_id, task_response_claim_outcome) =
            ensure_card_and_route(&self.health_registry, &shared, &delivery, route).await?;
        let (task_response_claim, task_response_already_delivered): (
            Option<ResponseDeliveryClaim>,
            bool,
        ) = match task_response_claim_outcome {
            Some(ResponseDeliveryClaimOutcome::Owned(claim)) => (Some(claim), false),
            Some(ResponseDeliveryClaimOutcome::Wait) => {
                tracing::warn!(
                    provider = provider.as_str(),
                    channel_id,
                    tmux_session = %delivery.session_name,
                    "task response is deferred to the watcher or owned by another live claimant"
                );
                return Ok(SessionRelayDeliveryOutcome::NotDelivered);
            }
            Some(ResponseDeliveryClaimOutcome::Delivered { .. }) => (None, true),
            Some(ResponseDeliveryClaimOutcome::SentUncommitted { card_message_id }) => {
                tracing::error!(
                    provider = provider.as_str(),
                    channel_id,
                    tmux_session = %delivery.session_name,
                    task_card_message_id = card_message_id,
                    "task response was already sent but its final delivery CAS is uncommitted; refusing a duplicate POST"
                );
                (None, true)
            }
            None => (None, false),
        };

        if task_response_already_delivered {
            self.advance_after_confirmed_post(
                &shared,
                &provider,
                channel_id,
                &delivery.session_name,
                &delivery,
                sink_lease_guard.as_ref(),
            );
            return Ok(SessionRelayDeliveryOutcome::Delivered);
        }

        if let SessionBoundTerminalDeliveryRoute::PlaceholderEdit(msg_id) = route {
            if let Some((start, end)) = cutover_range.filter(|_| cutover_short_replace) {
                let live_gateway = super::super::gateway::DiscordGateway::new(
                    http.clone(),
                    shared.clone(),
                    provider.clone(),
                    None,
                );
                return short_controller::deliver_short_replace_via_controller(
                    gateway.unwrap_or(&live_gateway),
                    short_controller::SinkShortReplaceCtx {
                        shared: &shared,
                        provider: &provider,
                        channel,
                        channel_id,
                        msg_id,
                        relay_text: &relay_text,
                        delivered_fingerprint_body: &raw_response_text,
                        delivery: &delivery,
                        sink_lease_key,
                        sink_delivery_authority,
                        trace: &trace,
                        range: (start, end),
                        delivered_total: &self.delivered_total,
                    },
                )
                .await;
            }
            let mut direct_journal_attempt =
                if journal::journals_sink_direct(&route, cutover_short_replace) {
                    self.journal.begin_fresh(&shared, &delivery)
                } else {
                    None
                };
            if session_bound_should_send_new_chunks_for_placeholder(&relay_text) {
                let (message_ids, chunk_anchor_receipt) =
                    journal::send_long_chunks_with_anchor_receipt(
                        gateway,
                        &http,
                        channel,
                        msg_id,
                        &relay_text,
                        &shared,
                    )
                    .await?;
                if let Some(gateway) = gateway {
                    let _ = gateway.delete_message(channel, msg_id).await;
                } else {
                    let _ =
                        super::super::http::delete_channel_message(&http, channel, msg_id).await;
                }
                self.delivered_total.fetch_add(1, Ordering::AcqRel);
                tracing::info!(
                    provider = provider.as_str(),
                    channel_id,
                    message = msg_id.get(),
                    tmux_session = %delivery.session_name,
                    turn_id = trace.turn_id().unwrap_or(""),
                    dispatch_id = trace.dispatch_id().unwrap_or(""),
                    session_key = trace.session_key().unwrap_or(""),
                    relay_owner = trace.relay_owner(),
                    runtime_kind = trace.runtime_kind(),
                    chars = relay_text.chars().count(),
                    "session-bound relay sink delivered long terminal response as ordered new chunks"
                );
                crate::services::observability::emit_relay_delivery(
                    provider.as_str(),
                    channel_id,
                    trace.dispatch_id(),
                    trace.session_key(),
                    trace.turn_id(),
                    None,
                    "session_relay_sink",
                    "post",
                    None,
                    None,
                    true,
                    Some("long response sent as ordered chunks"),
                );
                let proof = delivery_frontier::finish_sink_delivery(
                    sink_delivery_ctx,
                    message_ids.last().map(|message_id| message_id.get()),
                    &raw_response_text,
                    sink_lease_guard.as_ref(),
                    "src/services/discord/session_relay_sink.rs:sink_long_chunks_advance",
                );
                journal::settle(
                    self.journal,
                    &mut direct_journal_attempt,
                    chunk_anchor_receipt,
                    proof,
                );
                return Ok(SessionRelayDeliveryOutcome::from_proof(proof));
            }
            #[cfg(test)]
            let mut last_chunk_anchor = self.test_replace_anchor.clone();
            #[cfg(not(test))]
            let mut last_chunk_anchor = None;
            let mut edit_anchor_receipt = None;
            let replace_outcome = if let Some(gateway) = gateway {
                gateway
                    .replace_message_with_outcome(channel, msg_id, &relay_text)
                    .await
            } else {
                formatting::replace_long_message_raw_with_outcome_returning_receipt(
                    &http,
                    channel,
                    msg_id,
                    &relay_text,
                    &shared,
                    &mut last_chunk_anchor,
                    &mut edit_anchor_receipt,
                )
                .await
                .map_err(|error| error.to_string())
            };
            match replace_outcome {
                Ok(ReplaceLongMessageOutcome::EditedOriginal) => {
                    self.delivered_total.fetch_add(1, Ordering::AcqRel);
                    tracing::info!(
                        provider = provider.as_str(),
                        channel_id,
                        message = msg_id.get(),
                        tmux_session = %delivery.session_name,
                        turn_id = trace.turn_id().unwrap_or(""),
                        dispatch_id = trace.dispatch_id().unwrap_or(""),
                        session_key = trace.session_key().unwrap_or(""),
                        relay_owner = trace.relay_owner(),
                        runtime_kind = trace.runtime_kind(),
                        chars = relay_text.chars().count(),
                        "session-bound relay sink delivered terminal response via placeholder edit"
                    );
                    crate::services::observability::emit_relay_delivery(
                        provider.as_str(),
                        channel_id,
                        trace.dispatch_id(),
                        trace.session_key(),
                        trace.turn_id(),
                        Some(msg_id.get()),
                        "session_relay_sink",
                        "edit",
                        None,
                        None,
                        true,
                        Some("placeholder edit"),
                    );
                    let anchor = formatting::watcher_completion_footer_anchor(
                        last_chunk_anchor.as_ref(),
                        msg_id,
                        &relay_text,
                    )
                    .0;
                    let proof = delivery_frontier::finish_sink_delivery(
                        sink_delivery_ctx,
                        Some(anchor.get()),
                        &raw_response_text,
                        sink_lease_guard.as_ref(),
                        "src/services/discord/session_relay_sink.rs:sink_legacy_short_edit_advance",
                    );
                    journal::settle(
                        self.journal,
                        &mut direct_journal_attempt,
                        edit_anchor_receipt.take(),
                        proof,
                    );
                    Ok(SessionRelayDeliveryOutcome::from_proof(proof))
                }
                Ok(ReplaceLongMessageOutcome::SentFallbackAfterEditFailure {
                    edit_error,
                    replacement_anchor,
                }) => {
                    // #2757 (A0 #3089): never delete msg_id — it is the bridge's
                    // current_msg_id, possibly holding streamed content a transient edit
                    // failure would vacuum. The shared policy pins this preserve decision.
                    let preserve_original = !edit_fail_fallback_disposition().deletes_original();
                    debug_assert!(preserve_original, "#2757: must preserve original");
                    self.delivered_total.fetch_add(1, Ordering::AcqRel);
                    tracing::warn!(
                        provider = provider.as_str(),
                        channel_id,
                        message = msg_id.get(),
                        tmux_session = %delivery.session_name,
                        turn_id = trace.turn_id().unwrap_or(""),
                        dispatch_id = trace.dispatch_id().unwrap_or(""),
                        session_key = trace.session_key().unwrap_or(""),
                        relay_owner = trace.relay_owner(),
                        runtime_kind = trace.runtime_kind(),
                        chars = relay_text.chars().count(),
                        error = %edit_error,
                        "session-bound relay sink delivered terminal response via fallback; preserving original msg_id (#2757)"
                    );
                    crate::services::observability::emit_relay_delivery(
                        provider.as_str(),
                        channel_id,
                        trace.dispatch_id(),
                        trace.session_key(),
                        trace.turn_id(),
                        Some(msg_id.get()),
                        "session_relay_sink",
                        "post",
                        None,
                        None,
                        true,
                        Some("fallback after edit failure"),
                    );
                    let proof = delivery_frontier::finish_sink_delivery(
                        sink_delivery_ctx,
                        replacement_anchor.map(|anchor| anchor.get()),
                        &raw_response_text,
                        sink_lease_guard.as_ref(),
                        "src/services/discord/session_relay_sink.rs:sink_legacy_short_fallback_advance",
                    );
                    journal::settle(
                        self.journal,
                        &mut direct_journal_attempt,
                        edit_anchor_receipt.take(),
                        proof,
                    );
                    Ok(SessionRelayDeliveryOutcome::from_proof(proof))
                }
                Ok(ReplaceLongMessageOutcome::PartialContinuationFailure { error, .. }) => {
                    Err(RelaySinkError::Transient(
                        super::super::replace_outcome_policy::strip_watcher_send_failure_class_marker(
                            &error,
                        )
                        .to_string(),
                    ))
                }
                Err(error) => {
                    let error = error.to_string();
                    Err(RelaySinkError::Transient(
                        super::super::replace_outcome_policy::strip_watcher_send_failure_class_marker(
                            &error,
                        )
                        .to_string(),
                    ))
                }
            }
        } else {
            self.deliver_new_message_with_task_authority(
                gateway,
                &shared,
                &provider,
                channel_id,
                &delivery,
                &relay_text,
                task_card_message_id,
                task_response_claim,
                &trace,
                sink_lease_guard.as_ref(),
                sink_delivery_ctx,
            )
            .await
        }
    }
}
