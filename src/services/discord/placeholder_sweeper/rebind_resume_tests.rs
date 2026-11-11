//! The warm sweep's rebind-origin reaps resume a front the reaped row held exactly once; a row the
//! pass leaves, and an owner downgrade that keeps its row, give no notice.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use poise::serenity_prelude::ChannelId;
use serde_json::json;
use tokio::sync::mpsc;

use super::*;
use crate::services::discord::health::test_support::MockDiscord;
use crate::services::discord::host_teardown_gate::test_support::{
    Stored, channel_key, seed as seed_host, shared_on,
};
use crate::services::discord::inflight::{self, RelayOwnerKind, TurnSource};
use crate::services::discord::queue_io::cancel_backstop_test_support as support;
use crate::services::discord::queue_io::input_owner::admission::{AdmissionTicket, admit};
use crate::services::discord::queue_io::input_owner::drive::{
    self, Drive, Front, Handoff, claim_turn,
};
use crate::services::discord::queue_io::input_owner::effect::{Completion, Leaf, PreparedPayload};
use crate::services::discord::queue_io::input_owner::pump;
use crate::services::session_host::test_support::InjectedLivenessGuard;
use crate::services::session_host::{HostLiveness, HostSessionRef};
use crate::services::turn_orchestrator::{PendingQueueItem, QueuePersistenceContext, v2_queue};
use crate::test_support::wait::{WAIT_CAP, await_within};

#[derive(Clone, Copy, Debug, PartialEq)]
enum Case {
    /// An ownerless rebind origin of an earlier generation: the deadline reap removes it.
    Deadline,
    /// A watcher-owned one whose watcher is proven dead: the dead-watcher reap removes it.
    DeadWatcher,
    /// Born in this generation and not yet past its deadline: the pass keeps it.
    NotYet,
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
                    "target": {"host": "herdr", "owner": format!("reap-{channel}"),
                        "root": "/reap", "execution_nonce": nonce},
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

/// A structurally abandoned rebind origin as `should_reap_abandoned_rebind_origin` reads it.
fn rebind_row(channel: u64, tmux: &str, owner: RelayOwnerKind, birth: u64) -> InflightTurnState {
    let mut row = InflightTurnState::new(
        ProviderKind::Claude,
        channel,
        None,
        0,
        0,
        0,
        String::new(),
        None,
        Some(tmux.to_string()),
        None,
        None,
        0,
    );
    row.rebind_origin = true;
    row.turn_source = TurnSource::ExternalAdopted;
    row.set_relay_owner_kind(owner);
    row.turn_start_offset = Some(0);
    row.rebind_origin_birth_generation = Some(birth);
    row
}

/// Unhooks the channel's owner wake when the case ends.
struct WakeHook(ChannelId);

impl Drop for WakeHook {
    fn drop(&mut self) {
        support::set_owner_wake_for_tests(self.0, None);
    }
}

/// The channel's owner holding one front, refused once by `channel`'s row, and its wake count.
struct HeldFront {
    delivered: Arc<AtomicUsize>,
    looked: mpsc::UnboundedReceiver<bool>,
    wakes: Arc<Mutex<usize>>,
    _unhook: WakeHook,
}

async fn held_front(shared: &Arc<SharedData>, channel: ChannelId) -> HeldFront {
    let id = channel.get();
    let mailbox = shared.mailbox(channel);
    let context = QueuePersistenceContext::new(&ProviderKind::Claude, &shared.token_hash, None);
    let (path, namespace) = v2_queue(&context, channel).unwrap();
    mailbox.v2_adopt(path, namespace).await.unwrap();
    let front: PendingQueueItem = serde_json::from_value(json!({
        "author_id": 7, "message_id": id + 3, "channel_id": id,
        "source_message_ids": [id + 3], "queued_generation": 1, "text": "held front"}))
    .unwrap();
    let _ = admit(&mailbox, &front, AdmissionTicket::new()).await;
    let delivered = Arc::new(AtomicUsize::new(0));
    let (looked_tx, mut looked) = mpsc::unbounded_channel();
    let owner = Arc::new(Drive {
        mailbox: mailbox.clone(),
        channel: id,
        handoff: start_behind_row(mailbox.clone(), id, delivered.clone(), looked_tx),
        settled: None,
    });
    drive::wake(owner.clone());
    let refused = await_within(looked.recv(), WAIT_CAP).await;
    assert_eq!(refused, Some(false), "{id}: held by its row");
    let wakes = Arc::new(Mutex::new(0usize));
    let (seen, woken) = (wakes.clone(), owner);
    let hook = Arc::new(move || {
        *seen.lock().unwrap() += 1;
        drive::wake(woken.clone());
    });
    support::set_owner_wake_for_tests(channel, Some(hook));
    HeldFront {
        delivered,
        looked,
        wakes,
        _unhook: WakeHook(channel),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_warm_rebind_reap_resumes_the_held_front_once_and_a_kept_row_gives_no_notice_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let shared = shared_on(&pool).await;
    let discord = MockDiscord::start().await;
    let provider = ProviderKind::Claude;
    let generation = crate::services::discord::runtime_store::process_generation();
    let cases = [
        (Case::Deadline, 1_479_671_301_387_093_010u64),
        (Case::DeadWatcher, 1_479_671_301_387_093_020),
        (Case::NotYet, 1_479_671_301_387_093_030),
    ];
    for (case, id) in cases {
        let channel = ChannelId::new(id);
        let name = provider.build_tmux_session_name(&format!("c3-reap-{id}"));
        let (owner, birth) = match case {
            Case::Deadline => (RelayOwnerKind::None, generation.wrapping_add(1)),
            Case::DeadWatcher => (RelayOwnerKind::Watcher, generation.wrapping_add(1)),
            Case::NotYet => (RelayOwnerKind::None, generation),
        };
        let row = rebind_row(id, &name, owner, birth);
        let _pane =
            InjectedLivenessGuard::set(HostSessionRef::tmux(&name), HostLiveness::DeadOrAbsent);
        if case == Case::DeadWatcher {
            seed_host(
                &pool,
                &channel_key(&shared, &name),
                &name,
                id,
                Stored::Missing,
            )
            .await;
        }
        inflight::save_inflight_state(&row).unwrap();
        let mut held = held_front(&shared, channel).await;
        let completion = support::KickoffCompletion::install(channel);

        let mut tracker = StalledEditTracker::default();
        let report =
            run_placeholder_sweep_pass(&discord.http, &shared, &provider, &mut tracker).await;
        await_within(completion.processed(0, WAIT_CAP), WAIT_CAP).await;
        let row_left = inflight::inflight_state_file_exists(&provider, id);
        let wakes = *held.wakes.lock().unwrap();
        if case == Case::NotYet {
            assert!(row_left, "{case:?}: the row is kept");
            assert_eq!(
                (report.abandoned, wakes),
                (0, 0),
                "{case:?}: no resume notice"
            );
            assert_eq!(held.delivered.load(Ordering::SeqCst), 0, "{case:?}");
            continue;
        }
        assert_eq!((report.abandoned, row_left), (1, false), "{case:?}: reaped");
        assert_eq!(wakes, 1, "{case:?}: one resume notice");
        let looked = await_within(held.looked.recv(), WAIT_CAP).await;
        assert_eq!(looked, Some(true), "{case:?}");
        assert_eq!(
            held.delivered.load(Ordering::SeqCst),
            1,
            "{case:?}: delivered once"
        );
    }
    pool.close().await;
    db.drop().await;
}

/// A SessionBoundRelay orphan the watcherless reclaim and the supervision downgrade both take.
fn orphan_row(channel: u64, tmux: &str) -> InflightTurnState {
    let stale = chrono::Local::now()
        - chrono::Duration::seconds(inflight::INFLIGHT_STALENESS_THRESHOLD_SECS as i64 + 1);
    let stale = stale.format("%Y-%m-%d %H:%M:%S").to_string();
    serde_json::from_value(json!({
        "version": 9, "provider": "claude", "channel_id": channel, "channel_name": "c3",
        "request_owner_user_id": 7, "user_msg_id": channel + 1, "current_msg_id": 0,
        "current_msg_len": 0, "user_text": "typed in TUI", "source": "text", "session_id": null,
        "tmux_session_name": tmux, "output_path": "/tmp/c3-orphan.jsonl",
        "input_fifo_path": null, "last_offset": 0, "full_response": "",
        "response_sent_offset": 0, "started_at": stale, "updated_at": stale,
        "terminal_delivery_committed": false, "relay_owner_kind": "session_bound_relay",
        "turn_source": "external_input", "injected_prompt_message_id": channel + 2}))
    .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_owner_downgrade_keeps_its_row_and_gives_no_resume_notice() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let shared = crate::services::discord::make_shared_data_for_tests();
    let provider = ProviderKind::Claude;
    for (n, id) in [7_753_510u64, 7_753_520].into_iter().enumerate() {
        let channel = ChannelId::new(id);
        let tmux = format!("AgentDesk-claude-c3-downgrade-{id}");
        let mut row = orphan_row(id, &tmux);
        row.ensure_finalizer_turn_id();
        // Written verbatim: a guarded save would refresh `updated_at` and end the orphan's age.
        let root = crate::services::discord::runtime_store::discord_inflight_root().unwrap();
        let path = inflight::inflight_state_path(&root, &provider, id);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, serde_json::to_vec_pretty(&row).unwrap()).unwrap();
        let held = held_front(&shared, channel).await;
        let completion = support::KickoffCompletion::install(channel);

        let outcome = if n == 0 {
            let pin = inflight::InflightEpisodePin::from_state(&row);
            let reclaim = inflight::reclaim_watcherless_session_bound_relay_owner;
            reclaim(&shared, &provider, id, &pin)
        } else {
            let identity = inflight::InflightTurnIdentity::from_state(&row);
            let downgrade = inflight::downgrade_orphaned_session_bound_relay_owner_locked;
            downgrade(&provider, id, &identity, &tmux)
        };
        await_within(completion.processed(0, WAIT_CAP), WAIT_CAP).await;
        assert_eq!(
            outcome,
            inflight::OrphanRelayReclaimOutcome::Downgraded,
            "{id}"
        );
        let kept = inflight::load_inflight_state(&provider, id).expect("the row stays");
        assert_eq!(
            kept.effective_relay_owner_kind(),
            RelayOwnerKind::None,
            "{id}"
        );
        assert_eq!(*held.wakes.lock().unwrap(), 0, "{id}: no resume notice");
        assert_eq!(held.delivered.load(Ordering::SeqCst), 0, "{id}");
    }
}
