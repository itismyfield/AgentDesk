//! The leaked-row sweep resumes a front its FOREIGN row held once, only when that recovery removed
//! the row and released no token; a successor row or a released token gives no sweep notice.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use poise::serenity_prelude::{ChannelId, MessageId, UserId};
use serde_json::json;
use tokio::sync::mpsc;

use crate::services::discord::queue_io::cancel_backstop_test_support as support;
use crate::services::discord::queue_io::input_owner::admission::{AdmissionTicket, admit};
use crate::services::discord::queue_io::input_owner::drive::{
    self, Drive, Front, Handoff, claim_turn,
};
use crate::services::discord::queue_io::input_owner::effect::{Completion, Leaf, PreparedPayload};
use crate::services::discord::queue_io::input_owner::pump;
use crate::services::discord::{self as discord, inflight, tui_direct_pending_start};
use crate::services::provider::{CancelToken, ProviderKind};
use crate::services::turn_orchestrator::PendingQueueItem;
use crate::test_support::wait::{WAIT_CAP, await_within};

#[derive(Clone, Copy, Debug, PartialEq)]
enum Case {
    /// No token; the Cancel finalizer clears the row, so the lifecycle clear reads Missing.
    FinalizerClears,
    /// No token; a committed row whose terminal answer was delivered is cleared by its finalizer.
    Committed,
    /// No token; another turn's row replaces this one before the finalizer runs.
    Successor,
    /// The mailbox still holds the stale turn's token; the finalizer releases it and publishes.
    Token,
    /// The row is not stale yet, so the sweep recovers nothing.
    Refused,
}

/// What a start behind a row does (owner_input.rs:677-681): refuse while the channel's row stands,
/// otherwise claim and deliver its front, counting each delivery.
fn start_behind_row(
    mailbox: crate::services::turn_orchestrator::ChannelMailboxHandle,
    channel: u64,
    delivered: Arc<AtomicUsize>,
    looked: mpsc::UnboundedSender<bool>,
) -> Handoff {
    Arc::new(move |front: Front| {
        let (mailbox, delivered, looked) = (mailbox.clone(), delivered.clone(), looked.clone());
        Box::pin(async move {
            if inflight::inflight_state_file_exists(&ProviderKind::Claude, channel) {
                let _ = looked.send(false);
                return Err("NotSent(\"synthetic_waits_idle\")".to_string());
            }
            let claim = claim_turn(&mailbox, front.1.message_id, &front).await?;
            let claim = claim.ok_or("NotSent(\"front_not_granted\")")?;
            let nonce = uuid::Uuid::new_v4().simple().to_string();
            let prepared = PreparedPayload {
                frame: front.1.text.clone(),
                sources: vec![],
                attempt: json!({"nonce": nonce, "provider": "claude",
                    "target": {"host": "herdr", "owner": format!("sweep-{channel}"),
                        "root": "/sweep", "execution_nonce": nonce},
                    "source": {"kind": "pre_source", "expected_session_id": null,
                        "expected_transcript": null, "binding_watermark": null}}),
            };
            let leaf: Leaf = Box::new(|_| Ok(Completion::Submitted));
            let clock = Arc::new(|| 10_000);
            let submitted =
                pump::submit_front(&mailbox, Some(claim), prepared, clock, leaf).await?;
            if submitted != Completion::Submitted {
                return Err(format!("{submitted:?}"));
            }
            delivered.fetch_add(1, Ordering::SeqCst);
            let _ = looked.send(true);
            Ok(())
        })
    })
}

/// A stale noncommitted FOREIGN row the sweep may recover, keyed to `token` when one is given.
fn stale_row(
    channel_id: u64,
    user_msg: u64,
    token: Option<&CancelToken>,
    output: &std::path::Path,
) -> inflight::InflightTurnState {
    let mut row = inflight::InflightTurnState::new(
        ProviderKind::Claude,
        channel_id,
        None,
        1,
        user_msg,
        user_msg + 1,
        "stale foreign".to_string(),
        None,
        Some(format!("sweep-resume-{channel_id}")),
        Some(output.to_string_lossy().to_string()),
        None,
        0,
    );
    let age = tui_direct_pending_start::STALE_FOREIGN_INFLIGHT_MIN_AGE_SECS + 1;
    let stale = (chrono::Local::now() - chrono::Duration::seconds(age))
        .format("%Y-%m-%d %H:%M:%S")
        .to_string();
    row.started_at = stale.clone();
    row.updated_at = stale;
    row.set_relay_owner_kind(inflight::RelayOwnerKind::Watcher);
    row.turn_nonce = token.and_then(|token| token.turn_nonce().map(str::to_owned));
    row.last_offset = std::fs::metadata(output).unwrap().len();
    row.runtime_kind = Some(crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui);
    row.save_generation = 1;
    row
}

fn write_row(row: &inflight::InflightTurnState) {
    let root = discord::runtime_store::discord_inflight_root().unwrap();
    let path = inflight::inflight_state_path(&root, &ProviderKind::Claude, row.channel_id);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, serde_json::to_vec_pretty(row).unwrap()).unwrap();
}

/// Unhooks the channel's owner wake when the case ends.
struct WakeHook(ChannelId);

impl Drop for WakeHook {
    fn drop(&mut self) {
        support::set_owner_wake_for_tests(self.0, None);
    }
}

async fn run_case(case: Case, channel_id: u64) {
    let root = crate::config::TestRuntimeRootGuard::new();
    let provider = ProviderKind::Claude;
    let channel = ChannelId::new(channel_id);
    let shared = discord::make_shared_data_for_tests();
    let registry = discord::health::HealthRegistry::new();
    registry
        .register("claude".to_string(), shared.clone())
        .await;
    let output = root.path().join(format!("sweep-resume-{channel_id}.jsonl"));
    std::fs::write(
        &output,
        r#"{"type":"system","subtype":"init","session_id":"s"}"#,
    )
    .unwrap();
    let user_msg = channel_id + 100;
    let token = Arc::new(CancelToken::new());
    if case == Case::Token {
        let start = discord::mailbox_try_start_turn;
        assert!(
            start(
                &shared,
                channel,
                token.clone(),
                UserId::new(1),
                MessageId::new(user_msg)
            )
            .await
        );
        shared.restart.global_active.store(1, Ordering::Relaxed);
    }
    let held = (case == Case::Token).then_some(token.as_ref());
    let mut row = stale_row(channel_id, user_msg, held, &output);
    if case == Case::Refused {
        row.started_at = chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
        row.updated_at = row.started_at.clone();
    }
    if case == Case::Committed {
        std::fs::write(
            &output,
            r#"{"type":"result","result":"delivered","session_id":"s"}"#,
        )
        .unwrap();
        row.last_offset = std::fs::metadata(&output).unwrap().len();
        row.terminal_delivery_committed = true;
        row.full_response = "delivered".to_string();
        row.response_sent_offset = row.full_response.len();
    }
    write_row(&row);
    // The owner's queue holds one front the row already refused.
    let mailbox = shared.mailbox(channel);
    let dir = discord::runtime_store::discord_pending_queue_root().unwrap();
    let file = dir.join(format!("claude/sweep-resume/{channel_id}.json"));
    mailbox.v2_adopt(file, "sweep-resume".into()).await.unwrap();
    let front: PendingQueueItem = serde_json::from_value(json!({
        "author_id": 7, "message_id": channel_id + 3, "channel_id": channel_id,
        "source_message_ids": [channel_id + 3], "queued_generation": 1, "text": "held front"}))
    .unwrap();
    let _ = admit(&mailbox, &front, AdmissionTicket::new()).await;
    let delivered = Arc::new(AtomicUsize::new(0));
    let (looked_tx, mut looked) = mpsc::unbounded_channel();
    let owner = Arc::new(Drive {
        mailbox: mailbox.clone(),
        channel: channel_id,
        handoff: start_behind_row(mailbox.clone(), channel_id, delivered.clone(), looked_tx),
        settled: None,
    });
    drive::wake(owner.clone());
    assert_eq!(
        await_within(looked.recv(), WAIT_CAP).await,
        Some(false),
        "{case:?}: held by its row"
    );

    let wakes = Arc::new(Mutex::new(0usize));
    let (seen, woken) = (wakes.clone(), owner.clone());
    let hook = Arc::new(move || {
        *seen.lock().unwrap() += 1;
        drive::wake(woken.clone());
    });
    support::set_owner_wake_for_tests(channel, Some(hook));
    let _unhook = WakeHook(channel);
    let completion = support::KickoffCompletion::install(channel);
    let mut events = discord::turn_completion_events::subscribe_turn_completion_events(&shared);
    let successor = case == Case::Successor;
    let successor_row = stale_row(channel_id, user_msg + 50, None, &output);
    let _successor = successor.then(|| {
        tui_direct_pending_start::set_destructive_cancel_post_gate_hook_for_tests(Arc::new(
            move |point| {
                if point == tui_direct_pending_start::DestructiveCancelHookPoint::PreRegistryCas {
                    write_row(&successor_row);
                }
            },
        ))
    });

    let sweep = discord::relay_recovery::leaked_row_sweep::sweep_leaked_inflight_rows;
    let applied = sweep(&registry, &provider).await;
    await_within(completion.processed(0, WAIT_CAP), WAIT_CAP).await;
    let row_left = inflight::inflight_state_file_exists(&provider, channel_id);
    let wakes = *wakes.lock().unwrap();
    match case {
        Case::FinalizerClears | Case::Committed => {
            assert_eq!(
                (applied, row_left),
                (1, false),
                "{case:?}: the row is recovered"
            );
            assert_eq!(wakes, 1, "{case:?}: one resume notice");
            assert_eq!(
                await_within(looked.recv(), WAIT_CAP).await,
                Some(true),
                "{case:?}"
            );
            assert_eq!(
                delivered.load(Ordering::SeqCst),
                1,
                "{case:?}: delivered once"
            );
        }
        Case::Successor | Case::Refused => {
            assert!(row_left, "{case:?}: a row still holds the channel");
            assert_eq!(wakes, 0, "{case:?}: no resume notice");
            assert_eq!(
                delivered.load(Ordering::SeqCst),
                0,
                "{case:?}: nothing delivered"
            );
        }
        Case::Token => {
            assert_eq!(
                (applied, row_left),
                (1, false),
                "{case:?}: the row is recovered"
            );
            assert_eq!(
                wakes, 0,
                "{case:?}: the finalizer's publish owns the notice"
            );
            let mut eligible = 0;
            loop {
                match events.try_recv() {
                    Ok(event) => {
                        eligible +=
                            usize::from(event.channel_id == channel && event.queue_is_eligible());
                    }
                    Err(tokio::sync::broadcast::error::TryRecvError::Empty) => break,
                    Err(other) => panic!("{case:?}: completion bus unreadable: {other:?}"),
                }
            }
            assert_eq!(eligible, 1, "{case:?}: the token release publishes once");
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_sweep_resumes_a_held_front_once_only_when_its_recovery_removed_the_row() {
    let cases = [
        (Case::FinalizerClears, 7_753_210),
        (Case::Committed, 7_753_220),
        (Case::Successor, 7_753_230),
        (Case::Token, 7_753_240),
        (Case::Refused, 7_753_250),
    ];
    for (case, channel) in cases {
        run_case(case, channel).await;
    }
}
