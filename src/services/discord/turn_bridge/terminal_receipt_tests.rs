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
    run_with(pool, pool, channel, ending).await
}

/// `run` whose bridge writes through `storage` while the checks read `pool`.
async fn run_with(
    storage: &sqlx::PgPool,
    pool: &sqlx::PgPool,
    channel: u64,
    ending: Ending,
) -> Ran {
    let mut shared = discord::make_shared_data_for_tests_with_storage(Some(storage.clone()));
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

/// A pool whose every acquire fails at once, as when the database is unreachable.
fn blocked_pool() -> sqlx::PgPool {
    sqlx::postgres::PgPoolOptions::new()
        .acquire_timeout(Duration::from_millis(50))
        .connect_lazy("postgres://agentdesk@127.0.0.1:1/blocked")
        .unwrap()
}

/// While the receipt cannot commit, the bridge keeps the ended execution's row marked rather
/// than removing it; the origin already reads terminal from the mark, and the next admission on
/// the channel records exactly that receipt and removes the row.
#[tokio::test(flavor = "current_thread")]
async fn a_blocked_commit_keeps_the_row_marked_until_admission_records_it_pg() {
    let root = tempfile::tempdir().unwrap();
    let _env = crate::config::set_agentdesk_root_for_test(root.path());
    let _legacy = crate::services::tui_o::cutover::test_override::force_channels(&[]);
    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let channel = 7_340_530;
    let execution = format!("discord:{channel}:{}", 7_340_000_000 + channel % 1_000);

    let ran = run_with(&blocked_pool(), &pool, channel, Ending::Answer).await;
    assert_eq!(ran.receipt, None, "nothing reached the database");
    assert!(ran.row_left, "the row stays while its receipt is owed");
    let row = inflight::load_inflight_state(&ProviderKind::Codex, channel).unwrap();
    let mark = row.terminal_receipt.expect("the kept row carries the mark");
    assert_eq!(mark.outcome, crate::db::turns::TerminalOutcome::Completed);
    let Some(InputResult::Terminal(found)) = ran.after else {
        panic!("the marked row reads terminal: {:?}", ran.after);
    };
    assert_eq!(
        (found.turn_id.as_str(), found.terminal_outcome.as_str()),
        (execution.as_str(), "completed")
    );

    let healthy = discord::make_shared_data_for_tests_with_storage(Some(pool.clone()));
    let settled = crate::services::discord::receipt_settle::settle_marked_row(
        &healthy,
        &ProviderKind::Codex,
        channel,
    )
    .await;
    assert!(settled, "admission records the mark");
    assert!(inflight::load_inflight_state(&ProviderKind::Codex, channel).is_none());
    let recorded: (Vec<String>, String, chrono::DateTime<chrono::Utc>) = sqlx::query_as(
        "SELECT consumed_origin_ids, terminal_outcome, terminal_confirmed_at FROM turns
         WHERE turn_id = $1",
    )
    .bind(&execution)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(recorded.0, vec![ORIGIN.to_string()]);
    assert_eq!(recorded.1, "completed");
    assert_eq!(
        recorded.2.timestamp_micros(),
        mark.confirmed_at.timestamp_micros()
    );
    pool.close().await;
    db.drop().await;
}

/// An entry abort ends the execution as failed only when the unwind released this turn's own
/// claim and the row is still solely its own; any hand-over records nothing here.
#[tokio::test(flavor = "current_thread")]
async fn an_entry_abort_fails_only_its_own_released_row() {
    let root = tempfile::tempdir().unwrap();
    let _env = crate::config::set_agentdesk_root_for_test(root.path());
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum Abort {
        OwnRow,
        SuccessorRow,
        RestartMarked,
        WatcherRelay,
        NoClaim,
    }
    for (n, abort) in [
        Abort::OwnRow,
        Abort::SuccessorRow,
        Abort::RestartMarked,
        Abort::WatcherRelay,
        Abort::NoClaim,
    ]
    .into_iter()
    .enumerate()
    {
        let mut shared = discord::make_shared_data_for_tests_with_storage(Some(blocked_pool()));
        Arc::get_mut(&mut shared).unwrap().provider = ProviderKind::Codex;
        let channel = 7_340_540 + n as u64;
        let cancel = Arc::new(CancelToken::new());
        let message = 7_340_100_000 + n as u64;
        let mut row = InflightTurnState::new(
            ProviderKind::Codex,
            channel,
            None,
            1,
            message,
            18,
            "entry prompt".into(),
            None,
            None,
            None,
            None,
            0,
        );
        row.turn_nonce = cancel.turn_nonce().map(str::to_owned);
        row.input_origin_ids = vec![ORIGIN.to_string()];
        match abort {
            Abort::SuccessorRow => row.turn_nonce = Some("successor-turn".into()),
            Abort::RestartMarked => row.restart_mode = Some(InflightRestartMode::DrainRestart),
            Abort::WatcherRelay => {
                row.relay_owner_kind = inflight::RelayOwnerKind::Watcher;
                row.watcher_owner_channel_id = Some(channel);
            }
            _ => {}
        }
        inflight::save_inflight_state(&row).unwrap();
        let channel_id = ChannelId::new(channel);
        if abort != Abort::NoClaim {
            let start = discord::mailbox_try_start_turn(
                &shared,
                channel_id,
                cancel.clone(),
                UserId::new(1),
                MessageId::new(message),
            );
            assert!(start.await);
        }

        super::super::bridge_entry_persist::unwind_and_notify(&shared, channel_id, &cancel, None)
            .await;

        let mark = inflight::load_inflight_state_read_only(&ProviderKind::Codex, channel)
            .and_then(|row| row.terminal_receipt)
            .map(|mark| mark.outcome);
        let expected =
            (abort == Abort::OwnRow).then_some(crate::db::turns::TerminalOutcome::Failed);
        assert_eq!(mark, expected, "{abort:?}");
        shared.mailboxes.remove_fixture_for_test(channel_id);
    }
}
