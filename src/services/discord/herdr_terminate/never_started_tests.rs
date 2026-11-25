//! The automatic end of a never-started Pending execution, asked by the front that waited a due
//! on it: one close, no turn settled, the original and its claim kept for the turn that follows.
use super::*;
use crate::services::discord::queue_io::input_owner::admission::{AdmissionTicket, admit};
use crate::services::discord::queue_io::input_owner::drive::{FrontClaim, claim_turn};
use crate::services::discord::queue_io::input_owner::pending_recovery::recover;
use crate::services::provider::ProviderKind;
use crate::services::session_host::execution_closing;
use crate::services::tui_prompt_dedupe::{binding_context, binding_events as events};
use crate::services::turn_orchestrator::{
    PendingQueueItem, RetryWaitCause, assert_original_and_ticket_kept, v2_queue,
};
use futures::FutureExt;
use herdr::HostedState::{Pending, Retired};
use poise::serenity_prelude::ChannelId;
use std::sync::atomic::Ordering;

use super::super::{NeverStartedTerminate, TerminationResult, terminate_never_started, test_turn};

const SESSION: &str = "4d8f6f0e-6a53-4f0b-9c55-1d2f3a4b5c6d";

fn channel() -> ChannelId {
    ChannelId::new(CHANNEL)
}

fn child(name: &str) -> Option<(herdr::P11TerminationRig, tokio::runtime::Runtime)> {
    rig_child(&format!("never_started_tests::{name}"))
}

async fn claim(node: &Node) -> FrontClaim {
    let mailbox = node.shared.mailbox(channel());
    let granted = mailbox.v2_request(|slot| {
        let owner = slot.as_mut().ok_or("no owner")?;
        let front = owner.front().ok_or("no front")?;
        owner.grant_front(&front).map(|_| front)
    });
    let front = granted.await.unwrap();
    let claimed = claim_turn(&mailbox, front.1.message_id, &front).await;
    claimed.unwrap().expect("the granted front is claimed")
}

/// One original of `node`'s channel whose wait on the rig's execution consumed its first due,
/// claimed again; the launch context names the session a logged start would carry.
async fn waited(node: &Node) -> FrontClaim {
    let context = binding_context::BindingContext {
        schema: 1,
        provider: "claude".into(),
        created_at: chrono::Utc::now(),
        execution_nonce: herdr::EXECUTION.into(),
        tmux_session: node.key.rsplit_once(':').unwrap().1.into(),
        channel_id: Some(CHANNEL),
        owner_runtime_root: "/root".into(),
        host: None,
        expected_native_session_id: Some(SESSION.into()),
        launch_mode: "fresh".into(),
        provider_root: None,
        first_prompt_digest: None,
        source_policy: None,
        resume_start: None,
    };
    let own = binding_context::context_path("claude", herdr::EXECUTION).unwrap();
    std::fs::create_dir_all(own.parent().unwrap()).unwrap();
    std::fs::write(&own, serde_json::to_vec(&context).unwrap()).unwrap();
    let mailbox = node.shared.mailbox(channel());
    let provider = ProviderKind::Claude;
    let persistence =
        crate::services::discord::queue_persistence_context(&node.shared, &provider, channel());
    let (path, namespace) = v2_queue(&persistence, channel()).unwrap();
    mailbox.v2_adopt(path, namespace).await.unwrap();
    let original: PendingQueueItem = serde_json::from_value(json!({"author_id":7,
        "message_id":71,"channel_id":CHANNEL,"source_message_ids":[71],
        "queued_generation":1,"text":"original input"}))
    .unwrap();
    let _ = admit(&mailbox, &original, AdmissionTicket::new()).await;
    let first = claim(node).await;
    let entered = mailbox.v2_request(move |slot| {
        let owner = slot.as_mut().ok_or("no owner")?;
        let cause = RetryWaitCause::PendingUnstarted {
            nonce: herdr::EXECUTION.into(),
        };
        owner.enter_retry_wait_claimed((first.front(), first.number()), cause, 0)?;
        owner.tick(30_000)
    });
    entered.await.unwrap();
    claim(node).await
}

/// Whether `claim` is still the owner's claim on its kept original and ticket, and whether its
/// retry wait remains.
async fn kept(node: &Node, claim: &FrontClaim) -> (bool, bool) {
    let (front, number) = (claim.front().clone(), claim.number());
    let read = node.shared.mailbox(channel()).v2_request(move |slot| {
        let owner = slot.as_ref().ok_or("no owner")?;
        assert_original_and_ticket_kept(owner, &front);
        let current = owner.claim_number() == number && owner.claimed(front.1.message_id, &front);
        Ok((current, owner.snapshot.retry.wait.is_some()))
    });
    read.await.unwrap()
}

fn asked(node: &Node, claim: &FrontClaim, number: u64) -> NeverStartedTerminate {
    NeverStartedTerminate {
        session_key: node.key.clone(),
        execution_nonce: herdr::EXECUTION.into(),
        session_id: SESSION.into(),
        front: claim.front().clone(),
        claim_number: number,
    }
}

async fn state(node: &Node) -> Option<herdr::HostedState> {
    node.row().await.map(|(state, _)| state)
}

#[test]
fn never_started_b_pending_is_closed_once_then_original_delivered_once() {
    let name = "never_started_b_pending_is_closed_once_then_original_delivered_once";
    let Some((rig, runtime)) = child(name) else {
        return;
    };
    let _root = crate::config::TestRuntimeRootGuard::new();
    runtime.block_on(async {
        let node = Node::store(&rig, None, Stored::NeverAttached).await;
        let claim = waited(&node).await;
        recover(&node.shared, channel(), &claim).await;
        assert_eq!((closes(&rig), state(&node).await), (1, Some(Retired)));
        assert!(execution_closing(herdr::EXECUTION), "its nonce stays shut");
        // The same claim still holds the original for its fresh launch; its wait is gone.
        assert_eq!(kept(&node, &claim).await, (true, false));
        assert!(test_turn::active(&node.shared, channel()).await.is_none());
        recover(&node.shared, channel(), &claim).await;
        assert_eq!(closes(&rig), 1, "closed once");
        node.finish().await;
    });
}

#[test]
fn auto_recovery_between_episodes_does_not_finalize_requesting_turn() {
    let name = "auto_recovery_between_episodes_does_not_finalize_requesting_turn";
    let Some((rig, runtime)) = child(name) else {
        return;
    };
    let _root = crate::config::TestRuntimeRootGuard::new();
    runtime.block_on(async {
        let node = Node::store(&rig, None, Stored::NeverAttached).await;
        let claim = waited(&node).await;
        // The earlier episode still holds the channel: nothing closes and it is not settled.
        let token = test_turn::start(&node.shared, channel(), 81, "episode-a").await;
        recover(&node.shared, channel(), &claim).await;
        assert_eq!((closes(&rig), state(&node).await), (0, Some(Pending)));
        assert!(!token.cancelled.load(Ordering::SeqCst));
        let active = test_turn::active(&node.shared, channel()).await;
        assert!(active.is_some_and(|active| Arc::ptr_eq(&active, &token)));
        let episode = Some((81, Some("episode-a".to_string())));
        assert_eq!(test_turn::inflight(channel()), episode);
        assert_eq!(kept(&node, &claim).await, (true, true));
        // Once it ended by itself the same claim closes the pane, publishing no turn of its own.
        test_turn::finish_naturally(&node.shared, channel(), 81, "episode-a", &token).await;
        recover(&node.shared, channel(), &claim).await;
        assert_eq!((closes(&rig), state(&node).await), (1, Some(Retired)));
        assert!(test_turn::active(&node.shared, channel()).await.is_none());
        assert_eq!(kept(&node, &claim).await, (true, false));
        node.finish().await;
    });
}

#[test]
fn auto_recovery_refuses_while_any_turn_is_captured() {
    let Some((rig, runtime)) = child("auto_recovery_refuses_while_any_turn_is_captured") else {
        return;
    };
    let _root = crate::config::TestRuntimeRootGuard::new();
    runtime.block_on(async {
        let node = Node::store(&rig, None, Stored::NeverAttached).await;
        let claim = waited(&node).await;
        let refused = |why: &str| TerminationResult::Refused(why.into());
        // A claim another wake replaced asks for nothing.
        let stale = asked(&node, &claim, claim.number() + 1);
        let result = terminate_never_started(stale, node.shared.clone(), &node.pool).await;
        assert_eq!(result, refused("claim no longer qualifies"));
        let token = test_turn::start(&node.shared, channel(), 82, "episode-b").await;
        let current = asked(&node, &claim, claim.number());
        let result = terminate_never_started(current, node.shared.clone(), &node.pool).await;
        assert_eq!(result, refused("turn active"));
        assert!(!execution_closing(herdr::EXECUTION), "no fence was taken");
        assert!(!token.cancelled.load(Ordering::SeqCst));
        assert_eq!((closes(&rig), state(&node).await), (0, Some(Pending)));
        node.finish().await;
    });
}

#[test]
fn auto_terminate_on_same_nonce_bound_closes_nothing() {
    let Some((rig, runtime)) = child("auto_terminate_on_same_nonce_bound_closes_nothing") else {
        return;
    };
    let _root = crate::config::TestRuntimeRootGuard::new();
    runtime.block_on(async {
        let node = Node::bind(&rig, None).await;
        let claim = waited(&node).await;
        recover(&node.shared, channel(), &claim).await;
        let current = asked(&node, &claim, claim.number());
        let result = terminate_never_started(current, node.shared.clone(), &node.pool).await;
        let bound = "execution is not a never-started pending";
        assert_eq!(result, TerminationResult::Refused(bound.into()));
        assert_eq!((closes(&rig), node.row().await), (0, node.bound()));
        assert_eq!(kept(&node, &claim).await, (true, true));
        // An operator's terminate of that Bound execution is what it was.
        let (status, reply) = node.terminate(herdr::EXECUTION).await;
        assert_eq!(
            (status, &reply["result"]),
            (StatusCode::OK, &json!("retired"))
        );
        assert_eq!(closes(&rig), 1);
        node.finish().await;
    });
}

/// Logs a start of `SESSION` by the rig's execution on `logical`'s pane, as its hook would.
fn log_start(logical: &str) -> std::path::PathBuf {
    let path = events::log_path(CHANNEL).unwrap().unwrap();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let at = chrono::Utc::now();
    let event = events::BindingEvent {
        seq: 1,
        channel_id: CHANNEL,
        provider: "claude".into(),
        tmux_session: logical.into(),
        execution_nonce: Some(herdr::EXECUTION.into()),
        old: None,
        new: events::BindingTarget::Pending {
            payload_session_id: SESSION.into(),
            payload_transcript_path: None,
        },
        cause: events::BindingCause::Startup,
        parent_hint: None,
        evidence: events::BindingEvidence {
            hook_event: Some("session_start".into()),
            received_at: at,
        },
        committed_at: at,
    };
    let row = serde_json::to_string(&event).unwrap();
    std::fs::write(&path, format!("{row}\n")).unwrap();
    events::forget_channel_for_tests(CHANNEL);
    path
}

#[test]
fn start_log_racing_after_fence_row_check_is_ordered_by_source_authority() {
    let name = "start_log_racing_after_fence_row_check_is_ordered_by_source_authority";
    let Some((rig, runtime)) = child(name) else {
        return;
    };
    let _root = crate::config::TestRuntimeRootGuard::new();
    runtime.block_on(async {
        let node = Node::store(&rig, None, Stored::NeverAttached).await;
        let claim = waited(&node).await;
        // The start was logged first: the judgment reads it under the authority and shuts nothing.
        let logged = log_start(node.key.rsplit_once(':').unwrap().1);
        recover(&node.shared, channel(), &claim).await;
        assert_eq!((closes(&rig), state(&node).await), (0, Some(Pending)));
        assert!(!execution_closing(herdr::EXECUTION));
        assert_eq!(kept(&node, &claim).await, (true, true));
        // The judgment came first: the fence is shut before any close, so a start hook arriving
        // from here on reads it under the same authority and records nothing.
        std::fs::remove_file(logged).unwrap();
        events::forget_channel_for_tests(CHANNEL);
        probe_after_fence(Box::new(|| {
            Box::pin(async { assert!(execution_closing(herdr::EXECUTION)) })
        }));
        recover(&node.shared, channel(), &claim).await;
        assert_eq!((closes(&rig), state(&node).await), (1, Some(Retired)));
        node.finish().await;
    });
}

#[test]
fn auto_terminate_crash_between_close_and_retire_recovers_by_read_end_then_retire() {
    let name = "auto_terminate_crash_between_close_and_retire_recovers_by_read_end_then_retire";
    let Some((rig, runtime)) = child(name) else {
        return;
    };
    let _root = crate::config::TestRuntimeRootGuard::new();
    runtime.block_on(async {
        let node = Node::store(&rig, None, Stored::NeverAttached).await;
        let claim = waited(&node).await;
        probe_after_close(Box::new(|| {
            Box::pin(async { panic!("stopped between the close and the retire") })
        }));
        let stopped = std::panic::AssertUnwindSafe(recover(&node.shared, channel(), &claim));
        assert!(stopped.catch_unwind().await.is_err());
        assert_eq!((closes(&rig), state(&node).await), (1, Some(Pending)));
        assert_eq!(kept(&node, &claim).await, (true, true));
        // The wait and its consumed due are durable: the next look reads the end and retires.
        recover(&node.shared, channel(), &claim).await;
        assert_eq!((closes(&rig), state(&node).await), (1, Some(Retired)));
        assert_eq!(kept(&node, &claim).await, (true, false));
        node.finish().await;
    });
}
