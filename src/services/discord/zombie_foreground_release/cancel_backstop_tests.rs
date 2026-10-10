use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use poise::serenity_prelude::{ChannelId, MessageId, UserId};

use super::cancel_backstop_test_support as release_support;
use super::tests::fixtures::missing_tmux_fixture;
use super::{ZombieForegroundVerdict, release_zombie_foreground_turn};
use crate::services::discord::mailbox_finish::MailboxLookup;
use crate::services::discord::{self as discord, inflight};
use crate::services::provider::{CancelToken, ProviderKind};
use crate::services::turn_orchestrator::{
    Intervention, InterventionMode, SourceMessageQueuedGeneration, SourceMessageTextSegment,
    TokenFinish,
};

fn preserved_queue_item(message: u64) -> Intervention {
    let generation = discord::runtime_store::process_generation();
    let ids = [MessageId::new(message), MessageId::new(message + 1)];
    Intervention {
        author_id: UserId::new(21),
        author_is_bot: false,
        message_id: ids[0],
        queued_generation: generation,
        source_message_ids: ids.to_vec(),
        source_message_queued_generations: ids
            .into_iter()
            .map(|id| SourceMessageQueuedGeneration::user_instruction(id, generation))
            .collect(),
        source_text_segments: ids
            .into_iter()
            .enumerate()
            .map(|(index, id)| SourceMessageTextSegment::new(id, format!("source {index}")))
            .collect(),
        text: "preserved first source\npreserved second source".to_string(),
        mode: InterventionMode::Soft,
        created_at: Instant::now(),
        reply_context: Some("original reply boundary".to_string()),
        has_reply_boundary: true,
        merge_consecutive: false,
        pending_uploads: Vec::new(),
        voice_announcement: None,
    }
}

async fn stale_cancel_probe_preserves_successor(cancelled_successor: bool) {
    let temp = tempfile::tempdir().expect("isolated runtime root");
    let _root = crate::config::TestEnvVarGuard::set_path("AGENTDESK_ROOT_DIR", temp.path());
    let _tmux = missing_tmux_fixture(&temp);
    let provider = ProviderKind::Claude;
    let channel = ChannelId::new(if cancelled_successor {
        6_016_142
    } else {
        6_016_141
    });
    let shared = discord::make_shared_data_for_tests();
    let actor = shared.mailbox(channel);
    let old_token = Arc::new(CancelToken::new());
    old_token.cancelled.store(true, Ordering::Relaxed);
    old_token.bind_unmanaged_session_name("AgentDesk-claude-cancel-backstop-old");
    assert!(
        actor
            .try_start_turn(old_token.clone(), UserId::new(7), MessageId::new(70))
            .await,
        "A must hold the actual mailbox actor"
    );
    discord::increment_global_active(&shared, "cancel backstop fixture A");
    actor
        .replace_queue(
            vec![preserved_queue_item(channel.get() + 10)],
            discord::queue_persistence_context(&shared, &provider, channel),
        )
        .await;

    let barrier = release_support::install_release_pause(channel);
    let mut release = tokio::spawn({
        let shared = shared.clone();
        let provider = provider.clone();
        async move {
            release_zombie_foreground_turn(&shared, &provider, channel, "idle_queue_backstop").await
        }
    });
    tokio::select! {
        _ = barrier.wait(Duration::from_secs(5)) => {}
        outcome = &mut release => {
            panic!("A must reach the evidence barrier before returning: {outcome:?}");
        }
    }

    let finish_a = discord::mailbox_finish_judged_turn(
        &shared,
        &provider,
        channel,
        Some(&old_token),
        MailboxLookup::Peek,
    )
    .await;
    assert!(
        matches!(finish_a, TokenFinish::Finished(ref result)
            if result.removed_token.as_ref().is_some_and(|token| Arc::ptr_eq(token, &old_token))),
        "the actor must actually finish A while its cancel probe is paused"
    );
    discord::saturating_decrement_global_active(&shared);

    let next_token = Arc::new(CancelToken::new());
    next_token
        .cancelled
        .store(cancelled_successor, Ordering::Relaxed);
    next_token.bind_unmanaged_session_name("AgentDesk-claude-cancel-backstop-next");
    assert!(
        actor
            .try_start_turn(next_token.clone(), UserId::new(8), MessageId::new(80))
            .await,
        "the actor must actually admit successor B"
    );
    discord::increment_global_active(&shared, "cancel backstop fixture B");

    let inflight_root = inflight::inflight_runtime_root().expect("isolated inflight root");
    assert!(inflight_root.starts_with(temp.path()));
    let row_path = inflight::inflight_state_path(&inflight_root, &provider, channel.get());
    let row_before = if cancelled_successor {
        let row = inflight::InflightTurnState::new(
            provider.clone(),
            channel.get(),
            Some("adk-cc".to_string()),
            8,
            80,
            81,
            "successor B owns this inflight row".to_string(),
            Some("successor-session".to_string()),
            Some("AgentDesk-claude-cancel-backstop-next".to_string()),
            Some(
                temp.path()
                    .join("successor.jsonl")
                    .to_string_lossy()
                    .into_owned(),
            ),
            None,
            0,
        );
        inflight::save_inflight_state(&row).expect("persist successor B inflight row");
        Some(std::fs::read(&row_path).expect("read successor B row bytes"))
    } else {
        assert!(
            !row_path.exists(),
            "uncancelled successor needs no inflight fixture"
        );
        None
    };
    let before = actor.snapshot().await;
    assert!(
        before
            .cancel_token
            .as_ref()
            .is_some_and(|token| Arc::ptr_eq(token, &next_token)),
        "B must own the actor before A resumes"
    );
    assert_eq!(before.intervention_queue.len(), 1);
    assert!(before.intervention_queue[0].preserve_on_cancel());
    let queue_before = format!("{:?}", before.intervention_queue);
    let active_before = shared.restart.global_active.load(Ordering::Relaxed);
    assert_eq!(active_before, 1, "only successor B holds a foreground slot");

    barrier.release();
    let outcome = tokio::time::timeout(Duration::from_secs(5), release)
        .await
        .expect("the old cancel probe must finish after the barrier opens")
        .expect("the old cancel probe must not panic");
    let after = actor.snapshot().await;
    assert!(
        after
            .cancel_token
            .as_ref()
            .is_some_and(|token| Arc::ptr_eq(token, &next_token)),
        "a stale A cancel probe must preserve successor B's exact token"
    );
    assert_eq!(
        next_token.cancelled.load(Ordering::Relaxed),
        cancelled_successor
    );
    assert_eq!(after.active_user_message_id, before.active_user_message_id);
    assert_eq!(after.active_request_owner, before.active_request_owner);
    assert_eq!(after.active_turn_nonce, before.active_turn_nonce);
    assert_eq!(after.active_turn_kind, before.active_turn_kind);
    assert_eq!(format!("{:?}", after.intervention_queue), queue_before);
    assert_eq!(std::fs::read(&row_path).ok(), row_before);
    assert_eq!(
        shared.restart.global_active.load(Ordering::Relaxed),
        active_before
    );
    assert_eq!(outcome.verdict, Some(ZombieForegroundVerdict::Release));
    assert!(!outcome.released);
    assert_eq!(outcome.queue_depth_after, 1);
    assert!(!outcome.queue_kickoff_scheduled);
    assert_eq!(
        release_support::finish_status(channel),
        Some("token_mismatch")
    );
}

#[tokio::test]
async fn stale_cancel_probe_preserves_uncancelled_successor() {
    stale_cancel_probe_preserves_successor(false).await;
}

#[tokio::test]
async fn stale_cancel_probe_preserves_cancelled_successor_with_inflight() {
    stale_cancel_probe_preserves_successor(true).await;
}
