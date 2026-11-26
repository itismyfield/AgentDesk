//! The idle-kill policy's kill-tmux call on a Bound Herdr row: the pane closes and the row retires
//! with its provider session kept only when no dispatch, turn or inflight row holds the channel.

use super::*;

const IDLE: &str = "idle 7시간 초과 — 자동 정리";

impl Node {
    /// The row as idle-kill selects it: idle for 7 hours, its provider session recorded.
    async fn idle(rig: &herdr::P11TerminationRig, dispatch: Option<&str>) -> Self {
        let node = Self::bind(rig, herdr::HostedState::Bound, dispatch).await;
        node.sql("UPDATE sessions SET status = 'idle', claude_session_id = 'kept-session', last_heartbeat = NOW() - INTERVAL '7 hours' WHERE session_key = $1 AND $2::TEXT IS NULL", None).await;
        node
    }

    async fn idle_kill(&self) -> Value {
        let key = self.key.replace('/', "%2F").replace(':', "%3A");
        let body = json!({"reason": IDLE, "minimum_idle_minutes": 360});
        let (status, reply) = self.post(&format!("/sessions/{key}/kill-tmux"), body).await;
        assert_eq!(status, StatusCode::OK, "{reply}");
        reply
    }

    /// The pane closed once, the row retired and left idle, and the provider session stayed.
    async fn assert_ended(&self, rig: &herdr::P11TerminationRig, reply: &Value) {
        assert_eq!(reply["tmux_killed"], true, "{reply}");
        assert_eq!(reply["session_row_disconnected"], true, "{reply}");
        let after = self.effects(rig).await;
        assert_eq!(
            (after.closes(), after.row, after.session),
            (1, retired(), disconnected())
        );
        let kept: Option<String> =
            sqlx::query_scalar("SELECT claude_session_id FROM sessions WHERE session_key = $1")
                .bind(&self.key)
                .fetch_one(&self.pool)
                .await
                .unwrap();
        assert_eq!(kept.as_deref(), Some("kept-session"));
        let alarms = crate::services::observability::idle_cleanup_preserved_count(&self.key);
        assert_eq!(alarms, 0, "no preserve alarm for a Herdr row");
    }
}

// Each holder refuses for its own reason with nothing written, then the free channel ends.
#[test]
fn idle_kill_ends_only_an_unheld_herdr_execution_pg() {
    let Some((rig, runtime)) = rig_child("idle_kill_ends_only_an_unheld_herdr_execution_pg") else {
        return;
    };
    let _root = crate::config::TestRuntimeRootGuard::new();
    runtime.block_on(async {
        let node = Node::idle(&rig, Some(DISPATCH)).await;
        let refused = async |node: &Node, why: &str| {
            let reply = node.idle_kill().await;
            assert_eq!(
                (&reply["tmux_killed"], &reply["detail"]),
                (&json!(false), &json!(why))
            );
            assert!(
                node.effects(&rig).await.writes.is_empty(),
                "{why}: no close"
            );
        };
        refused(&node, "active dispatch").await;
        node.sql(
            "UPDATE sessions SET active_dispatch_id = $2 WHERE session_key = $1",
            None,
        )
        .await;
        node.start(51, "turn-a", None).await;
        refused(&node, "inflight row open").await;
        let row = inflight::load_inflight_state(&ProviderKind::Claude, CHANNEL).unwrap();
        inflight::delete_inflight_state_file_checked(&ProviderKind::Claude, CHANNEL);
        let unmatched = "active turn has no matching captured inflight evidence";
        refused(&node, unmatched).await;
        inflight::save_inflight_state(&row).unwrap();
        let token = test_turn::active(&node.shared, channel()).await.unwrap();
        test_turn::finish_naturally(&node.shared, channel(), 51, "turn-a", &token).await;
        // A restart can leave the inflight row with no turn in the mailbox.
        inflight::save_inflight_state(&row).unwrap();
        refused(&node, "inflight row open").await;
        inflight::delete_inflight_state_file_checked(&ProviderKind::Claude, CHANNEL);
        let reply = node.idle_kill().await;
        node.assert_ended(&rig, &reply).await;
        node.finish().await;
    });
}

// A channel no runtime serves any more is ended by the runtime of the bot owning the row.
#[test]
fn idle_kill_ends_a_herdr_execution_no_runtime_serves_pg() {
    let Some((rig, runtime)) = rig_child("idle_kill_ends_a_herdr_execution_no_runtime_serves_pg")
    else {
        return;
    };
    let _root = crate::config::TestRuntimeRootGuard::new();
    runtime.block_on(async {
        let node = Node::idle(&rig, None).await;
        node.shared.settings.write().await.allowed_channel_ids = vec![OTHER];
        let provider = ProviderKind::Claude;
        let served = node
            .registry
            .shared_for_provider_on_channel(&provider, channel());
        assert!(served.await.is_none(), "no runtime serves the channel");
        let reply = node.idle_kill().await;
        node.assert_ended(&rig, &reply).await;
        node.finish().await;
    });
}
