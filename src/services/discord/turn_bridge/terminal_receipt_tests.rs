//! The real bridge leaves an execution receipt only where it actually ends the execution, carrying
//! the origins its inflight row consumed and no answer text.
use std::sync::Arc;
use std::time::Duration;

use super::*;
use crate::services::agent_protocol::StreamMessage;
use crate::services::agents::input_result::{InputResult, OriginChannel, lookup};
use crate::services::discord::formatting::ReplaceLongMessageOutcome;
use crate::services::discord::gateway;
use crate::services::discord::inflight::{self, InflightTurnState};
use crate::services::discord::{self as discord, InflightRestartMode};
use crate::services::provider::CancelToken;
use crate::services::turn_orchestrator::Intervention;

const ORIGIN: &str = "G-bridge-origin";

/// A Discord gateway that delivers directly and always succeeds, so an answer is committed.
struct DirectGateway;

impl gateway::TurnGateway for DirectGateway {
    fn send_message<'a>(
        &'a self,
        _: ChannelId,
        _: &'a str,
    ) -> gateway::GatewayFuture<'a, Result<MessageId, String>> {
        Box::pin(async { Ok(MessageId::new(7_340_999)) })
    }
    fn edit_message<'a>(
        &'a self,
        _: ChannelId,
        _: MessageId,
        _: &'a str,
    ) -> gateway::GatewayFuture<'a, Result<(), String>> {
        Box::pin(async { Ok(()) })
    }
    fn replace_message_with_outcome<'a>(
        &'a self,
        _: ChannelId,
        _: MessageId,
        _: &'a str,
    ) -> gateway::GatewayFuture<'a, Result<ReplaceLongMessageOutcome, String>> {
        Box::pin(async { Ok(ReplaceLongMessageOutcome::EditedOriginal) })
    }
    fn schedule_retry_with_history<'a>(
        &'a self,
        _: ChannelId,
        _: MessageId,
        _: &'a str,
    ) -> gateway::GatewayFuture<'a, ()> {
        Box::pin(async {})
    }
    fn dispatch_queued_turn<'a>(
        &'a self,
        _: ChannelId,
        _: &'a Intervention,
        _: &'a str,
        _: bool,
        _: Option<Arc<crate::services::turn_orchestrator::DispatchLease>>,
    ) -> gateway::GatewayFuture<'a, Result<(), String>> {
        Box::pin(async { Ok(()) })
    }
    fn validate_live_routing<'a>(
        &'a self,
        _: ChannelId,
    ) -> gateway::GatewayFuture<'a, Result<(), String>> {
        Box::pin(async { Ok(()) })
    }
    fn requester_mention(&self) -> Option<String> {
        None
    }
    fn can_chain_locally(&self) -> bool {
        false
    }
    fn can_deliver_directly(&self) -> bool {
        true
    }
    fn bot_owner_provider(&self) -> Option<ProviderKind> {
        Some(ProviderKind::Codex)
    }
}

/// What happens to the turn once the bridge saved its entry.
#[derive(Clone, Copy, Debug)]
enum Ending {
    Answer,
    Empty,
    ProviderError,
    UserCancel,
    RestartHandoff,
    SessionDiedRetry,
}

struct Ran {
    receipt: Option<(Vec<String>, Option<String>)>,
    row_left: bool,
    running: Option<InputResult>,
    after: Option<InputResult>,
}

async fn run(pool: &sqlx::PgPool, channel: u64, ending: Ending) -> Ran {
    let mut shared = discord::make_shared_data_for_tests_with_storage(Some(pool.clone()));
    let writable = Arc::get_mut(&mut shared).unwrap();
    writable.provider = ProviderKind::Codex;
    writable.restart.current_generation = 17;
    let cancel = Arc::new(CancelToken::new());
    let message = 7_340_000_000 + channel % 1_000;
    let mut row = InflightTurnState::new(
        ProviderKind::Codex,
        channel,
        None,
        1,
        message,
        18,
        "receipt prompt".into(),
        None,
        None,
        None,
        None,
        0,
    );
    row.turn_nonce = cancel.turn_nonce().map(str::to_owned);
    row.input_origin_ids = vec![ORIGIN.to_string()];
    let (channel_id, message_id) = (ChannelId::new(channel), MessageId::new(message));
    let start = discord::mailbox_try_start_turn(
        &shared,
        channel_id,
        cancel.clone(),
        UserId::new(1),
        message_id,
    );
    assert!(start.await);
    discord::increment_global_active(&shared, "test_bridge_admission");
    inflight::save_inflight_state(&row).unwrap();
    let entry = inflight::load_inflight_state(&ProviderKind::Codex, channel).unwrap();

    let mut bridge = super::super::bridge_entry_persist::entry_abort_mailbox_tests::seed_context(
        "",
        row.clone(),
    );
    bridge.gateway = Arc::new(DirectGateway);
    bridge.user_msg_id = Some(message_id);
    bridge.user_text_owned = "receipt prompt".into();
    let (completion_tx, completion_rx) = tokio::sync::oneshot::channel();
    bridge.completion_tx = Some(completion_tx);
    let (tx, rx) = std::sync::mpsc::channel();
    super::super::bridge_entry_persist::spawn_turn_bridge(
        shared.clone(),
        cancel.clone(),
        rx,
        bridge,
    );
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while !inflight::load_inflight_state(&ProviderKind::Codex, channel)
        .is_some_and(|saved| saved.save_generation > entry.save_generation)
    {
        assert!(
            std::time::Instant::now() < deadline,
            "bridge never saved its entry"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let channels = [OriginChannel {
        provider: "codex".into(),
        channel_id: channel,
    }];
    let running = lookup(pool, "receipt-agent", ORIGIN, &channels)
        .await
        .unwrap();

    let frames = match ending {
        Ending::Answer => vec![
            StreamMessage::Text {
                content: "final answer".into(),
            },
            StreamMessage::Done {
                result: "final answer".into(),
                session_id: None,
            },
        ],
        Ending::Empty => vec![StreamMessage::Done {
            result: String::new(),
            session_id: None,
        }],
        Ending::ProviderError => vec![StreamMessage::Error {
            message: "provider exited".into(),
            stdout: String::new(),
            stderr: String::new(),
            exit_code: Some(1),
        }],
        Ending::SessionDiedRetry => vec![StreamMessage::Done {
            result: "__session_died_retry__".into(),
            session_id: None,
        }],
        Ending::UserCancel | Ending::RestartHandoff => Vec::new(),
    };
    if matches!(ending, Ending::RestartHandoff) {
        cancel.set_restart_mode(Some(InflightRestartMode::HotSwapHandoff));
    }
    if matches!(ending, Ending::UserCancel | Ending::RestartHandoff) {
        cancel
            .cancelled
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }
    for frame in frames {
        tx.send(frame).unwrap();
    }
    drop(tx);
    tokio::time::timeout(Duration::from_secs(20), completion_rx)
        .await
        .expect("the bridge reports")
        .unwrap();
    let receipt: Option<(Vec<String>, Option<String>)> = sqlx::query_as(
        "SELECT consumed_origin_ids, terminal_outcome FROM turns
         WHERE turn_id = $1 AND terminal_outcome IS NOT NULL",
    )
    .bind(format!("discord:{channel}:{message}"))
    .fetch_optional(pool)
    .await
    .unwrap();
    let row_left = inflight::load_inflight_state(&ProviderKind::Codex, channel).is_some();
    let after = lookup(pool, "receipt-agent", ORIGIN, &channels)
        .await
        .unwrap();
    shared.mailboxes.remove_fixture_for_test(channel_id);
    Ran {
        receipt,
        row_left,
        running,
        after,
    }
}

/// Each real ending of a managed execution commits one receipt naming the consumed origin,
/// whether or not any answer text exists, and the origin lookup follows the row to it.
#[tokio::test(flavor = "current_thread")]
async fn each_real_ending_commits_its_receipt_before_the_row_goes_pg() {
    let root = tempfile::tempdir().unwrap();
    let _env = crate::config::set_agentdesk_root_for_test(root.path());
    let _legacy = crate::services::tui_o::cutover::test_override::force_channels(&[]);
    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    for (n, (ending, outcome)) in [
        (Ending::Answer, "completed"),
        (Ending::Empty, "empty"),
        (Ending::ProviderError, "failed"),
        (Ending::UserCancel, "cancelled"),
    ]
    .into_iter()
    .enumerate()
    {
        let channel = 7_340_510 + n as u64;
        let ran = run(&pool, channel, ending).await;
        let execution = format!("discord:{channel}:{}", 7_340_000_000 + channel % 1_000);
        assert_eq!(
            ran.running,
            Some(InputResult::Running {
                channel_id: channel,
                execution_id: execution.clone(),
            }),
            "{ending:?}: the running row names the origin"
        );
        assert_eq!(
            ran.receipt,
            Some((vec![ORIGIN.to_string()], Some(outcome.to_string()))),
            "{ending:?}"
        );
        assert!(!ran.row_left, "{ending:?}: the execution's row was cleared");
        let Some(InputResult::Terminal(found)) = ran.after else {
            panic!("{ending:?}: the origin reads its receipt: {:?}", ran.after);
        };
        assert_eq!(
            (found.turn_id.as_str(), found.terminal_outcome.as_str()),
            (execution.as_str(), outcome),
            "{ending:?}"
        );
    }
    pool.close().await;
    db.drop().await;
}

/// A restart hand-off and a session-died retry keep the execution for another owner: no
/// receipt, even though the bridge saw a cancel flag or a Done frame.
#[tokio::test(flavor = "current_thread")]
async fn a_kept_execution_leaves_no_receipt_pg() {
    let root = tempfile::tempdir().unwrap();
    let _env = crate::config::set_agentdesk_root_for_test(root.path());
    let _legacy = crate::services::tui_o::cutover::test_override::force_channels(&[]);
    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    for (n, ending) in [Ending::RestartHandoff, Ending::SessionDiedRetry]
        .into_iter()
        .enumerate()
    {
        let ran = run(&pool, 7_340_520 + n as u64, ending).await;
        assert_eq!(ran.receipt, None, "{ending:?}");
        assert!(
            !matches!(ran.after, Some(InputResult::Terminal(_))),
            "{ending:?}: no terminal is reported: {:?}",
            ran.after
        );
    }
    pool.close().await;
    db.drop().await;
}

/// The origin list is part of the row only when it holds something, so a row written without
/// one keeps its earlier bytes and an earlier row reads with none.
#[test]
fn an_inflight_row_without_origins_keeps_its_earlier_shape() {
    let row = InflightTurnState::new(
        ProviderKind::Claude,
        7_340_600,
        None,
        1,
        7_340_601,
        18,
        "text".into(),
        None,
        None,
        None,
        None,
        0,
    );
    let encoded = serde_json::to_value(&row).unwrap();
    assert!(encoded.get("input_origin_ids").is_none(), "{encoded}");
    let mut earlier = encoded.clone();
    earlier.as_object_mut().unwrap().remove("input_origin_ids");
    let read: InflightTurnState = serde_json::from_value(earlier).unwrap();
    assert!(read.input_origin_ids.is_empty());
    assert_eq!(serde_json::to_value(&read).unwrap(), encoded);
    let mut carried = row;
    carried.input_origin_ids = vec![ORIGIN.into()];
    let back: InflightTurnState =
        serde_json::from_str(&serde_json::to_string(&carried).unwrap()).unwrap();
    assert_eq!(back.input_origin_ids, vec![ORIGIN.to_string()]);
}

/// The outcome of a restart-mode or retry ending is decided by the bridge's branch, not by
/// this table; the table only names terminals and never a hand-off.
#[test]
fn hand_off_labels_name_no_terminal() {
    for label in [
        "recovery_retry",
        "watcher_relay",
        "standby_relay",
        "tmux_handoff",
        "delivery_pending",
        "delivery_unresolved",
    ] {
        assert_eq!(super::terminal_receipt_outcome(label), None, "{label}");
    }
}
