use super::*;
use crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui;
use crate::services::discord::formatting::ReplaceLongMessageOutcome;
use crate::services::discord::gateway::GatewayFuture;
use crate::services::tui_o::cutover::test_override;
use crate::services::tui_o::writer::deliver;
use std::sync::Mutex;

const CHANNEL: u64 = 6_325_400;
const USER_MSG: u64 = 6_325_401;
const ANSWER: u64 = 6_325_402;
const PANEL: u64 = 6_325_410;
const LATE_BODY: u64 = 6_325_420;
const SEND_BASE: u64 = 6_325_430;

#[derive(Default)]
struct Calls {
    /// One id sequence for panel sends and O's posts, as Discord's snowflakes are.
    next: u64,
    sent: Vec<(u64, String)>,
    edited: Vec<u64>,
    deleted: Vec<u64>,
}

#[derive(Default)]
struct PanelGateway {
    calls: Arc<Mutex<Calls>>,
    fail_delete: bool,
}

impl TurnGateway for PanelGateway {
    fn send_message<'a>(
        &'a self,
        _channel_id: ChannelId,
        content: &'a str,
    ) -> GatewayFuture<'a, Result<MessageId, String>> {
        let mut calls = self.calls.lock().unwrap();
        let id = calls.next;
        calls.next += 1;
        calls.sent.push((id, content.to_string()));
        Box::pin(async move { Ok(MessageId::new(id)) })
    }

    fn edit_message<'a>(
        &'a self,
        _channel_id: ChannelId,
        message_id: MessageId,
        _content: &'a str,
    ) -> GatewayFuture<'a, Result<(), String>> {
        let mut calls = self.calls.lock().unwrap();
        calls.edited.push(message_id.get());
        let gone = !self.fail_delete && calls.deleted.contains(&message_id.get());
        Box::pin(async move {
            if gone {
                Err("Unknown Message (10008)".into())
            } else {
                Ok(())
            }
        })
    }

    fn delete_message<'a>(
        &'a self,
        _channel_id: ChannelId,
        message_id: MessageId,
    ) -> GatewayFuture<'a, Result<(), String>> {
        self.calls.lock().unwrap().deleted.push(message_id.get());
        let fail = self.fail_delete;
        Box::pin(async move {
            if fail {
                Err("delete failed".into())
            } else {
                Ok(())
            }
        })
    }

    fn replace_message_with_outcome<'a>(
        &'a self,
        _channel_id: ChannelId,
        _message_id: MessageId,
        _content: &'a str,
    ) -> GatewayFuture<'a, Result<ReplaceLongMessageOutcome, String>> {
        Box::pin(async { Ok(ReplaceLongMessageOutcome::EditedOriginal) })
    }

    fn schedule_retry_with_history<'a>(
        &'a self,
        _channel_id: ChannelId,
        _user_message_id: MessageId,
        _user_text: &'a str,
    ) -> GatewayFuture<'a, ()> {
        Box::pin(async {})
    }

    fn dispatch_queued_turn<'a>(
        &'a self,
        _channel_id: ChannelId,
        _intervention: &'a Intervention,
        _request_owner_name: &'a str,
        _has_more_queued_turns: bool,
        _dispatch_lease: Option<Arc<crate::services::turn_orchestrator::DispatchLease>>,
    ) -> GatewayFuture<'a, Result<(), String>> {
        Box::pin(async { Ok(()) })
    }

    fn validate_live_routing<'a>(
        &'a self,
        _channel_id: ChannelId,
    ) -> GatewayFuture<'a, Result<(), String>> {
        Box::pin(async { Ok(()) })
    }

    fn requester_mention(&self) -> Option<String> {
        None
    }

    fn can_chain_locally(&self) -> bool {
        true
    }

    fn bot_owner_provider(&self) -> Option<ProviderKind> {
        Some(ProviderKind::Claude)
    }
}

/// A two-message turn whose panel sits above where O will post the body.
struct Turn {
    shared: Arc<SharedData>,
    gateway: Arc<dyn TurnGateway>,
    calls: Arc<Mutex<Calls>>,
    row: InflightTurnState,
    _root: crate::config::TestRuntimeRootGuard,
    _posted: std::sync::MutexGuard<'static, ()>,
}

impl Turn {
    fn open(fail_delete: bool) -> Self {
        let root = crate::config::TestRuntimeRootGuard::new();
        let posted = deliver::forget_posted_for_tests(CHANNEL);
        let mut shared = crate::services::discord::make_shared_data_for_tests();
        let ui = &mut Arc::get_mut(&mut shared).expect("fresh shared").ui;
        (ui.status_panel_v2_enabled, ui.two_message_panel_enabled) = (true, true);
        let calls = Arc::new(Mutex::new(Calls {
            next: SEND_BASE,
            ..Calls::default()
        }));
        let gateway: Arc<dyn TurnGateway> = Arc::new(PanelGateway {
            calls: Arc::clone(&calls),
            fail_delete,
        });
        let mut row = InflightTurnState::new(
            ProviderKind::Claude,
            CHANNEL,
            None,
            1,
            USER_MSG,
            ANSWER,
            "prompt".to_string(),
            None,
            None,
            None,
            None,
            0,
        );
        row.runtime_kind = Some(ClaudeTui);
        row.status_message_id = Some(PANEL);
        inflight::save_inflight_state(&row).expect("seed the turn row");
        singleton::bind_if_owned(
            &ProviderKind::Claude,
            &shared.token_hash,
            CHANNEL,
            PANEL,
            Some(1),
        )
        .expect("bind the turn's panel");
        Self {
            shared,
            gateway,
            calls,
            row,
            _root: root,
            _posted: posted,
        }
    }

    /// The bridge's completion edit, then the follow the postlude starts after it.
    async fn complete(&self) -> Option<tokio::task::JoinHandle<()>> {
        let channel_id = ChannelId::new(CHANNEL);
        let mut text = String::new();
        let committed = complete_bridge_terminal_footer_or_status_panel(
            self.shared.as_ref(),
            self.gateway.as_ref(),
            channel_id,
            MessageId::new(ANSWER),
            Some(MessageId::new(USER_MSG)),
            Some(MessageId::new(PANEL)),
            &ProviderKind::Claude,
            0,
            &mut text,
            false,
            false,
            None,
            "⠋",
            1,
            None,
            true,
        )
        .await;
        assert!(
            committed && text.contains("완료"),
            "completed panel: {text:?}"
        );
        let owner = (
            &self.shared,
            &self.gateway,
            &ProviderKind::Claude,
            channel_id,
        );
        follow(owner, &self.row, &text)
    }

    /// The bridge closes the turn's row once its postlude is done.
    fn close_row(&self) {
        let root = crate::services::discord::runtime_store::discord_inflight_root().unwrap();
        let path = inflight::inflight_state_path(&root, &ProviderKind::Claude, CHANNEL);
        std::fs::remove_file(path).expect("close the turn row");
    }

    fn panel(&self) -> Option<u64> {
        singleton::load(&ProviderKind::Claude, &self.shared.token_hash, CHANNEL)
            .map(|binding| binding.panel_message_id)
    }

    /// O posts a body piece with the channel's next id.
    fn o_posts(&self) -> u64 {
        let mut calls = self.calls();
        let id = calls.next;
        calls.next += 1;
        deliver::note_posted_for_tests(CHANNEL, id);
        id
    }

    fn calls(&self) -> std::sync::MutexGuard<'_, Calls> {
        self.calls.lock().unwrap()
    }
}

/// A completed panel on O's channel ends below a body O posts after the completion edit, and
/// the old panel goes to delete; the body itself is never edited or deleted.
#[tokio::test]
async fn a_completed_o_panel_moves_below_a_body_o_posts_after_completion() {
    let turn = Turn::open(false);
    let _o = test_override::force_channels(&[(CHANNEL, ClaudeTui)]);
    let follow = turn
        .complete()
        .await
        .expect("O's channel follows its panel");
    deliver::note_posted_for_tests(CHANNEL, LATE_BODY);
    tokio::time::sleep(POLL * 3).await;
    assert!(
        turn.calls().sent.is_empty(),
        "no move while the turn's row is open"
    );
    turn.close_row();
    follow.await.unwrap();

    let calls = turn.calls();
    let [(moved, text)] = calls.sent.as_slice() else {
        panic!("one move: {:?}", calls.sent);
    };
    assert!(
        *moved > LATE_BODY && text.contains("완료"),
        "{moved} {text:?}"
    );
    assert_eq!(turn.panel(), Some(*moved));
    assert_eq!(calls.deleted, vec![PANEL]);
    assert_eq!(calls.edited, vec![PANEL]);
}

/// A Legacy channel's completed panel keeps main's place and edits whatever O posts.
#[tokio::test]
async fn a_legacy_completed_panel_is_never_moved() {
    let turn = Turn::open(false);
    let _o = test_override::force_channels(&[]);
    let follow = turn.complete().await;
    deliver::note_posted_for_tests(CHANNEL, LATE_BODY);
    turn.close_row();
    match follow {
        Some(follow) => follow.await.unwrap(),
        None => tokio::time::sleep(POLL * 4).await,
    }

    let calls = turn.calls();
    assert!(
        calls.sent.is_empty() && calls.deleted.is_empty(),
        "{:?}",
        calls.sent
    );
    assert_eq!(calls.edited, vec![PANEL]);
    assert_eq!(turn.panel(), Some(PANEL));
}

/// An old panel whose delete fails is handed to the orphan drain that retries it.
#[tokio::test]
async fn a_failed_old_panel_delete_goes_to_the_orphan_drain() {
    let turn = Turn::open(true);
    let _o = test_override::force_channels(&[(CHANNEL, ClaudeTui)]);
    let follow = turn
        .complete()
        .await
        .expect("O's channel follows its panel");
    turn.close_row();
    deliver::note_posted_for_tests(CHANNEL, LATE_BODY);
    follow.await.unwrap();

    let moved = turn.calls().sent.first().map(|(id, _)| *id);
    assert_eq!(turn.panel(), moved);
    let pending = orphans::load_pending(&ProviderKind::Claude, &turn.shared.token_hash);
    assert_eq!(pending, vec![(CHANNEL, PANEL)]);
}

/// A long body O keeps posting after completion, in more pieces than the move budget, still ends
/// with the completed panel below its last piece, within the per-turn move budget.
#[tokio::test(start_paused = true)]
async fn a_completed_o_panel_ends_below_the_last_of_many_late_o_posts() {
    let turn = Turn::open(false);
    let _o = test_override::force_channels(&[(CHANNEL, ClaudeTui)]);
    let follow = turn
        .complete()
        .await
        .expect("O's channel follows its panel");
    turn.close_row();
    let mut last_body = 0;
    for _ in 0..=MAX_MOVES {
        last_body = turn.o_posts();
        tokio::time::sleep(QUIET + POLL + POLL / 5).await;
    }
    follow.await.unwrap();

    let calls = turn.calls();
    let panel = turn.panel().expect("a singleton panel");
    assert!(
        panel > last_body,
        "panel {panel} above O's last body {last_body}"
    );
    assert!(calls.sent.len() <= MAX_MOVES, "{:?}", calls.sent);
    assert!(
        calls
            .deleted
            .iter()
            .all(|id| *id == PANEL || calls.sent.iter().any(|s| s.0 == *id))
    );
}

/// A completion that still names a panel already moved below O's posts sends no second panel.
#[tokio::test]
async fn a_late_completion_of_a_moved_panel_sends_no_second_panel() {
    let turn = Turn::open(false);
    let _o = test_override::force_channels(&[(CHANNEL, ClaudeTui)]);
    let follow = turn
        .complete()
        .await
        .expect("O's channel follows its panel");
    turn.close_row();
    turn.o_posts();
    follow.await.unwrap();
    let moved = turn.panel();
    assert_ne!(moved, Some(PANEL));
    let sent = turn.calls().sent.len();

    let mut text = "working".to_string();
    let committed = super::super::super::status_panel::complete_status_panel_v2(
        turn.shared.as_ref(),
        turn.gateway.as_ref(),
        ChannelId::new(CHANNEL),
        Some(MessageId::new(PANEL)),
        &ProviderKind::Claude,
        0,
        &mut text,
        false,
        false,
        "late_completion",
        USER_MSG,
        true,
    )
    .await;
    assert!(committed);
    assert_eq!(turn.calls().sent.len(), sent, "{:?}", turn.calls().sent);
    assert_eq!(turn.panel(), moved);
}
