//! #3960 — idle-JSONL-relay gate that reclaims an orphaned `SessionBoundRelay`
//! TUI-direct row whose claim-time producer has died (the #3876 residual
//! deferred from PR #3953).
//!
//! The #3876 producer-gate stamps `SessionBoundRelay` only when a live
//! per-session relay producer exists AT CLAIM time. The producer can die in the
//! window before the terminal commit/ACK; the row then stays owned by
//! `SessionBoundRelay`, so the ownerless staleness reclaim never fires and the
//! TUI-direct answer black-holes. This gate RE-CHECKS producer liveness AND the
//! generation-aware committed-offset delivery authority at the idle-relay tick
//! (not just at claim) and, when the body was provably never delivered,
//! downgrades the orphaned owner to the bridge-adapter backstop (`None`) under
//! the inflight flock — re-joining the row to ownerless recovery.
//!
//! NO-DOUBLE-RELAY ATTRIBUTION: the `committed_offset <= turn_floor` check here is
//! a best-effort UNLOCKED first-line filter — it suppresses a downgrade when the
//! watermark has ALREADY advanced past the body at scan time. It is NOT the
//! authoritative guard, because the real `SessionBoundRelay` NewMessage route
//! (`session_relay_sink.rs:1066-1124`) advances ONLY the shared
//! `confirmed_end_offset` watermark and writes NOTHING to the inflight row: a
//! delivery that lands AFTER this read still leaves the row orphan-shaped, so the
//! downgrade proceeds. Single delivery is then guaranteed at the SEND POINT —
//! every re-delivery path re-reads `effective_committed_offset` FRESH and
//! `idle_relay_range_action` returns `SkipAlreadyRelayed` (body already past the
//! watermark) or `SendSuffixFrom(committed)` (only the uncommitted tail). See the
//! `send_point_re_gate_*` tests below.

use crate::services::cluster::relay_producer_registry::RelayProducerRegistry;
use crate::services::discord::health::HealthRegistry;
use crate::services::discord::inflight::{self, InflightTurnIdentity, InflightTurnState};
use crate::services::discord::outbound::delivery_record::effective_committed_offset;
use crate::services::provider::ProviderKind;
use serenity::model::id::ChannelId;

/// #3960 — pure reclaim decision for an orphaned `SessionBoundRelay` TUI-direct
/// row. ALL THREE conditions must hold before the owner is downgraded to the
/// bridge-adapter backstop:
///   1. `orphan_shape` — the row is a quiescent, stale, uncommitted
///      `SessionBoundRelay` TUI-direct claim (see
///      `inflight::orphan_relay_reclaim::session_bound_relay_external_input_orphan_shape`).
///   2. `producer_gone` — the claim-time relay producer is no longer registered
///      (the #3876 `global_relay_producer_registry` liveness, RE-CHECKED here at
///      commit time rather than only at claim time). A live producer that
///      survives keeps delivering normally → `producer_gone == false` → no
///      reclaim (no false reclaim).
///   3. `committed_offset <= turn_floor` — the generation-aware committed-offset
///      authority covers NOTHING of this turn's body AT SCAN TIME. This is a
///      best-effort UNLOCKED first-line filter (it suppresses a downgrade for a
///      row whose watermark already advanced, e.g. a delivered-but-unmirrored
///      row, #2415). It is NOT the authoritative no-double-relay guard — a
///      NewMessage delivery landing AFTER this read advances only the watermark
///      and leaves the row orphan-shaped, so that downgrade proceeds and the
///      SEND-POINT committed re-gate (`idle_relay_range_action`) is what trims it.
pub(super) fn should_reclaim_orphaned_session_bound_relay(
    orphan_shape: bool,
    producer_gone: bool,
    committed_offset: u64,
    turn_floor: u64,
) -> bool {
    orphan_shape && producer_gone && committed_offset <= turn_floor
}

/// #3960 — reclaim an orphaned `SessionBoundRelay` TUI-direct row whose
/// claim-time producer has died. Returns `true` iff the orphaned owner was
/// downgraded to the bridge-adapter backstop (`None`).
///
/// The cheap row-shape gate runs first (the common live-row case returns
/// immediately without touching the producer registry or the delivery
/// authority). Only a stale orphan-shaped row consults the (re-checked) producer
/// liveness and the committed-offset authority, and only when both prove the
/// body was never delivered does it perform the flock-guarded owner downgrade.
/// The flock RMW's in-lock re-check rejects row-mutating in-window commits (the
/// watcher terminal-commit route) + identity/lifecycle races; the watermark-only
/// NewMessage commit leaves the row orphan-shaped and is trimmed downstream by
/// the send-point committed re-gate, never here.
pub(super) async fn reclaim_orphaned_session_bound_relay_if_dead(
    health_registry: &HealthRegistry,
    producers: &RelayProducerRegistry,
    provider: &ProviderKind,
    channel_id: u64,
    _iterating_session_name: &str,
    inflight: &InflightTurnState,
) -> bool {
    if !inflight::session_bound_relay_external_input_orphan_shape(inflight) {
        return false;
    }
    let Some(owner_session_name) = inflight
        .tmux_session_name
        .as_deref()
        .filter(|session| !session.trim().is_empty())
    else {
        return false;
    };
    // #3876 producer-liveness, re-checked at THIS tick: a live producer (the
    // original or a replacement) still owns delivery → never reclaim.
    let producer_gone = producers.get_live_producer(owner_session_name).is_none();
    if !producer_gone {
        return false;
    }
    let channel = ChannelId::new(channel_id);
    let Some(shared) = health_registry
        .shared_for_provider_on_channel(provider, channel)
        .await
        .or(health_registry.shared_for_provider(provider).await)
    else {
        return false;
    };
    let committed = effective_committed_offset(&shared, provider, channel, owner_session_name);
    let turn_floor = inflight.turn_start_offset.unwrap_or(inflight.last_offset);
    if !should_reclaim_orphaned_session_bound_relay(true, producer_gone, committed, turn_floor) {
        return false;
    }
    matches!(
        inflight::downgrade_orphaned_session_bound_relay_owner_locked(
            provider,
            channel_id,
            &InflightTurnIdentity::from_state(inflight),
            owner_session_name,
        ),
        inflight::OrphanRelayReclaimOutcome::Downgraded
    )
}

#[cfg(test)]
mod tests {
    use super::should_reclaim_orphaned_session_bound_relay as decide;
    use super::*;
    use serde_json::json;

    fn local_timestamp(unix: i64) -> String {
        use chrono::TimeZone;
        chrono::Local
            .timestamp_opt(unix, 0)
            .single()
            .expect("valid local timestamp")
            .format("%Y-%m-%d %H:%M:%S")
            .to_string()
    }

    fn stale_session_bound_relay_row(
        channel_id: u64,
        owner_session_name: &str,
    ) -> InflightTurnState {
        let stale_unix = chrono::Utc::now().timestamp()
            - (inflight::INFLIGHT_STALENESS_THRESHOLD_SECS as i64)
            - 1;
        serde_json::from_value(json!({
            "version": 9,
            "provider": "claude",
            "channel_id": channel_id,
            "channel_name": "adk-cc",
            "request_owner_user_id": 7,
            "user_msg_id": 7001,
            "current_msg_id": 0,
            "current_msg_len": 0,
            "user_text": "typed in TUI",
            "source": "text",
            "session_id": null,
            "tmux_session_name": owner_session_name,
            "output_path": "/tmp/claude-transcript.jsonl",
            "input_fifo_path": null,
            "last_offset": 0,
            "full_response": "",
            "response_sent_offset": 0,
            "started_at": local_timestamp(stale_unix),
            "updated_at": local_timestamp(stale_unix),
            "terminal_delivery_committed": false,
            "relay_owner_kind": "session_bound_relay",
            "turn_source": "external_input",
            "injected_prompt_message_id": 8001
        }))
        .expect("deserialize stale SessionBoundRelay row")
    }

    fn write_inflight_row_verbatim(
        agentdesk_root: &std::path::Path,
        provider: &ProviderKind,
        channel_id: u64,
        state: &InflightTurnState,
    ) {
        let path = agentdesk_root
            .join("runtime")
            .join("discord_inflight")
            .join(provider.as_str())
            .join(format!("{channel_id}.json"));
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create inflight dir");
        }
        let json = serde_json::to_string_pretty(state).expect("serialize inflight row");
        std::fs::write(&path, json).expect("write inflight row");
    }

    #[test]
    fn producer_dies_before_commit_with_undelivered_body_is_reclaimed() {
        // Orphan shape + producer gone + committed authority covers nothing of
        // the body (committed <= turn_floor) → reclaim.
        assert!(decide(true, true, 0, 0));
        assert!(decide(true, true, 5, 10));
        assert!(decide(true, true, 10, 10)); // boundary: nothing PAST the start
    }

    #[test]
    fn delivered_at_scan_time_suppressed_by_unlocked_first_line_filter() {
        // committed offset ALREADY advanced PAST the turn floor at scan time → the
        // unlocked first-line filter suppresses the downgrade. (This is best-effort
        // only; a delivery landing AFTER the scan is handled by the send-point
        // re-gate, asserted in `send_point_re_gate_*`.) Covers the #2415
        // delivered-but-unmirrored row when the watermark is already visible.
        assert!(!decide(true, true, 11, 10));
        assert!(!decide(true, true, u64::MAX, 0));
    }

    #[test]
    fn live_producer_that_survives_is_not_falsely_reclaimed() {
        // A live producer still owns delivery → never reclaim regardless of the
        // (irrelevant) offsets.
        assert!(!decide(true, false, 0, 10));
        assert!(!decide(true, false, 0, 0));
    }

    #[test]
    fn non_orphan_shape_is_never_reclaimed() {
        assert!(!decide(false, true, 0, 10));
        assert!(!decide(false, false, 0, 10));
    }

    #[tokio::test]
    async fn cross_session_reclaim_uses_row_owner_and_unblocks_iterating_session() {
        let _lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        struct EnvReset(Option<std::ffi::OsString>);
        impl Drop for EnvReset {
            fn drop(&mut self) {
                match self.0.take() {
                    Some(value) => unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", value) },
                    None => unsafe { std::env::remove_var("AGENTDESK_ROOT_DIR") },
                }
            }
        }
        let _env_reset = EnvReset(std::env::var_os("AGENTDESK_ROOT_DIR"));
        let temp = tempfile::TempDir::new().expect("temp runtime root");
        unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", temp.path()) };

        let provider = ProviderKind::Claude;
        let channel_id = 4_136_116;
        let owner_session = "AgentDesk-claude-vanished-owner-x";
        let iterating_session = "AgentDesk-claude-live-iterator-y";
        let inflight = stale_session_bound_relay_row(channel_id, owner_session);
        write_inflight_row_verbatim(temp.path(), &provider, channel_id, &inflight);

        let health_registry = HealthRegistry::new();
        let shared = crate::services::discord::make_shared_data_for_tests();
        health_registry
            .register(provider.as_str().to_string(), shared)
            .await;
        let producers = RelayProducerRegistry::new();

        assert!(
            reclaim_orphaned_session_bound_relay_if_dead(
                &health_registry,
                &producers,
                &provider,
                channel_id,
                iterating_session,
                &inflight,
            )
            .await,
            "iterating session Y must be able to reclaim a dead orphan owned by vanished session X"
        );

        let reloaded =
            inflight::load_inflight_state(&provider, channel_id).expect("downgraded row survives");
        assert_eq!(
            reloaded.effective_relay_owner_kind(),
            inflight::RelayOwnerKind::None,
            "dead-owner reclaim downgrades the row to ownerless; it never reassigns ownership to Y"
        );
        assert!(
            inflight::ownerless_external_input_inflight_is_stale(&reloaded),
            "Y's next idle tick sees a stale ownerless blocker and can proceed to its own JSONL backlog"
        );
    }

    #[tokio::test]
    async fn registered_but_shutdown_producer_is_reclaimed_as_dead() {
        use crate::services::cluster::session_matcher::{
            MatchedChannel, expected_rollout_path_for,
        };
        use crate::services::cluster::stream_relay::{DiscardSink, RelaySink, spawn_stream_relay};
        use std::sync::Arc;

        let _lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        struct EnvReset(Option<std::ffi::OsString>);
        impl Drop for EnvReset {
            fn drop(&mut self) {
                match self.0.take() {
                    Some(value) => unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", value) },
                    None => unsafe { std::env::remove_var("AGENTDESK_ROOT_DIR") },
                }
            }
        }
        let _env_reset = EnvReset(std::env::var_os("AGENTDESK_ROOT_DIR"));
        let temp = tempfile::TempDir::new().expect("temp runtime root");
        unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", temp.path()) };

        let provider = ProviderKind::Claude;
        let channel_id = 4_140_117;
        let owner_session = provider.build_tmux_session_name("dead-producer-owner");
        let iterating_session = "AgentDesk-claude-live-iterator-z";
        let inflight = stale_session_bound_relay_row(channel_id, &owner_session);
        write_inflight_row_verbatim(temp.path(), &provider, channel_id, &inflight);

        let health_registry = HealthRegistry::new();
        let shared = crate::services::discord::make_shared_data_for_tests();
        health_registry
            .register(provider.as_str().to_string(), shared)
            .await;
        let producers = RelayProducerRegistry::new();
        let matched = MatchedChannel {
            channel_id: "dead-producer-owner".to_string(),
            agent_id: "agent-dead-producer-owner".to_string(),
            provider: provider.clone(),
            expected_session_name: owner_session.clone(),
            expected_rollout_path: expected_rollout_path_for(&owner_session),
        };
        let sink: Arc<dyn RelaySink> = Arc::new(DiscardSink);
        let handle = spawn_stream_relay(matched, sink);
        producers.register(owner_session.clone(), handle.producer());
        handle.shutdown().await;
        assert!(
            producers
                .get_producer(&owner_session)
                .is_some_and(|producer| !producer.is_alive()),
            "registry must still contain a non-live producer before reclaim"
        );

        assert!(
            reclaim_orphaned_session_bound_relay_if_dead(
                &health_registry,
                &producers,
                &provider,
                channel_id,
                iterating_session,
                &inflight,
            )
            .await,
            "a registered but shutdown producer is dead for orphan reclaim"
        );

        let reloaded =
            inflight::load_inflight_state(&provider, channel_id).expect("downgraded row survives");
        assert_eq!(
            reloaded.effective_relay_owner_kind(),
            inflight::RelayOwnerKind::None,
            "dead registered producer reclaim downgrades the row to ownerless"
        );
    }

    /// #3960 — the AUTHORITATIVE no-double-relay guard for the watermark-only
    /// NewMessage commit. After the orphan is downgraded (the in-lock shape
    /// re-check cannot see a watermark-only delivery — see
    /// `inflight::orphan_relay_reclaim::tests::locked_downgrade_proceeds_for_watermark_only_newmessage_commit`),
    /// the re-delivery path re-reads the FRESH `effective_committed_offset` —
    /// already advanced past the body by `advance_after_confirmed_post` — and
    /// `idle_relay_range_action` skips the whole, already-relayed body.
    #[test]
    fn send_point_re_gate_skips_a_fully_delivered_body() {
        use super::super::idle_jsonl::{IdleRelayRangeAction, idle_relay_range_action};
        let init = "{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"s1\"}\n";
        let body = "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"answer\"}]}}\n";
        let full = format!("{init}{body}");
        let bytes = full.as_bytes();
        let turn_floor = 0u64;
        let body_end = bytes.len() as u64;
        // The NewMessage route delivered the WHOLE body and advanced the shared
        // watermark to/past body_end (watermark-only; the row stayed orphan-shaped).
        let committed_advanced = body_end;
        assert_eq!(
            idle_relay_range_action(
                bytes,
                turn_floor,
                body_end,
                committed_advanced,
                false,
                false,
                false
            ),
            IdleRelayRangeAction::SkipAlreadyRelayed,
            "the send-point re-gate skips a body already past the watermark — no double-relay"
        );
    }

    /// #3960 — partial watermark-only commit: the send-point re-gate delivers ONLY
    /// the uncommitted tail `[committed, body_end)`, never re-posting the already
    /// delivered prefix (no duplicate).
    #[test]
    fn send_point_re_gate_sends_only_the_uncommitted_tail() {
        use super::super::idle_jsonl::{IdleRelayRangeAction, idle_relay_range_action};
        let init = "{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"s1\"}\n";
        let body = "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"answer\"}]}}\n";
        let full = format!("{init}{body}");
        let bytes = full.as_bytes();
        let turn_floor = 0u64;
        let body_end = bytes.len() as u64;
        // The watermark advanced PAST turn_floor but not to body_end → re-deliver
        // ONLY the uncommitted tail.
        let committed_advanced = init.len() as u64;
        assert!(turn_floor < committed_advanced && committed_advanced < body_end);
        assert_eq!(
            idle_relay_range_action(
                bytes,
                turn_floor,
                body_end,
                committed_advanced,
                false,
                false,
                false
            ),
            IdleRelayRangeAction::SendSuffixFrom(committed_advanced),
            "the send-point re-gate delivers only the uncommitted tail — no duplicate of the \
             delivered prefix"
        );
    }
}
