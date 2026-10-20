use super::tests::{Isolation, row};
use super::*;
use crate::services::discord::gateway::TurnGateway;
use crate::services::discord::recovery_engine::o_cut_recorder::{self, DiscordRecorder};
use crate::services::discord::serenity::{MessageId, UserId};
use crate::services::provider::CancelToken;
use std::sync::atomic::Ordering;

struct Fixture {
    shared: Arc<SharedData>,
    recorder: DiscordRecorder,
    channel: ChannelId,
    summary: LiveSummary,
    original: Arc<CancelToken>,
    actor: Option<std::sync::Weak<CancelToken>>,
}

impl Fixture {
    async fn new(channel: u64, active: bool) -> Self {
        let mut shared = crate::services::discord::make_shared_data_for_tests();
        Arc::get_mut(&mut shared)
            .unwrap()
            .ui
            .status_panel_v2_enabled = true;
        let channel_id = ChannelId::new(channel);
        let state = row(channel, 11, "watcher-await-original");
        let summary = LiveSummary::watcher(
            &shared,
            Some(
                TurnKey::new(channel_id, 11, shared.restart.current_generation)
                    .with_episode_nonce(state.turn_nonce.as_deref()),
            ),
            Some(&state),
            false,
            Outcome::Completed,
        )
        .unwrap();
        let original = Arc::new(CancelToken::from_persisted_turn_nonce(
            state.turn_nonce.clone(),
        ));
        if active {
            assert!(
                crate::services::discord::mailbox_try_start_turn(
                    &shared,
                    channel_id,
                    original.clone(),
                    UserId::new(1),
                    MessageId::new(11),
                )
                .await
            );
            crate::services::discord::increment_global_active(&shared, "summary-original");
            shared
                .dispatch
                .role_overrides
                .insert(channel_id, ChannelId::new(701));
        }
        let (_, actor) =
            crate::services::discord::tmux::tmux_watcher::completion_producer::capture_actors(
                &shared,
                channel_id,
                Some(&state),
            )
            .await;
        let recorder = o_cut_recorder::start(channel).await;
        let gateway = DiscordGateway::new(
            recorder.http.clone(),
            shared.clone(),
            ProviderKind::Claude,
            None,
        );
        gateway.send_message(channel_id, "answer").await.unwrap();
        Self {
            shared,
            recorder,
            channel: channel_id,
            summary,
            original,
            actor,
        }
    }

    async fn recovered(channel: u64, nonce: Option<&str>) -> Self {
        let mut fixture = Self::new(channel, false).await;
        let mut state = row(fixture.channel.get(), 0, "unused");
        state.turn_nonce = nonce.map(str::to_owned);
        fixture.original = Arc::new(CancelToken::from_persisted_turn_nonce(
            state.turn_nonce.clone(),
        ));
        let key = TurnKey::new(
            fixture.channel,
            state.effective_finalizer_turn_id(),
            fixture.shared.restart.current_generation,
        )
        .with_episode_nonce(state.turn_nonce.as_deref());
        fixture.summary = LiveSummary::watcher(
            &fixture.shared,
            Some(key),
            Some(&state),
            false,
            Outcome::Completed,
        )
        .unwrap();
        assert_ne!(fixture.summary.key.user_msg_id, 0);
        assert!(
            fixture
                .shared
                .mailbox(fixture.channel)
                .recovery_kickoff(fixture.original.clone(), UserId::new(1), None,)
                .await
                .activated_turn()
        );
        let (completion_actor, summary_actor) =
            crate::services::discord::tmux::tmux_watcher::completion_producer::capture_actors(
                &fixture.shared,
                fixture.channel,
                Some(&state),
            )
            .await;
        assert!(
            completion_actor.is_none(),
            "summary witness must not widen user-less cleanup authority"
        );
        assert!(summary_actor.is_some());
        fixture.actor = summary_actor;
        crate::services::discord::increment_global_active(
            &fixture.shared,
            "summary-recovery-actor",
        );
        fixture
            .shared
            .dispatch
            .role_overrides
            .insert(fixture.channel, ChannelId::new(701));
        fixture
    }

    async fn complete<S, F>(&self, panel: Option<MessageId>, sniffer: S)
    where
        S: FnOnce(Option<String>) -> F,
        F: std::future::Future<Output = bool>,
    {
        let mut spin = 0;
        let mut last_panel = String::new();
        crate::services::discord::tmux::tmux_watcher::completion_producer::complete_watcher_terminal_footer_or_status_panel_with_sniffer(
            &self.recorder.http, &self.shared, self.channel, &ProviderKind::Claude,
            0, false, &mut spin, None, None, "answer", panel, &mut last_panel, None,
            None, sniffer, Some(11), false, false, false, Some(self.summary), self.actor.as_ref(),
        ).await;
    }

    async fn release_original_and_start_successor(&self, same_identity: bool) -> Arc<CancelToken> {
        assert!(crate::services::discord::tmux::tmux_watcher::completion_producer::release_restored_watcher_active_turn_before_panel_edit(
            &self.shared, &ProviderKind::Claude, Some(self.summary.key),
        ).await);
        assert!(self.original.cancelled.load(Ordering::Relaxed));
        let nonce = if same_identity {
            "watcher-await-original"
        } else {
            "watcher-await-successor"
        };
        let next = Arc::new(CancelToken::from_persisted_turn_nonce(Some(
            nonce.to_string(),
        )));
        let message = if same_identity { 11 } else { 12 };
        assert!(
            crate::services::discord::mailbox_try_start_turn(
                &self.shared,
                self.channel,
                next.clone(),
                UserId::new(2),
                MessageId::new(message),
            )
            .await
        );
        crate::services::discord::increment_global_active(&self.shared, "summary-successor");
        self.shared
            .dispatch
            .role_overrides
            .insert(self.channel, ChannelId::new(702));
        next
    }

    async fn assert_successor_untouched(&self, successor: &Arc<CancelToken>) {
        let snapshot = self
            .shared
            .mailbox_peek(self.channel)
            .unwrap()
            .snapshot()
            .await;
        assert!(
            snapshot
                .cancel_token
                .as_ref()
                .is_some_and(|token| Arc::ptr_eq(token, successor))
        );
        assert!(!successor.cancelled.load(Ordering::Relaxed));
        assert_eq!(self.shared.restart.global_active.load(Ordering::Relaxed), 1);
        assert_eq!(
            self.shared
                .dispatch
                .role_overrides
                .get(&self.channel)
                .map(|entry| *entry.value()),
            Some(ChannelId::new(702))
        );
    }
}

#[tokio::test(flavor = "current_thread")]
async fn actual_watcher_producer_rechecks_the_actor_after_sniffer_await_before_summary_or_latch() {
    let _isolation = Isolation::new();
    let _legacy = crate::services::tui_o::cutover::test_override::force_channels(&[]);
    for (index, same_identity) in [false, true].into_iter().enumerate() {
        let fixture = Fixture::new(586_501 + index as u64, true).await;
        let entered = tokio::sync::Notify::new();
        let resume = tokio::sync::Notify::new();
        let producer = fixture.complete(None, |_| async {
            entered.notify_one();
            resume.notified().await;
            false
        });
        let takeover = async {
            entered.notified().await;
            let successor = fixture
                .release_original_and_start_successor(same_identity)
                .await;
            resume.notify_one();
            successor
        };
        let (_, successor) = tokio::time::timeout(Duration::from_secs(10), async {
            tokio::join!(producer, takeover)
        })
        .await
        .unwrap();
        assert_eq!(
            fixture.recorder.contents(),
            ["answer"],
            "actual watcher summary POST must be vetoed after a successor claims the awaited episode"
        );
        assert_eq!(
            fixture.recorder.calls().len(),
            1,
            "no hidden edit or summary transport"
        );
        fixture.assert_successor_untouched(&successor).await;
        crate::services::discord::mailbox_finish_turn(
            &fixture.shared,
            &ProviderKind::Claude,
            fixture.channel,
        )
        .await;
        fixture.complete(None, |_| async { false }).await;
        fixture.complete(None, |_| async { false }).await;
        assert_eq!(
            fixture.recorder.contents(),
            ["answer", "-# ✅ 완료"],
            "an authority veto must leave the old summary latch available for one permitted attempt"
        );
    }
}

#[tokio::test(flavor = "current_thread")]
async fn actual_watcher_upgrade_panel_still_edits_once_but_summary_obeys_the_post_await_owner() {
    let _isolation = Isolation::new();
    let _legacy = crate::services::tui_o::cutover::test_override::force_channels(&[]);
    let fixture = Fixture::new(586_503, true).await;
    let successor = tokio::sync::OnceCell::new();
    fixture
        .complete(Some(MessageId::new(101)), |_| async {
            let next = fixture.release_original_and_start_successor(false).await;
            assert!(successor.set(next).is_ok());
            false
        })
        .await;
    let calls = fixture.recorder.calls();
    assert_eq!(
        calls.len(),
        2,
        "old upgrade panel EDIT is allowed; a new summary POST is not"
    );
    assert_eq!(calls[0].content.as_deref(), Some("answer"));
    assert!(calls[0].route.starts_with("POST "));
    assert!(calls[1].route.starts_with("PATCH "));
    assert!(calls[1].route.ends_with("/messages/101"));
    assert_eq!(
        calls[1].content.as_deref().unwrap().lines().next(),
        Some("-# ✅ 완료")
    );
    fixture
        .assert_successor_untouched(successor.get().unwrap())
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn actual_watcher_summary_allows_a_row_owned_episode_without_creating_a_mailbox() {
    let _isolation = Isolation::new();
    let _legacy = crate::services::tui_o::cutover::test_override::force_channels(&[]);
    let fixture = Fixture::new(586_504, false).await;
    assert!(fixture.shared.mailbox_peek(fixture.channel).is_none());
    fixture.complete(None, |_| async { false }).await;
    fixture.complete(None, |_| async { false }).await;
    assert_eq!(
        fixture.recorder.contents(),
        ["answer", "-# ✅ 완료"],
        "a completed row-owned watcher needs no mailbox for one nonbody summary"
    );
    assert!(
        fixture.shared.mailbox_peek(fixture.channel).is_none(),
        "summary chrome must not create mailbox authority"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn actual_watcher_summary_keeps_the_captured_active_actor_without_releasing_its_turn() {
    let _isolation = Isolation::new();
    let _legacy = crate::services::tui_o::cutover::test_override::force_channels(&[]);
    let fixture = Fixture::new(586_506, true).await;
    fixture.complete(None, |_| async { false }).await;
    fixture.complete(None, |_| async { false }).await;
    assert_eq!(fixture.recorder.contents(), ["answer", "-# ✅ 완료"]);
    let snapshot = fixture
        .shared
        .mailbox_peek(fixture.channel)
        .unwrap()
        .snapshot()
        .await;
    assert!(
        snapshot
            .cancel_token
            .as_ref()
            .is_some_and(|token| Arc::ptr_eq(token, &fixture.original))
    );
    assert!(!fixture.original.cancelled.load(Ordering::Relaxed));
    assert_eq!(
        fixture.shared.restart.global_active.load(Ordering::Relaxed),
        1
    );
    assert_eq!(
        fixture
            .shared
            .dispatch
            .role_overrides
            .get(&fixture.channel)
            .map(|entry| *entry.value()),
        Some(ChannelId::new(701))
    );
}

#[tokio::test(flavor = "current_thread")]
async fn actual_watcher_existing_panel_edits_then_posts_one_separate_summary_without_body_mutation()
{
    let _isolation = Isolation::new();
    let _legacy = crate::services::tui_o::cutover::test_override::force_channels(&[]);
    let fixture = Fixture::new(586_507, true).await;
    let original_body = fixture.recorder.contents();
    assert_eq!(original_body, ["answer"]);
    fixture
        .complete(Some(MessageId::new(101)), |_| async { false })
        .await;
    let calls = fixture.recorder.calls();
    let completed_panel = calls
        .get(1)
        .and_then(|call| call.content.clone())
        .expect("old-panel completion must produce a PATCH payload");
    assert_eq!(completed_panel.lines().next(), Some("-# ✅ 완료"));
    assert_eq!(
        calls.len(),
        3,
        "one old-panel PATCH and one separate summary POST must follow the original body"
    );
    let expected_contents = [
        Some(original_body[0].clone()),
        Some(completed_panel),
        Some("-# ✅ 완료".into()),
    ];
    assert_eq!(
        calls
            .iter()
            .map(|call| call.content.clone())
            .collect::<Vec<_>>(),
        expected_contents,
        "the complete raw history must preserve body bytes and allow only old-panel completion then summary"
    );
    let messages = format!("/channels/{}/messages", fixture.channel.get());
    assert!(calls[0].route.starts_with("POST ") && calls[0].route.ends_with(&messages));
    assert!(
        calls[1].route.starts_with("PATCH ")
            && calls[1].route.ends_with(&format!("{messages}/101"))
    );
    assert!(calls[2].route.starts_with("POST ") && calls[2].route.ends_with(&messages));
    assert_eq!(
        fixture.recorder.contents()[0].as_bytes(),
        original_body[0].as_bytes()
    );
    let snapshot = fixture
        .shared
        .mailbox_peek(fixture.channel)
        .unwrap()
        .snapshot()
        .await;
    assert!(
        snapshot
            .cancel_token
            .as_ref()
            .is_some_and(|token| Arc::ptr_eq(token, &fixture.original))
    );
    assert!(!fixture.original.cancelled.load(Ordering::Relaxed));
    assert_eq!(
        fixture.shared.restart.global_active.load(Ordering::Relaxed),
        1
    );
}

#[tokio::test(flavor = "current_thread")]
async fn actual_watcher_summary_allows_the_captured_synthetic_recovery_actor_without_a_user_id() {
    let _isolation = Isolation::new();
    let _legacy = crate::services::tui_o::cutover::test_override::force_channels(&[]);
    let fixture = Fixture::recovered(586_508, Some("watcher-await-original")).await;
    fixture.complete(None, |_| async { false }).await;
    fixture.complete(None, |_| async { false }).await;
    assert_eq!(
        fixture.recorder.contents(),
        ["answer", "-# ✅ 완료"],
        "a synthetic finalizer key must not require a Discord user message id"
    );
    let snapshot = fixture
        .shared
        .mailbox_peek(fixture.channel)
        .unwrap()
        .snapshot()
        .await;
    assert!(snapshot.active_user_message_id.is_none());
    assert!(
        snapshot
            .cancel_token
            .as_ref()
            .is_some_and(|actor| Arc::ptr_eq(actor, &fixture.original))
    );
    assert!(!fixture.original.cancelled.load(Ordering::Relaxed));
    assert_eq!(
        fixture.shared.restart.global_active.load(Ordering::Relaxed),
        1
    );
    assert_eq!(
        fixture
            .shared
            .dispatch
            .role_overrides
            .get(&fixture.channel)
            .map(|entry| *entry.value()),
        Some(ChannelId::new(701))
    );
}

#[tokio::test(flavor = "current_thread")]
async fn actual_watcher_summary_vetoes_a_new_actor_admitted_after_a_row_owned_absent_mailbox() {
    let _isolation = Isolation::new();
    let _legacy = crate::services::tui_o::cutover::test_override::force_channels(&[]);
    let fixture = Fixture::new(586_509, false).await;
    assert!(fixture.shared.mailbox_peek(fixture.channel).is_none());
    let successor = Arc::new(CancelToken::from_persisted_turn_nonce(Some(
        "row-owned-successor".into(),
    )));
    fixture
        .complete(None, |_| async {
            assert!(
                crate::services::discord::mailbox_try_start_turn(
                    &fixture.shared,
                    fixture.channel,
                    successor.clone(),
                    UserId::new(2),
                    MessageId::new(12),
                )
                .await
            );
            crate::services::discord::increment_global_active(
                &fixture.shared,
                "summary-row-owned-successor",
            );
            fixture
                .shared
                .dispatch
                .role_overrides
                .insert(fixture.channel, ChannelId::new(702));
            false
        })
        .await;
    assert_eq!(
        fixture.recorder.contents(),
        ["answer"],
        "an absent capture is no authority over an actor admitted during the real watcher await"
    );
    fixture.assert_successor_untouched(&successor).await;
}

#[tokio::test(flavor = "current_thread")]
async fn actual_watcher_summary_vetoes_same_nonce_replacement_before_the_completion_call() {
    let _isolation = Isolation::new();
    let _legacy = crate::services::tui_o::cutover::test_override::force_channels(&[]);
    let fixture = Fixture::new(586_510, true).await;
    let successor = fixture.release_original_and_start_successor(true).await;
    fixture.complete(None, |_| async { false }).await;
    assert_eq!(
        fixture.recorder.contents(),
        ["answer"],
        "original actor witness must veto a same-nonce replacement already present before producer capture"
    );
    fixture.assert_successor_untouched(&successor).await;
}

#[tokio::test(flavor = "current_thread")]
async fn actual_watcher_summary_does_not_recertify_a_live_actor_when_the_original_witness_is_missing()
 {
    let _isolation = Isolation::new();
    let _legacy = crate::services::tui_o::cutover::test_override::force_channels(&[]);
    let mut fixture = Fixture::new(586_511, true).await;
    fixture.actor = None;
    fixture.complete(None, |_| async { false }).await;
    assert_eq!(
        fixture.recorder.contents(),
        ["answer"],
        "a late current snapshot must not mint original-actor authority"
    );
    let snapshot = fixture
        .shared
        .mailbox_peek(fixture.channel)
        .unwrap()
        .snapshot()
        .await;
    assert!(
        snapshot
            .cancel_token
            .as_ref()
            .is_some_and(|actor| Arc::ptr_eq(actor, &fixture.original))
    );
    assert!(!fixture.original.cancelled.load(Ordering::Relaxed));
    assert_eq!(
        fixture.shared.restart.global_active.load(Ordering::Relaxed),
        1
    );
}

#[tokio::test(flavor = "current_thread")]
async fn actual_watcher_live_legacy_none_or_empty_nonce_keeps_the_old_panel_and_rejects_a_replaced_actor()
 {
    let _isolation = Isolation::new();
    let _legacy = crate::services::tui_o::cutover::test_override::force_channels(&[]);
    for (index, (nonce, replaced)) in [
        (None, false),
        (None, true),
        (Some(""), false),
        (Some(""), true),
    ]
    .into_iter()
    .enumerate()
    {
        let fixture = Fixture::recovered(586_550 + index as u64, nonce).await;
        let successor = if replaced {
            let released = crate::services::discord::mailbox_finish_turn(
                &fixture.shared,
                &ProviderKind::Claude,
                fixture.channel,
            )
            .await;
            assert!(
                released
                    .removed_token
                    .as_ref()
                    .is_some_and(|actor| Arc::ptr_eq(actor, &fixture.original))
            );
            crate::services::discord::saturating_decrement_global_active(&fixture.shared);
            let successor = Arc::new(CancelToken::from_persisted_turn_nonce(None));
            assert!(
                fixture
                    .shared
                    .mailbox(fixture.channel)
                    .recovery_kickoff(successor.clone(), UserId::new(1), None)
                    .await
                    .activated_turn()
            );
            crate::services::discord::increment_global_active(
                &fixture.shared,
                "summary-legacy-none-successor",
            );
            fixture
                .shared
                .dispatch
                .role_overrides
                .insert(fixture.channel, ChannelId::new(702));
            Some(successor)
        } else {
            None
        };
        fixture
            .complete(Some(MessageId::new(101)), |_| async { false })
            .await;
        let calls = fixture.recorder.calls();
        let panel = calls
            .get(1)
            .and_then(|call| call.content.clone())
            .expect("old-panel completion must produce a PATCH payload");
        assert_eq!(panel.lines().next(), Some("-# ✅ 완료"));
        let expected = if replaced {
            vec!["answer".to_string(), panel]
        } else {
            vec!["answer".to_string(), panel, "-# ✅ 완료".to_string()]
        };
        assert_eq!(
            fixture.recorder.contents(),
            expected,
            "live legacy None/empty nonce must summarize only its original actor allocation"
        );
        assert!(calls[0].route.starts_with("POST "));
        assert!(calls[1].route.starts_with("PATCH ") && calls[1].route.ends_with("/messages/101"));
        if let Some(successor) = successor {
            fixture.assert_successor_untouched(&successor).await;
        } else {
            assert_eq!(calls.len(), 3);
            assert_eq!(calls[2].content.as_deref(), Some("-# ✅ 완료"));
            assert!(calls[2].route.starts_with("POST "));
            let snapshot = fixture
                .shared
                .mailbox_peek(fixture.channel)
                .unwrap()
                .snapshot()
                .await;
            assert!(
                snapshot
                    .cancel_token
                    .as_ref()
                    .is_some_and(|actor| Arc::ptr_eq(actor, &fixture.original))
            );
        }
    }
}

#[tokio::test(flavor = "current_thread")]
async fn actual_watcher_summary_survives_the_existing_verified_idle_mailbox_purge() {
    let _isolation = Isolation::new();
    let _legacy = crate::services::tui_o::cutover::test_override::force_channels(&[]);
    let fixture = Fixture::new(586_514, true).await;
    fixture.complete(None, |_| async {
        assert!(crate::services::discord::tmux::tmux_watcher::completion_producer::release_restored_watcher_active_turn_before_panel_edit(
            &fixture.shared, &ProviderKind::Claude, Some(fixture.summary.key),
        ).await);
        let _ = fixture.shared.mailboxes.remove_idle_entry(fixture.channel).await;
        assert!(fixture.shared.mailbox_peek(fixture.channel).is_none(),
            "only the existing verified-idle purge may remove the fixture mailbox");
        false
    }).await;
    assert_eq!(
        fixture.recorder.contents(),
        ["answer", "-# ✅ 완료"],
        "a verified-idle purge must not revoke one nonbody completion summary"
    );
    assert!(fixture.shared.mailbox_peek(fixture.channel).is_none());
    assert_eq!(
        fixture.shared.restart.global_active.load(Ordering::Relaxed),
        0
    );
}
