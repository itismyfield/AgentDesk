use super::*;

pub(super) struct LegacySessionRelayConsumer(pub(super) Arc<SessionBoundDiscordRelaySink>);

#[async_trait::async_trait]
impl RelaySink for LegacySessionRelayConsumer {
    async fn deliver(&self, frame: &StreamFrame) -> Result<RelaySinkOutcome, RelaySinkError> {
        self.0.deliver(frame).await
    }
}

pub(crate) async fn run_session_bound_discord_relay_supervisor(
    health_registry: Option<Arc<HealthRegistry>>,
    shutdown: Arc<AtomicBool>,
) {
    let Some(health_registry) = health_registry else {
        tracing::warn!(
            "session-bound Discord relay sink unavailable: missing HealthRegistry; using metrics-only sink"
        );
        crate::services::cluster::registry_adapter_sink::run_with_registry_adapter_sink(shutdown)
            .await;
        return;
    };

    SESSION_BOUND_DISCORD_DELIVERY_ENABLED.store(true, Ordering::Release);
    let sink = Arc::new(SessionBoundDiscordRelaySink::new(health_registry));
    let idle_sink = sink.clone();
    let idle_shutdown = shutdown.clone();
    super::task_supervisor::spawn_observed(
        "session_bound_idle_jsonl_relay",
        async move {
            run_idle_jsonl_relay_loop(idle_shutdown, idle_sink).await;
        }
        .instrument(tracing::info_span!("session_bound_idle_jsonl_relay")),
    );
    run_watcher_supervisor_loop(
        SupervisorConfig::default(),
        Arc::new(LegacySessionRelayConsumer(sink)),
        shutdown,
    )
    .await;
    SESSION_BOUND_DISCORD_DELIVERY_ENABLED.store(false, Ordering::Release);
}

pub(super) async fn run_idle_jsonl_relay_loop(
    shutdown: Arc<AtomicBool>,
    sink: Arc<SessionBoundDiscordRelaySink>,
) {
    let registry = crate::services::cluster::session_registry::global_session_registry();
    let producers =
        crate::services::cluster::relay_producer_registry::global_relay_producer_registry();
    let mut offsets: HashMap<String, IdleCursor> = HashMap::new();
    let mut pending_ends: HashMap<String, IdlePending> = HashMap::new();
    let mut first_seen_at: HashMap<String, Instant> = HashMap::new();
    let mut last_inflight_seen_at: HashMap<String, Instant> = HashMap::new();
    let mut session_init_seen: HashSet<String> = HashSet::new();
    let mut session_generation_signatures: HashMap<String, i64> = HashMap::new();

    while !shutdown.load(Ordering::Acquire) {
        let mut seen_sessions = HashSet::new();
        for entry in registry.list_matched() {
            let matched = entry.matched;
            let session_name = matched.expected_session_name.clone();
            let relay_source = idle_jsonl_relay_source_for_matched(&matched);
            seen_sessions.insert(session_name.clone());
            let mut first_seen = *first_seen_at
                .entry(session_name.clone())
                .or_insert_with(Instant::now);
            let Ok(channel_id) = matched.channel_id.parse::<u64>() else {
                continue;
            };
            let Ok(metadata) = std::fs::metadata(&relay_source.path) else {
                continue;
            };
            let len = metadata.len();
            let expected_file = crate::services::cluster::stream_relay::SourceFileIdentity::Unix {
                dev: std::os::unix::fs::MetadataExt::dev(&metadata),
                ino: std::os::unix::fs::MetadataExt::ino(&metadata),
            };
            let source = (
                matched.provider.clone(),
                channel_id,
                relay_source.path.clone(),
                expected_file,
            );
            let replacements = idle_jsonl::relay_output_replacements(&relay_source.path);
            let mut replaced_in_place = false;
            if let Some(cursor) = offsets.get(&session_name).filter(|c| c.source != source) {
                // Only a rebind's replacement restarts at 0; another new file keeps EOF.
                replaced_in_place =
                    cursor.source.2 == source.2 && cursor.replacements != replacements;
                offsets.remove(&session_name);
                first_seen = Instant::now();
                first_seen_at.insert(session_name.clone(), first_seen);
                pending_ends.remove(&session_name);
                session_init_seen.remove(&session_name);
            }
            let cursor = offsets.entry(session_name.clone()).or_insert(IdleCursor {
                offset: if replaced_in_place { 0 } else { len },
                source,
                first_restore: !replaced_in_place,
                replacements,
            });
            if len < cursor.offset {
                cursor.offset = 0;
                cursor.first_restore = false;
                pending_ends.remove(&session_name);
                session_init_seen.remove(&session_name);
            }
            let source_marker = source_epoch_observer::marker_if_enabled(&session_name);
            let current_generation_signature =
                super::tmux::read_generation_file_mtime_ns(&session_name);
            if idle_jsonl_clear_session_init_on_generation_signature_change(
                &mut session_init_seen,
                &mut session_generation_signatures,
                &session_name,
                current_generation_signature,
            ) {
                pending_ends.remove(&session_name);
            }
            let channel = ChannelId::new(channel_id);
            let shared_for_dedup = idle_jsonl_prepare_dedup_shared(
                &sink.health_registry,
                &matched,
                channel,
                &session_name,
                len,
                &mut session_init_seen,
            )
            .await;
            let Some(shared) = shared_for_dedup else {
                continue;
            };
            let durable = dr::delivered_frontier_end_current_generation(
                &matched.provider,
                channel,
                &session_name,
                Some(len),
            );
            if std::mem::take(&mut cursor.first_restore) && (1..=cursor.offset).contains(&durable) {
                cursor.offset = durable;
            }
            let offset = &mut cursor.offset;
            let committed = dr::effective_committed_offset(
                &shared,
                &matched.provider,
                channel,
                &session_name,
                Some(len),
            )
            .max(durable);

            macro_rules! consume_idle_offset {
                ($to:expr, $rearm:expr) => {
                    pending_ends.remove(&session_name);
                    idle_jsonl_consume_offset(
                        &mut session_init_seen,
                        &session_name,
                        offset,
                        $to,
                        $rearm,
                    )
                };
            }

            if let Some(mut inflight) =
                super::inflight::load_inflight_state(&matched.provider, channel_id)
            {
                if orphan_reclaim::reclaim_orphaned_session_bound_relay_if_dead(
                    &sink.health_registry,
                    &producers,
                    &matched.provider,
                    channel_id,
                    &session_name,
                    &inflight,
                )
                .await
                {
                    inflight.set_relay_owner_kind(super::inflight::RelayOwnerKind::None);
                }
                if !super::inflight::ownerless_external_input_inflight_is_stale(&inflight) {
                    let decision = idle_jsonl_apply_active_inflight_gate(
                        &mut last_inflight_seen_at,
                        &matched,
                        channel_id,
                        &inflight,
                    );
                    if matches!(
                        decision,
                        idle_jsonl::IdleJsonlInflightGateDecision::DeferUntilCommitted if len > *offset
                    ) {
                        pending_ends.insert(session_name.clone(), Deferred);
                        if matches!(
                            idle_jsonl_suppressed_range_action(
                                committed,
                                *offset,
                                len,
                                IdleJsonlSuppression::DeferUntilCommitted,
                            ),
                            IdleRelayRangeAction::AdvanceCommitted
                        ) {
                            consume_idle_offset!(len, IdleJsonlSessionInitRearm::Keep);
                        }
                    }
                    continue;
                }
                last_inflight_seen_at.remove(&session_name);
            }

            let in_recent_inflight_grace = last_inflight_seen_at
                .get(&session_name)
                .is_some_and(|seen_at| seen_at.elapsed() < IDLE_JSONL_RELAY_RECENT_INFLIGHT_GRACE);
            let in_new_session_grace =
                first_seen.elapsed() < IDLE_JSONL_RELAY_RECENT_INFLIGHT_GRACE;
            if (in_recent_inflight_grace || in_new_session_grace) && len > *offset {
                pending_ends.insert(session_name.clone(), Deferred);
                match idle_jsonl_suppressed_range_action(
                    committed,
                    *offset,
                    len,
                    IdleJsonlSuppression::DeferUntilCommitted,
                ) {
                    IdleRelayRangeAction::AdvanceCommitted => {
                        consume_idle_offset!(len, IdleJsonlSessionInitRearm::Keep);
                    }
                    IdleRelayRangeAction::HoldPending => {}
                    _ => unreachable!("deferred suppression returns only hold/advance"),
                }
                continue;
            }
            if len <= *offset {
                continue;
            }

            let start = *offset;
            let was_deferred = matches!(
                pending_ends.get(&session_name),
                Some(Deferred | SentUnconfirmed | RetainedForRetry(true))
            );
            pending_ends.insert(session_name.clone(), RetainedForRetry(was_deferred));
            let end = len.min(start.saturating_add(IDLE_JSONL_RELAY_MAX_BYTES_PER_TICK));
            let Ok(opened_range) = read_jsonl_range(&relay_source.path, start, end) else {
                continue;
            };
            if opened_range.file_identity != expected_file {
                continue;
            }
            let (payload, end) = (&opened_range.payload, opened_range.end);
            if payload.is_empty() {
                continue;
            }
            if idle_jsonl_payload_contains_schedule_wakeup_setup(payload) {
                consume_idle_offset!(end, IdleJsonlSessionInitRearm::Keep);
                continue;
            }
            if !was_deferred && idle_jsonl_payload_contains_user_event(payload) {
                consume_idle_offset!(end, IdleJsonlSessionInitRearm::Clear);
                continue;
            }
            let session_has_init =
                idle_jsonl_session_has_init(&mut session_init_seen, &session_name, payload);
            let action = idle_relay_range_action(
                payload,
                start,
                end,
                committed,
                relay_source.allow_continued_session_without_init,
                session_has_init,
                was_deferred,
            );
            match action {
                IdleRelayRangeAction::DropAndConsume => {
                    consume_idle_offset!(end, IdleJsonlSessionInitRearm::Keep);
                }
                IdleRelayRangeAction::AdvanceCommitted => {
                    consume_idle_offset!(end, IdleJsonlSessionInitRearm::Keep);
                }
                IdleRelayRangeAction::SendPendingSuffixFrom(from) => {
                    let Some(producer) = producers.get_producer(&session_name) else {
                        continue;
                    };
                    let Ok(suffix) = opened_range.suffix(&relay_source.path, from) else {
                        continue;
                    };
                    if suffix.file_identity != expected_file || suffix.payload.is_empty() {
                        continue;
                    }
                    let end = suffix.end;
                    let source_stamp = source_marker.and_then(|marker| {
                        source_epoch_observer::source_stamp(
                            &session_name,
                            marker,
                            suffix.file_identity,
                        )
                    });
                    if producer.try_send_frame_for_range_with_source(
                        String::from_utf8_lossy(&suffix.payload).into_owned(),
                        from,
                        end,
                        current_generation_signature,
                        source_stamp,
                    ) {
                        pending_ends.insert(session_name.clone(), SentUnconfirmed);
                    }
                }
                IdleRelayRangeAction::HoldPending => {
                    pending_ends.insert(session_name.clone(), Deferred);
                }
            }
        }

        prune_idle_jsonl_session_state(
            &seen_sessions,
            &mut offsets,
            &mut first_seen_at,
            &mut last_inflight_seen_at,
            &mut session_init_seen,
            &mut session_generation_signatures,
            &mut pending_ends,
        );
        #[cfg(test)]
        tests::dc1_observe_tick(&offsets, &pending_ends);
        tokio::time::sleep(IDLE_JSONL_RELAY_POLL_INTERVAL).await;
    }
}
