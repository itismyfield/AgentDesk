//! #3038 S1 voice background completion targeting helpers moved out of turn_bridge/mod.rs.

use super::*;
use crate::voice::turn_link::VoiceTurnLink;

/// Returns true when any nested object inside `value` has `key` set to boolean `true`.
pub(super) fn json_any_true_flag(value: &serde_json::Value, key: &str) -> bool {
    let mut stack = vec![value];
    while let Some(node) = stack.pop() {
        match node {
            serde_json::Value::Object(map) => {
                if map.get(key).and_then(serde_json::Value::as_bool) == Some(true) {
                    return true;
                }
                stack.extend(map.values());
            }
            serde_json::Value::Array(items) => stack.extend(items.iter()),
            _ => {}
        }
    }
    false
}

/// Returns true when a typed voice-background handoff marker exists for
/// this `user_msg_id`.
///
/// The marker is stamped by `dispatch_voice_background_handoff` at the
/// foreground→background dispatch site (#2236) and consumed exactly once
/// by `voice_background_completion_target` on terminal delivery. The
/// previous implementation matched a hardcoded Korean/English prefix in
/// the prompt body — that prefix is user-controllable (it appears in the
/// LLM-visible prompt that any user could literally type into a mapped
/// background channel), so the prefix match was a routing-hijack vector.
///
/// The legacy prefix fallback was deliberately removed in #2236 follow-up
/// review: keeping it open during a deprecation window would have left
/// the original spoofable path alive for new traffic, not just in-flight
/// turns. Since voice-background routing was first merged in #2207
/// (immediate predecessor of this fix), there are no long-running
/// in-flight turns to migrate. The hard cutover is safe.
// #3034: handoff-marker predicate pinned by the voice-background unit tests; the
// live routing path consults the marker store inline. Test contract.
#[allow(dead_code)]
fn has_voice_background_handoff_marker(user_msg_id: MessageId, _text: &str) -> bool {
    crate::voice::announce_meta::global_store()
        .get_handoff(user_msg_id)
        .is_some()
}

/// Resolves the foreground voice channel that should hear the spoken
/// summary of a finished background turn.
///
/// #2236: delivery is bound to the typed marker stamped at dispatch time.
/// The marker carries the original `voice_channel_id` and
/// `background_channel_id`, so routing does NOT round-trip through the
/// (potentially-stale, multi-agent-ambiguous) reverse lookup any more.
///
/// #2274: when the in-memory store misses (e.g. after a dcserver restart
/// during a long background turn before rehydration finished, or when
/// terminal delivery lands on a different cluster node than the dispatch
/// node) the durable PG side store is consulted as a fallback. The atomic
/// `UPDATE … SET consumed_at = NOW() RETURNING …` claim there guarantees
/// exactly-once consumption across nodes.
///
/// - Returns `None` when no marker exists for this `user_msg_id` (this
///   turn was not a voice-background handoff).
/// - Returns `None` when the marker exists but the recorded
///   `background_channel_id` does not match the channel that fired this
///   turn (sanity-check; would only happen if a marker were stamped
///   against the wrong message id by a buggy dispatch path).
/// - Otherwise consumes the marker and returns the recorded voice
///   channel id directly. `mapped_voice_channel_id` is accepted only as
///   a cross-check parameter — if reverse lookup also resolved a voice
///   channel and it disagrees with the marker, a warn is emitted but the
///   marker still wins (it is the authoritative origin record).
pub(super) async fn voice_background_completion_target(
    mapped_voice_channel_id: Option<ChannelId>,
    dispatch_id: Option<&str>,
    user_msg_id: MessageId,
    turn_id: Option<&str>,
    user_text: &str,
    channel_id: ChannelId,
    pool: Option<&sqlx::PgPool>,
) -> Option<ChannelId> {
    if let Some(link) =
        resolve_voice_turn_link_for_playback(pool, dispatch_id, Some(user_msg_id), turn_id).await
    {
        if link.background_channel_id != channel_id.get() {
            tracing::warn!(
                event = "voice_turn_link_background_channel_mismatch",
                channel_id = channel_id.get(),
                link_background_channel_id = link.background_channel_id,
                link_voice_channel_id = link.voice_channel_id,
                dispatch_id,
                turn_id,
                user_msg_id = user_msg_id.get(),
                "voice_turn_link resolved a different background channel than the turn fired in; refusing to route spoken summary"
            );
            return None;
        }
        if let Some(mapped) = mapped_voice_channel_id
            && mapped.get() != link.voice_channel_id
        {
            tracing::warn!(
                event = "voice_turn_link_voice_channel_disagrees_with_reverse_lookup",
                channel_id = channel_id.get(),
                link_voice_channel_id = link.voice_channel_id,
                reverse_lookup_voice_channel_id = mapped.get(),
                dispatch_id,
                turn_id,
                user_msg_id = user_msg_id.get(),
                "voice_turn_link disagrees with current reverse-lookup voice channel; durable link wins"
            );
        }
        if let Some(pool) = pool
            && let Err(error) = crate::voice::turn_link::mark_terminal_voice_turn_link_pg(
                pool,
                link.guild_id,
                link.voice_channel_id,
                &link.utterance_id,
                link.generation,
            )
            .await
        {
            tracing::warn!(
                error = %error,
                dispatch_id,
                turn_id,
                user_msg_id = user_msg_id.get(),
                "voice_turn_link terminal mark failed after resolving TTS playback target"
            );
        }
        return Some(ChannelId::new(link.voice_channel_id));
    }

    let store = crate::voice::announce_meta::global_store();
    let handoff_correlation_id =
        crate::voice::prompt::parse_voice_background_handoff_correlation_id(user_text);
    // #2274 Codex review finding #1: when a PG pool is available the
    // durable `UPDATE ... SET consumed_at = NOW() RETURNING ...` claim is
    // ALWAYS authoritative for one-shot consumption, even when an
    // in-memory marker is present. Otherwise two cluster nodes (or a
    // pre-restart in-memory holder paired with a post-restart rehydrate)
    // could both observe a local marker and route duplicate spoken
    // summaries. The local store is treated as a hot cache, never as the
    // routing gate.
    let meta = if let Some(pool) = pool {
        match crate::voice::announce_meta::take_handoff_durable(pool, user_msg_id).await {
            Ok(Some(meta)) => {
                // Drop any local copy so a parallel caller cannot see it.
                store.forget_handoff(user_msg_id);
                meta
            }
            Ok(None) => {
                if let Some(correlation_id) = handoff_correlation_id.as_deref() {
                    match crate::voice::announce_meta::take_handoff_reservation_durable(
                        pool,
                        correlation_id,
                    )
                    .await
                    {
                        Ok(Some(meta)) => {
                            store.cancel_handoff_reservation(correlation_id);
                            store.forget_handoff(user_msg_id);
                            meta
                        }
                        Ok(None) => {
                            if let Some(local) = store.get_handoff_reservation(correlation_id) {
                                if local.local_only_fallback {
                                    if let Some(consumed) =
                                        store.take_handoff_reservation(correlation_id)
                                    {
                                        tracing::warn!(
                                            event = "voice_background_handoff_local_only_fallback",
                                            user_msg_id = user_msg_id.get(),
                                            channel_id = channel_id.get(),
                                            correlation_id,
                                            marker_voice_channel_id = consumed.voice_channel_id,
                                            marker_background_channel_id =
                                                consumed.background_channel_id,
                                            "durable PG reservation absent but local reservation is flagged local-only fallback; routing spoken summary from in-memory reservation"
                                        );
                                        consumed
                                    } else {
                                        return None;
                                    }
                                } else {
                                    tracing::info!(
                                        event = "voice_background_handoff_durable_already_consumed",
                                        user_msg_id = user_msg_id.get(),
                                        channel_id = channel_id.get(),
                                        correlation_id,
                                        "in-memory reservation present but durable PG reservation is absent or already consumed; refusing to route to avoid duplicate spoken summary"
                                    );
                                    store.cancel_handoff_reservation(correlation_id);
                                    return None;
                                }
                            } else if let Some(local) = store.get_handoff(user_msg_id) {
                                if local.local_only_fallback {
                                    if let Some(consumed) = store.take_handoff(user_msg_id) {
                                        tracing::warn!(
                                            event = "voice_background_handoff_local_only_fallback",
                                            user_msg_id = user_msg_id.get(),
                                            channel_id = channel_id.get(),
                                            marker_voice_channel_id = consumed.voice_channel_id,
                                            marker_background_channel_id =
                                                consumed.background_channel_id,
                                            "durable PG row absent but local marker is flagged local-only fallback (dispatch-time persist failure); routing spoken summary from in-memory marker"
                                        );
                                        consumed
                                    } else {
                                        return None;
                                    }
                                } else {
                                    tracing::info!(
                                        event = "voice_background_handoff_durable_already_consumed",
                                        user_msg_id = user_msg_id.get(),
                                        channel_id = channel_id.get(),
                                        "in-memory marker present but durable PG row is absent or already consumed; refusing to route to avoid duplicate spoken summary"
                                    );
                                    store.forget_handoff(user_msg_id);
                                    return None;
                                }
                            } else {
                                return None;
                            }
                        }
                        Err(error) => {
                            tracing::warn!(
                                error = %error,
                                user_msg_id = user_msg_id.get(),
                                correlation_id,
                                "voice_background_handoff durable reservation claim failed; refusing to route spoken summary"
                            );
                            return None;
                        }
                    }
                } else if let Some(local) = store.get_handoff(user_msg_id) {
                    if local.local_only_fallback {
                        if let Some(consumed) = store.take_handoff(user_msg_id) {
                            tracing::warn!(
                                event = "voice_background_handoff_local_only_fallback",
                                user_msg_id = user_msg_id.get(),
                                channel_id = channel_id.get(),
                                marker_voice_channel_id = consumed.voice_channel_id,
                                marker_background_channel_id = consumed.background_channel_id,
                                "durable PG row absent but local marker is flagged local-only fallback (dispatch-time persist failure); routing spoken summary from in-memory marker"
                            );
                            consumed
                        } else {
                            // Lost the race against another take_handoff
                            // caller on the same node — be safe and drop.
                            return None;
                        }
                    } else {
                        tracing::info!(
                            event = "voice_background_handoff_durable_already_consumed",
                            user_msg_id = user_msg_id.get(),
                            channel_id = channel_id.get(),
                            "in-memory marker present but durable PG row is absent or already consumed; refusing to route to avoid duplicate spoken summary"
                        );
                        store.forget_handoff(user_msg_id);
                        return None;
                    }
                } else {
                    return None;
                }
            }
            Err(error) => {
                tracing::warn!(
                    error = %error,
                    user_msg_id = user_msg_id.get(),
                    "voice_background_handoff durable claim failed; refusing to route spoken summary"
                );
                return None;
            }
        }
    } else {
        // No durable backstop available (typically: dev / no-PG mode).
        // Preserve legacy local-only behaviour for setups that never
        // persisted the marker in the first place. Single-node, no-PG
        // deployments cannot duplicate routing because there is only
        // one consumer.
        store.take_handoff(user_msg_id).or_else(|| {
            handoff_correlation_id
                .as_deref()
                .and_then(|correlation_id| store.take_handoff_reservation(correlation_id))
        })?
    };
    if meta.background_channel_id != channel_id.get() {
        tracing::warn!(
            event = "voice_background_handoff_channel_mismatch",
            user_msg_id = user_msg_id.get(),
            channel_id = channel_id.get(),
            marker_background_channel_id = meta.background_channel_id,
            marker_voice_channel_id = meta.voice_channel_id,
            "typed handoff marker recorded a different background channel than the turn fired in; refusing to route spoken summary"
        );
        return None;
    }
    if let Some(mapped) = mapped_voice_channel_id
        && mapped.get() != meta.voice_channel_id
    {
        tracing::warn!(
            event = "voice_background_handoff_voice_channel_disagrees_with_reverse_lookup",
            user_msg_id = user_msg_id.get(),
            channel_id = channel_id.get(),
            marker_voice_channel_id = meta.voice_channel_id,
            reverse_lookup_voice_channel_id = mapped.get(),
            "typed handoff marker disagrees with current reverse-lookup voice channel; marker wins (authoritative origin record)"
        );
    }
    Some(ChannelId::new(meta.voice_channel_id))
}

pub(super) async fn resolve_voice_turn_link_for_playback(
    pool: Option<&sqlx::PgPool>,
    dispatch_id: Option<&str>,
    announce_message_id: Option<MessageId>,
    turn_id: Option<&str>,
) -> Option<VoiceTurnLink> {
    let Some(pool) = pool else {
        return None;
    };

    if let Some(dispatch_id) = dispatch_id.map(str::trim).filter(|value| !value.is_empty()) {
        match crate::voice::turn_link::lookup_active_voice_turn_link_by_dispatch_id_pg(
            pool,
            dispatch_id,
        )
        .await
        {
            Ok(Some(link)) => return Some(link),
            Ok(None) => {}
            Err(error) => {
                tracing::warn!(
                    error = %error,
                    dispatch_id,
                    "voice_turn_link dispatch lookup failed for TTS playback routing"
                );
            }
        }
    }

    if let Some(announce_message_id) = announce_message_id {
        match crate::voice::turn_link::lookup_active_voice_turn_link_by_announce_message_id_pg(
            pool,
            announce_message_id.get(),
        )
        .await
        {
            Ok(Some(link)) => return Some(link),
            Ok(None) => {}
            Err(error) => {
                tracing::warn!(
                    error = %error,
                    announce_message_id = announce_message_id.get(),
                    "voice_turn_link announce-message lookup failed for TTS playback routing"
                );
            }
        }
    }

    if let Some(turn_id) = turn_id.map(str::trim).filter(|value| !value.is_empty()) {
        match crate::voice::turn_link::lookup_active_voice_turn_link_by_turn_id_pg(pool, turn_id)
            .await
        {
            Ok(Some(link)) => return Some(link),
            Ok(None) => {}
            Err(error) => {
                tracing::warn!(
                    error = %error,
                    turn_id,
                    "voice_turn_link turn lookup failed for TTS playback routing"
                );
            }
        }
    }

    None
}

#[cfg(test)]
#[path = "voice_completion_tests.rs"]
mod voice_completion_tests;
