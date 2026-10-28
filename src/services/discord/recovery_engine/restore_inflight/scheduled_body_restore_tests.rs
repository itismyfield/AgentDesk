//! Restart recovery delivering a scheduled snapshot answer through each direct body path.

use super::*;
use crate::services::discord::host_teardown_gate::test_support::{
    Stored, channel_key, seed, shared_on,
};
use crate::services::discord::recovery_engine::o_cut_recorder;
use crate::services::discord::restart_report::{self, RestartReportContext};
use crate::services::discord::scheduled_turn::{SKIP_FALLBACK_SETTLE_IN_TASK, ScheduledTurnMark};
use crate::services::scheduled_messages::turn_completion::tests::{
    delivery_state, launch_agent_delivery, poll_once,
};
use crate::services::session_host::test_support::{InjectedLivenessGuard, InjectedPresenceGuard};
use crate::services::session_host::{HostLiveness, HostPresence, HostSessionRef};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BodyPath {
    /// A restart report and a success result written while the process was down.
    CompletedDuringDowntime,
    /// A captured, never-sent `full_response` with a drained success result.
    CapturedFullResponse,
    /// No captured text: the answer comes from the output's success result.
    OutputTerminalSuccess,
}

/// Each direct body path records the outcome before its visible completion drops
/// the row: with the late transcript-side record cut away (a crash right after the
/// visible completion), the delivery still holds the outcome and the poll settles it.
/// The isolated answer never becomes a channel transcript.
#[tokio::test]
async fn each_direct_body_restore_records_before_its_visible_completion_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let _legacy = crate::services::tui_o::cutover::test_override::force_channels(&[]);
    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let mut shared = shared_on(&pool).await;
    Arc::get_mut(&mut shared)
        .expect("fresh runtime")
        .ui
        .status_panel_v2_enabled = true;
    let provider = ProviderKind::Claude;
    let transcripts = tempfile::tempdir().unwrap();
    let mut probes = Vec::new();
    let mut cases = Vec::new();
    for (n, path) in [
        BodyPath::CompletedDuringDowntime,
        BodyPath::CapturedFullResponse,
        BodyPath::OutputTerminalSuccess,
    ]
    .into_iter()
    .enumerate()
    {
        let channel = 1_479_742_100_000_000_000 + n as u64;
        let name = provider.build_tmux_session_name(&format!("i7420-body-{n}"));
        seed(
            &pool,
            &channel_key(&shared, &name),
            &name,
            channel,
            Stored::Legacy,
        )
        .await;
        let session = HostSessionRef::tmux(&name);
        probes.push((
            InjectedLivenessGuard::set(session, HostLiveness::DeadOrAbsent),
            InjectedPresenceGuard::set(session, HostPresence::Missing),
        ));
        let turn = format!("i7420-body-turn-{n}");
        launch_agent_delivery(&pool, &format!("i7420-body-{n}"), &turn, true, "fail").await;
        let body = format!("I7420 scheduled snapshot answer via {path:?}");
        let output = transcripts.path().join(format!("{n}.jsonl"));
        let assistant = serde_json::json!({"type": "assistant", "message": {"role": "assistant",
            "content": [{"type": "text", "text": body}]}});
        let result = serde_json::json!({"type": "result", "subtype": "success", "result": body});
        std::fs::write(&output, format!("{assistant}\n{result}\n")).unwrap();
        let mut state = inflight::InflightTurnState::new(
            provider.clone(),
            channel,
            None,
            7,
            channel + 1,
            channel + 2,
            "scheduled prompt".into(),
            Some("snapshot-provider-session".into()),
            Some(name),
            Some(output.display().to_string()),
            None,
            0,
        );
        state.born_generation = 0;
        state.turn_start_offset = Some(0);
        state.scheduled = ScheduledTurnMark::Scheduled {
            turn_id: turn.clone(),
            isolated: true,
            outcome: None,
        };
        if path == BodyPath::CapturedFullResponse {
            state.full_response = body.clone();
        }
        if path == BodyPath::CompletedDuringDowntime {
            let context = RestartReportContext::from_bridge(provider.clone(), channel, None, None);
            restart_report::announce_restart(&context).expect("restart report");
        }
        inflight::save_inflight_state_create_new(&state).unwrap();
        cases.push((path, channel, turn, body));
    }
    let discord = o_cut_recorder::start(cases[0].1).await;

    SKIP_FALLBACK_SETTLE_IN_TASK
        .scope(
            (),
            restore_inflight_turns(&discord.http, &shared, &provider),
        )
        .await;

    let shown = discord.contents();
    for (path, channel, turn, body) in &cases {
        assert!(
            shown.iter().any(|content| content.contains(body.as_str())),
            "{path:?} relayed its answer: {shown:?}"
        );
        assert!(
            inflight::load_inflight_state(&provider, *channel).is_none(),
            "{path:?} completed and dropped its row"
        );
        assert_eq!(
            delivery_state(&pool, turn).await,
            ("running".to_string(), Some("delivered".to_string())),
            "{path:?} recorded before the visible completion"
        );
    }
    let transcripts: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM session_transcripts")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        transcripts, 0,
        "an isolated answer writes no channel transcript"
    );
    assert!(poll_once(&pool).await);
    for (path, _, turn, _) in &cases {
        assert_eq!(delivery_state(&pool, turn).await.0, "sent", "{path:?}");
    }
    drop(probes);
    pool.close().await;
    db.drop().await;
}
