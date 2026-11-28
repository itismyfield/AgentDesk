//! The automatic end of a never-started Pending execution, asked by the front that waited a due
//! on it: one close, no turn settled, the original and its claim kept for the turn that follows.
use super::*;
use crate::services::discord::queue_io::input_owner::admission::{AdmissionTicket, admit};
use crate::services::discord::queue_io::input_owner::drive::{Front, FrontClaim, claim_turn};
use crate::services::discord::queue_io::input_owner::pending_recovery::recover;
use crate::services::discord::router::message_handler::{
    OwnerTurn, install_root_recovery_launch, start_idle_for_test,
};
use crate::services::discord::tui_prompt_relay::relay_e2e::discord_mock;
use crate::services::provider::ProviderKind;
use crate::services::session_host::execution_closing;
use crate::services::tui_prompt_dedupe::{binding_context, binding_events as events};
use crate::services::turn_orchestrator::{
    PendingQueueItem, RetryWaitCause, assert_original_and_ticket_kept, v2_queue,
};
use adk_test_support::test_support::wait::{WAIT_CAP, join_within, recv_within};
use futures::FutureExt;
use herdr::HostedState::{Pending, Retired};
use poise::serenity_prelude::ChannelId;
use std::sync::Mutex;
use std::sync::atomic::Ordering;

use crate::services::discord::herdr_terminate::{
    NeverStartedTerminate, TerminationResult, terminate_never_started, test_turn,
};

const SESSION: &str = "4d8f6f0e-6a53-4f0b-9c55-1d2f3a4b5c6d";

fn channel() -> ChannelId {
    ChannelId::new(CHANNEL)
}

/// Runs `body` in test `name`'s child process, on a node storing `stored` whose channel holds one
/// original that waited its first due on the rig's execution and is claimed again.
fn with_waited(
    name: &str,
    stored: Stored,
    body: impl AsyncFnOnce(&herdr::P11TerminationRig, &Node, &FrontClaim),
) {
    let Some((rig, runtime)) = rig_child(&format!("never_started_tests::{name}")) else {
        return;
    };
    let _root = crate::config::TestRuntimeRootGuard::new();
    runtime.block_on(async {
        let node = Node::store(&rig, None, stored).await;
        waited(&node).await;
        let claim = claim(&node).await;
        body(&rig, &node, &claim).await;
        node.finish().await;
    });
}

/// The owner's front, granted as the drive grants it before a start.
async fn grant(node: &Node) -> Front {
    let mailbox = node.shared.mailbox(channel());
    let granted = mailbox.v2_request(|slot| {
        let owner = slot.as_mut().ok_or("no owner")?;
        let front = owner.front().ok_or("no front")?;
        owner.grant_front(&front).map(|_| front)
    });
    granted.await.unwrap()
}

async fn claim(node: &Node) -> FrontClaim {
    let (front, mailbox) = (grant(node).await, node.shared.mailbox(channel()));
    let claimed = claim_turn(&mailbox, front.1.message_id, &front).await;
    claimed.unwrap().expect("the granted front is claimed")
}

/// One original of `node`'s channel whose wait on the rig's execution consumed its first due; the
/// launch context names the session a logged start would carry.
async fn waited(node: &Node) {
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
}

/// Whether `claim` is still the owner's claim on its kept original and ticket, and whether its
/// retry wait remains.
async fn kept(node: &Node, claim: &FrontClaim) -> (bool, bool) {
    let (front, number, mailbox) = (
        claim.front().clone(),
        claim.number(),
        node.shared.mailbox(channel()),
    );
    let read = mailbox.v2_request(move |slot| {
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
    with_waited(name, Stored::NeverAttached, async |rig, node, claim| {
        recover(&node.shared, channel(), claim).await;
        assert_eq!((closes(rig), state(node).await), (1, Some(Retired)));
        assert!(execution_closing(herdr::EXECUTION), "its nonce stays shut");
        // The same claim still holds the original for its fresh launch; its wait is gone.
        assert_eq!(kept(node, claim).await, (true, false));
        recover(&node.shared, channel(), claim).await;
        assert_eq!(closes(rig), 1, "closed once");
    });
}

#[test]
fn auto_recovery_between_episodes_does_not_finalize_requesting_turn() {
    let name = "auto_recovery_between_episodes_does_not_finalize_requesting_turn";
    with_waited(name, Stored::NeverAttached, async |rig, node, claim| {
        // The earlier episode still holds the channel: nothing closes and it is not settled.
        let token = test_turn::start(&node.shared, channel(), 81, "episode-a").await;
        recover(&node.shared, channel(), claim).await;
        assert_eq!((closes(rig), state(node).await), (0, Some(Pending)));
        assert!(!token.cancelled.load(Ordering::SeqCst));
        let active = test_turn::active(&node.shared, channel()).await;
        assert!(active.is_some_and(|active| Arc::ptr_eq(&active, &token)));
        let episode = Some((81, Some("episode-a".to_string())));
        assert_eq!(test_turn::inflight(channel()), episode);
        // Once it ended by itself the same claim closes the pane, publishing no turn of its own.
        test_turn::finish_naturally(&node.shared, channel(), 81, "episode-a", &token).await;
        recover(&node.shared, channel(), claim).await;
        assert_eq!((closes(rig), state(node).await), (1, Some(Retired)));
        assert!(test_turn::active(&node.shared, channel()).await.is_none());
        assert_eq!(kept(node, claim).await, (true, false));
    });
}

#[test]
fn auto_recovery_refuses_while_any_turn_is_captured() {
    let name = "auto_recovery_refuses_while_any_turn_is_captured";
    with_waited(name, Stored::NeverAttached, async |rig, node, claim| {
        let ask = |number| {
            terminate_never_started(asked(node, claim, number), node.shared.clone(), &node.pool)
        };
        let refused = |why: &str| TerminationResult::Refused(why.into());
        assert_eq!(
            ask(claim.number() + 1).await,
            refused("claim no longer qualifies")
        );
        let _token = test_turn::start(&node.shared, channel(), 82, "episode-b").await;
        assert_eq!(ask(claim.number()).await, refused("turn active"));
        assert!(!execution_closing(herdr::EXECUTION), "no fence was taken");
        assert_eq!((closes(rig), state(node).await), (0, Some(Pending)));
    });
}

#[test]
fn auto_terminate_on_same_nonce_bound_closes_nothing() {
    let name = "auto_terminate_on_same_nonce_bound_closes_nothing";
    with_waited(name, Stored::Bound, async |rig, node, claim| {
        recover(&node.shared, channel(), claim).await;
        let current = asked(node, claim, claim.number());
        let result = terminate_never_started(current, node.shared.clone(), &node.pool).await;
        let bound = "execution is not a never-started pending";
        assert_eq!(result, TerminationResult::Refused(bound.into()));
        assert_eq!((closes(rig), node.row().await), (0, node.bound()));
        assert_eq!(kept(node, claim).await, (true, true));
    });
}

/// Logs a start of `SESSION` by the rig's execution on `logical`'s pane, as its hook would.
fn log_start(logical: &str) -> std::path::PathBuf {
    events::set_test_root(crate::config::runtime_root().as_deref());
    let path = events::log_path(CHANNEL).unwrap().unwrap();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let (at, session) = (chrono::Utc::now(), SESSION.to_string());
    let event = events::BindingEvent {
        seq: 1,
        channel_id: CHANNEL,
        provider: "claude".into(),
        tmux_session: logical.into(),
        execution_nonce: Some(herdr::EXECUTION.into()),
        old: None,
        new: events::BindingTarget::Pending {
            payload_session_id: session,
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
    with_waited(name, Stored::NeverAttached, async |rig, node, claim| {
        // The start was logged first: the judgment reads it under the authority and shuts nothing.
        let logged = log_start(node.key.rsplit_once(':').unwrap().1);
        recover(&node.shared, channel(), claim).await;
        assert_eq!((closes(rig), state(node).await), (0, Some(Pending)));
        assert!(!execution_closing(herdr::EXECUTION));
        // The judgment came first: the fence is shut before any close, for every later start hook.
        std::fs::remove_file(logged).unwrap();
        events::forget_channel_for_tests(CHANNEL);
        probe_after_fence(Box::new(|| {
            Box::pin(async { assert!(execution_closing(herdr::EXECUTION)) })
        }));
        recover(&node.shared, channel(), claim).await;
        assert_eq!((closes(rig), state(node).await), (1, Some(Retired)));
    });
}

#[test]
fn auto_terminate_crash_between_close_and_retire_recovers_by_read_end_then_retire() {
    let name = "auto_terminate_crash_between_close_and_retire_recovers_by_read_end_then_retire";
    with_waited(name, Stored::NeverAttached, async |rig, node, claim| {
        probe_after_close(Box::new(|| {
            Box::pin(async { panic!("stopped between the close and the retire") })
        }));
        let stopped = std::panic::AssertUnwindSafe(recover(&node.shared, channel(), claim));
        assert!(stopped.catch_unwind().await.is_err());
        assert_eq!((closes(rig), state(node).await), (1, Some(Pending)));
        assert_eq!(kept(node, claim).await, (true, true));
        // The wait and its consumed due are durable: the next look reads the end and retires.
        recover(&node.shared, channel(), claim).await;
        assert_eq!((closes(rig), state(node).await), (1, Some(Retired)));
        assert_eq!(kept(node, claim).await, (true, false));
    });
}

#[test]
fn hook_record_and_auto_qualify_share_one_source_authority_key() {
    let name = "hook_record_and_auto_qualify_share_one_source_authority_key";
    with_waited(name, Stored::NeverAttached, async |rig, node, claim| {
        // A start hook that read the fence open holds the authority of the pane its context names.
        events::set_test_root(crate::config::runtime_root().as_deref());
        let context = binding_context::execution_context("claude", herdr::EXECUTION).unwrap();
        let (held, held_rx) = std::sync::mpsc::channel();
        let (judged, judged_rx) = std::sync::mpsc::channel::<()>();
        let hook = std::thread::spawn(move || {
            let pane = context.tmux_session;
            crate::services::tmux_common::with_tmux_source_authority(&pane, |_| {
                held.send(()).unwrap();
                // A judgment under another key ends meanwhile; under the same key it waits here.
                let _ = judged_rx.recv_timeout(std::time::Duration::from_millis(500));
                log_start(&pane);
            })
        });
        recv_within(&held_rx, WAIT_CAP);
        recover(&node.shared, channel(), claim).await;
        let _ = judged.send(());
        join_within(hook, WAIT_CAP);
        assert_eq!((closes(rig), state(node).await), (0, Some(Pending)));
        assert!(!execution_closing(herdr::EXECUTION), "the start came first");
    });
}

#[test]
fn unreadable_start_log_refuses_the_auto_close_and_keeps_the_wait() {
    let name = "unreadable_start_log_refuses_the_auto_close_and_keeps_the_wait";
    with_waited(name, Stored::NeverAttached, async |rig, node, claim| {
        // A start was logged, then the log took a complete line no reader can parse.
        let logged = log_start(node.key.rsplit_once(':').unwrap().1);
        let mut log = std::fs::OpenOptions::new()
            .append(true)
            .open(logged)
            .unwrap();
        std::io::Write::write_all(&mut log, b"{\"seq\":2,\n").unwrap();
        events::forget_channel_for_tests(CHANNEL);
        let current = asked(node, claim, claim.number());
        let result = terminate_never_started(current, node.shared.clone(), &node.pool).await;
        let unreadable = matches!(&result, TerminationResult::Refused(why)
            if why.starts_with("start log unreadable"));
        assert!(unreadable, "{result:?}");
        recover(&node.shared, channel(), claim).await;
        assert_eq!((closes(rig), state(node).await), (0, Some(Pending)));
        assert!(!execution_closing(herdr::EXECUTION), "no fence was taken");
        assert_eq!(kept(node, claim).await, (true, true));
    });
}

#[test]
fn start_idle_closes_the_never_started_pending_before_the_launch_takes_the_original() {
    let name = "start_idle_closes_the_never_started_pending_before_the_launch_takes_the_original";
    let Some((rig, runtime)) = rig_child(&format!("never_started_tests::{name}")) else {
        return;
    };
    let _root = crate::config::TestRuntimeRootGuard::new();
    runtime.block_on(async {
        let node = Node::store(&rig, None, Stored::NeverAttached).await;
        waited(&node).await;
        let (proxy, gateway, _discord) =
            discord_mock::start(discord_mock::DiscordMockState::new()).await;
        let context = discord_mock::serenity_context(proxy, gateway).await;
        crate::services::discord::ready_gateway_for_test(&node.shared, context);
        // The launch is the handoff a user front reaches after its claim; it stops there unsent.
        let launched = Arc::new(Mutex::new(Vec::new()));
        let seen = launched.clone();
        let stop = Box::new(move |owner: OwnerTurn, text: &str| -> Result<(), String> {
            let front = owner.front();
            seen.lock()
                .unwrap()
                .push((front.0.clone(), front.1.message_id, text.to_string()));
            owner.defer_before_submit("fixture_stop");
            Err("NotSent(\"fixture_stop\")".to_string())
        });
        let _launch = install_root_recovery_launch(channel(), stop);
        let front = grant(&node).await;
        let started = start_idle_for_test(node.shared.clone(), channel(), front.clone()).await;
        assert_eq!(started, Err("NotSent(\"fixture_stop\")".to_string()));
        assert_eq!((closes(&rig), state(&node).await), (1, Some(Retired)));
        let original = (
            front.0.clone(),
            front.1.message_id,
            "original input".to_string(),
        );
        assert_eq!(*launched.lock().unwrap(), [original]);
        let mailbox = node.shared.mailbox(channel());
        let read = mailbox.v2_request(move |slot| {
            let owner = slot.as_ref().ok_or("no owner")?;
            assert_original_and_ticket_kept(owner, &front);
            Ok(owner.snapshot.retry.wait.is_none())
        });
        assert!(read.await.unwrap(), "the wait ended with the close");
        node.finish().await;
    });
}
