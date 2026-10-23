//! A cold Herdr turn whose executor `Err` reaches the bridge with no stop: on O's channel as on a
//! Legacy one, the bridge's failure notice stays visible and the turn ends failed, not completed.
use super::*;
use crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui;
use crate::services::discord::abandon_request_store as abandon;
use crate::services::discord::turn_bridge::single_message_footer::SEPARATE_PANEL_FOR_TESTS;
use crate::services::tui_o::cutover::test_override;
use std::collections::BTreeMap;

/// The message intake left as the turn's placeholder; the bridge answers there.
const PLACEHOLDER: u64 = 1_440_000_000_000_000_018;
const POSTED: u64 = 1_440_000_000_000_100_000;
const SUMMARY: &str = "provider가 응답을 완료하지 못했어요.";

/// Every Discord message the bridge wrote, by id, with its final content and whether it was deleted.
#[derive(Default)]
struct Board {
    messages: Mutex<BTreeMap<u64, String>>,
    deleted: Mutex<Vec<u64>>,
    next: std::sync::atomic::AtomicU64,
}

impl Board {
    fn write(&self, id: MessageId, content: &str) {
        self.messages
            .lock()
            .unwrap()
            .insert(id.get(), content.to_owned());
    }
    fn content(&self, id: u64) -> Option<String> {
        self.messages.lock().unwrap().get(&id).cloned()
    }
    fn finals(&self) -> BTreeMap<u64, String> {
        self.messages.lock().unwrap().clone()
    }
    fn deleted(&self) -> Vec<u64> {
        self.deleted.lock().unwrap().clone()
    }
}

impl TurnGateway for Board {
    fn send_message<'a>(
        &'a self,
        _: ChannelId,
        content: &'a str,
    ) -> GatewayFuture<'a, Result<MessageId, String>> {
        let id = MessageId::new(POSTED + self.next.fetch_add(1, Ordering::SeqCst));
        self.write(id, content);
        Box::pin(async move { Ok(id) })
    }
    fn edit_message<'a>(
        &'a self,
        _: ChannelId,
        id: MessageId,
        content: &'a str,
    ) -> GatewayFuture<'a, Result<(), String>> {
        self.write(id, content);
        Box::pin(async { Ok(()) })
    }
    fn delete_message<'a>(
        &'a self,
        _: ChannelId,
        id: MessageId,
    ) -> GatewayFuture<'a, Result<(), String>> {
        self.deleted.lock().unwrap().push(id.get());
        Box::pin(async { Ok(()) })
    }
    fn replace_message_with_outcome<'a>(
        &'a self,
        _: ChannelId,
        id: MessageId,
        content: &'a str,
    ) -> GatewayFuture<'a, Result<ReplaceLongMessageOutcome, String>> {
        self.write(id, content);
        Box::pin(async { Ok(ReplaceLongMessageOutcome::EditedOriginal) })
    }
    fn schedule_retry_with_history<'a>(
        &'a self,
        _: ChannelId,
        _: MessageId,
        _: &'a str,
    ) -> GatewayFuture<'a, ()> {
        panic!("a provider failure schedules no retry")
    }
    fn dispatch_queued_turn<'a>(
        &'a self,
        _: ChannelId,
        _: &'a discord::Intervention,
        _: &'a str,
        _: bool,
        _: Option<Arc<crate::services::turn_orchestrator::DispatchLease>>,
    ) -> GatewayFuture<'a, Result<(), String>> {
        Box::pin(async { Ok(()) })
    }
    fn validate_live_routing<'a>(&'a self, _: ChannelId) -> GatewayFuture<'a, Result<(), String>> {
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
        None
    }
}

/// Restores the process-wide footer mode for the next test on this thread.
struct SeparatePanel;

impl SeparatePanel {
    fn set(separate: bool) -> Self {
        SEPARATE_PANEL_FOR_TESTS.set(separate);
        Self
    }
}

impl Drop for SeparatePanel {
    fn drop(&mut self) {
        SEPARATE_PANEL_FOR_TESTS.set(false);
    }
}

/// A cold Claude TUI turn as intake leaves it, with the status panel enabled.
async fn cold_claude(channel_id: u64) -> Cold {
    let provider = ProviderKind::Claude;
    let mut shared = discord::make_shared_data_for_tests();
    let writable = Arc::get_mut(&mut shared).unwrap();
    writable.provider = provider.clone();
    writable.restart.current_generation = 17;
    (
        writable.ui.status_panel_v2_enabled,
        writable.ui.two_message_panel_enabled,
    ) = (true, true);
    let channel = ChannelId::new(channel_id);
    let cancel = Arc::new(CancelToken::new());
    let owner = owner(&shared, &provider, channel_id);
    let mut row = InflightTurnState::new(
        provider.clone(),
        channel_id,
        None,
        1,
        1_430_000_000_000_000_000 + channel_id % 1_000,
        PLACEHOLDER,
        String::new(),
        None,
        Some(owner.logical_key.clone()),
        None,
        None,
        0,
    );
    row.runtime_kind = Some(ClaudeTui);
    row.turn_nonce = cancel.turn_nonce().map(str::to_owned);
    cancel.prepare_herdr_interrupt(provider.clone(), &owner);
    let message = MessageId::new(row.user_msg_id);
    let start =
        discord::mailbox_try_start_turn(&shared, channel, cancel.clone(), UserId::new(1), message);
    assert!(start.await);
    discord::increment_global_active(&shared, "test_bridge_admission");
    discord::inflight::save_inflight_state(&row).unwrap();
    Cold {
        provider,
        shared,
        channel,
        cancel,
        owner,
        row,
        shown: Arc::new(Shown::default()),
    }
}

/// Runs the real bridge on `board` and sends `frames` once its entry is saved, as a launch that
/// takes its time does, then waits for its completion signal.
async fn bridge_on(turn: &Cold, board: &Arc<Board>, frames: Vec<StreamMessage>) {
    let mut bridge = seed_context("", turn.row.clone());
    bridge.provider = turn.provider.clone();
    bridge.gateway = board.clone();
    bridge.user_text_owned = "cold".into();
    bridge.user_msg_id = Some(MessageId::new(turn.row.user_msg_id));
    bridge.current_msg_id = Some(MessageId::new(PLACEHOLDER));
    let (completion_tx, completion_rx) = tokio::sync::oneshot::channel();
    bridge.completion_tx = Some(completion_tx);
    let (tx, rx) = mpsc::channel();
    let entry = turn.durable().expect("the turn has its admitted row");
    spawn_turn_bridge(turn.shared.clone(), turn.cancel.clone(), rx, bridge);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let entered = |row: InflightTurnState| {
        row.user_msg_id == turn.row.user_msg_id
            && row.turn_nonce == turn.row.turn_nonce
            && row.save_generation > entry.save_generation
    };
    while !turn.durable().is_some_and(entered) {
        assert!(
            std::time::Instant::now() < deadline,
            "the bridge never saved its entry"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    for frame in frames {
        tx.send(frame).unwrap();
    }
    drop(tx);
    tokio::time::timeout(std::time::Duration::from_secs(20), completion_rx)
        .await
        .expect("the bridge reports")
        .unwrap();
}

/// Reactions the bridge added to the user's message.
fn added(turn: &Cold) -> Vec<char> {
    let ops = turn.shared.turn_view_reconciler.ops();
    let mine = ops.into_iter().filter(|op| op.add);
    mine.filter(|op| op.target.message_id.get() == turn.row.user_msg_id)
        .map(|op| op.emoji)
        .collect()
}

/// A launch error keeps one detailed notice and warning reaction; only Legacy gets an error summary.
#[tokio::test(flavor = "current_thread")]
async fn cold_launch_error_without_stop_shows_failure_not_completion() {
    let root = tempfile::tempdir().unwrap();
    let _env = crate::config::set_agentdesk_root_for_test(root.path());
    let cases = [(true, true), (true, false), (false, true), (false, false)];
    for (n, (o, separate)) in cases.into_iter().enumerate() {
        let case = format!("o={o} separate={separate}");
        let channel = 5_340_664_000 + n as u64;
        let selected = [(channel, ClaudeTui)];
        let _o = test_override::force_channels(if o { &selected[..] } else { &[] });
        let _panel = SeparatePanel::set(separate);
        let turn = cold_claude(channel).await;
        let (result, launches) = turn.execute(turn.owner.clone()).await;
        assert_eq!(launches, 1, "{case}: {result:?}");
        let error = result.unwrap_err();
        assert_eq!(error, NO_LAUNCH_HOST, "{case}");
        let frame = crate::services::discord::router::message_handler::producer_error_frame(
            channel,
            error.clone(),
        );
        let board = Arc::new(Board::default());
        bridge_on(&turn, &board, frame.into_iter().collect()).await;
        let finals = board.finals();
        let shown: Vec<_> = finals.iter().filter(|(_, t)| t.contains(SUMMARY)).collect();
        let [(answer, notice)] = shown[..] else {
            panic!("{case}: one notice is shown: {finals:?}");
        };
        assert!(
            notice.contains(&format!("Error: {error}")),
            "{case}: {notice}"
        );
        assert!(
            !board.deleted().contains(answer),
            "{case}: {:?}",
            board.deleted()
        );
        let completed = finals.values().any(|text| text.contains("✅ 완료"));
        assert!(!completed, "{case}: {finals:?}");
        assert_eq!(notice.matches(&error).count(), 1, "{case}: detail once");
        assert!(
            !crate::services::discord::placeholder_sweeper::is_message_still_placeholder(notice),
            "{case}: the finalizer cannot rewrite the failure notice"
        );
        assert!(
            !finals.values().any(|text| text.starts_with("⚠️ 실패")),
            "{case}: no new failure panel: {finals:?}"
        );
        let summaries = finals
            .values()
            .filter(|text| text.starts_with("-# ⚠️ 오류"))
            .count();
        assert_eq!(summaries, usize::from(!o), "{case}: {finals:?}");
        assert_eq!(finals.len(), 1 + usize::from(!o), "{case}: {finals:?}");
        assert_eq!(added(&turn), ['⚠'], "{case}");
    }
}

/// The production symptom's text, a launch whose evidence never arrived, takes the same path.
#[tokio::test(flavor = "current_thread")]
async fn unproven_launch_evidence_error_is_shown_on_an_o_channel() {
    let root = tempfile::tempdir().unwrap();
    let _env = crate::config::set_agentdesk_root_for_test(root.path());
    let channel = 5_340_665_000;
    let _o = test_override::force_channels(&[(channel, ClaudeTui)]);
    let _panel = SeparatePanel::set(true);
    let turn = cold_claude(channel).await;
    let error = "herdr launch did not complete: launch evidence pane=w2:p1 EnvUnreadable";
    let frame = crate::services::discord::router::message_handler::producer_error_frame(
        channel,
        error.to_string(),
    );
    let board = Arc::new(Board::default());
    bridge_on(&turn, &board, frame.into_iter().collect()).await;
    let finals = board.finals();
    let notices: Vec<_> = finals
        .iter()
        .filter(|(_, text)| text.contains(SUMMARY))
        .collect();
    let [(answer, notice)] = notices[..] else {
        panic!("one detailed failure notice: {finals:?}");
    };
    assert_eq!(notice.matches(error).count(), 1, "{finals:?}");
    assert!(!board.deleted().contains(answer), "{:?}", board.deleted());
    assert_eq!(
        finals.len(),
        1,
        "no new panel or Legacy summary on O: {finals:?}"
    );
    assert!(
        !finals.values().any(|t| t.contains("✅ 완료")),
        "{finals:?}"
    );
    assert_eq!(added(&turn), ['⚠']);
}

/// An answered turn keeps its body ownership and gets only a separate Legacy completion summary.
#[tokio::test(flavor = "current_thread")]
async fn answered_turn_keeps_its_completed_panel() {
    let root = tempfile::tempdir().unwrap();
    let _env = crate::config::set_agentdesk_root_for_test(root.path());
    const BODY: &str = "the provider's own answer";
    for (n, o) in [true, false].into_iter().enumerate() {
        let channel = 5_340_666_000 + n as u64;
        let selected = [(channel, ClaudeTui)];
        let _o = test_override::force_channels(if o { &selected[..] } else { &[] });
        let _panel = SeparatePanel::set(true);
        let turn = cold_claude(channel).await;
        let frames = vec![
            StreamMessage::Text {
                content: BODY.into(),
            },
            StreamMessage::Done {
                result: String::new(),
                session_id: None,
            },
        ];
        let board = Arc::new(Board::default());
        bridge_on(&turn, &board, frames).await;
        let finals = board.finals();
        assert!(
            !finals.values().any(|text| text.contains("✅ 완료")),
            "o={o}: no new completion panel: {finals:?}"
        );
        let posted = finals.values().filter(|text| text.as_str() == BODY).count();
        assert_eq!(posted, usize::from(!o), "o={o}: {finals:?}");
        let summaries = finals
            .values()
            .filter(|text| text.starts_with("-# ⏱️ "))
            .count();
        assert_eq!(summaries, usize::from(!o), "o={o}: {finals:?}");
        assert_eq!(finals.len(), 2 * usize::from(!o), "o={o}: {finals:?}");
        assert!(!added(&turn).contains(&'⚠'), "o={o}");
    }
}

/// A failed turn whose panel a successor turn now owns leaves that panel alone, and its own panel
/// purges the queued Completed reconcile once it shows the failure.
#[tokio::test(flavor = "current_thread")]
async fn failure_panel_respects_successor_and_purges_completed_reconcile() {
    let root = tempfile::tempdir().unwrap();
    let _env = crate::config::set_agentdesk_root_for_test(root.path());
    let channel = 5_340_667_000;
    let turn = cold_claude(channel).await;
    let complete = |board: Arc<Board>, turn: &Cold| {
        let (shared, provider) = (turn.shared.clone(), turn.provider.clone());
        let (channel, user) = (turn.channel, MessageId::new(turn.row.user_msg_id));
        async move {
            let mut last = String::from("working");
            crate::services::discord::turn_bridge::single_message_footer::complete_bridge_terminal_footer_or_status_panel(
                &shared, board.as_ref(), channel, MessageId::new(POSTED), Some(user),
                Some(MessageId::new(PLACEHOLDER)), &provider, 1_700_000_000, &mut last,
                false, false, None, "⠸", 0, None, true, true,
            )
            .await
        }
    };
    let mut successor = turn.row.clone();
    successor.user_msg_id += 1;
    successor.status_message_id = Some(PLACEHOLDER);
    discord::inflight::save_inflight_state(&successor).unwrap();
    let board = Arc::new(Board::default());
    assert!(complete(board.clone(), &turn).await);
    assert!(board.finals().is_empty(), "{:?}", board.finals());

    let mut own = turn.row.clone();
    own.status_message_id = Some(PLACEHOLDER);
    discord::inflight::save_inflight_state(&own).unwrap();
    let record = abandon::AbandonRecord {
        msg_id: PLACEHOLDER,
        started_at: own.started_at.clone(),
        current_tool_line: None,
        terminal_status: abandon::TerminalCardStatus::Completed,
        episode: abandon::AbandonEpisodeIdentity {
            user_msg_id: own.user_msg_id,
            started_at: own.started_at.clone(),
            status_panel_generation: 0,
            save_generation: own.save_generation,
        },
    };
    abandon::enqueue(&turn.provider, &turn.shared.token_hash, channel, record).unwrap();
    assert!(complete(board.clone(), &turn).await);
    let panel = board.content(PLACEHOLDER).unwrap_or_default();
    assert!(panel.starts_with("⚠️ 실패"), "{panel}");
    let pending = abandon::load_pending(&turn.provider, &turn.shared.token_hash);
    assert!(pending.is_empty(), "{pending:?}");
}
