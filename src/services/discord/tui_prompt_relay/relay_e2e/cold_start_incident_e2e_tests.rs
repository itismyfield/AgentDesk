//! Cold deferred attach, stale retry and terminal release through the actual watcher loop.

use super::*;
use discord::router::message_handler::incident_attach;
use discord::turn_completion_events::{TurnCompletionEvent, TurnCompletionPhase};
use poise::serenity_prelude::{MessageId, UserId};
use tokio::sync::broadcast::Receiver;

const BODY: &str = "cold incident A exact terminal body [5754]";
static LOG: Mutex<Vec<u8>> = Mutex::new(Vec::new());

struct Capture;
impl std::io::Write for Capture {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        LOG.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn child(name: &str) -> bool {
    if std::env::var("ADK_COLD_INCIDENT_CHILD").as_deref() == Ok(name) {
        tracing::subscriber::set_global_default(
            tracing_subscriber::fmt()
                .without_time()
                .with_ansi(false)
                .with_max_level(tracing::Level::INFO)
                .with_writer(|| Capture)
                .finish(),
        )
        .unwrap();
        return true;
    }
    let exact = format!("{}::{name}", module_path!().split_once("::").unwrap().1);
    let outer_root = tempfile::tempdir().unwrap();
    let out = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", &exact, "--nocapture"])
        .env("ADK_COLD_INCIDENT_CHILD", name)
        .env("AGENTDESK_ROOT_DIR", outer_root.path())
        .env(
            crate::services::tui_o::cutover::test_override::CHANNELS_ENV,
            "[]",
        )
        .env("AGENTDESK_SINGLE_MESSAGE_PANEL", "0")
        .env("AGENTDESK_STATUS_INTERVAL_SECS", "0")
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{stdout}\n{stderr}");
    assert!(
        stdout.contains("1 passed; 0 failed; 0 ignored;"),
        "{stdout}"
    );
    eprintln!("{stdout}\n{stderr}");
    false
}

fn dequeues(h: &RelayE2eHarness) -> usize {
    String::from_utf8_lossy(&LOG.lock().unwrap())
        .matches(&format!(
            "KICKOFF: starting queued turn for channel {}",
            h.channel_id
        ))
        .count()
}

struct WatcherTasks(discord::task_supervisor::n4d_spawn_observation::Scope);

impl Drop for WatcherTasks {
    fn drop(&mut self) {
        tokio::task::block_in_place(|| {
            let outcomes = tokio::runtime::Handle::current().block_on(self.0.cancel_and_join_all());
            if !std::thread::panicking() {
                assert!(
                    outcomes.iter().all(|(_, outcome)| *outcome
                        == discord::task_supervisor::watcher_completion::Outcome::Returned),
                    "all production watcher tasks returned: {outcomes:?}"
                );
            }
        });
    }
}

struct Incident {
    _tasks: WatcherTasks,
    pane: ProbePane,
    observed: inflight::o_seed_observation::Guard,
    h: RelayE2eHarness,
    row: inflight::InflightTurnState,
    token: Arc<CancelToken>,
    source: PathBuf,
    body_id: u64,
    panel_id: u64,
    queued: u64,
    events: Receiver<TurnCompletionEvent>,
}

impl Incident {
    async fn start() -> Self {
        let mut h = RelayE2eHarness::start().await;
        let mut shared = discord::make_shared_data_for_tests();
        Arc::get_mut(&mut shared)
            .unwrap()
            .ui
            .status_panel_v2_enabled = true;
        {
            let mut settings = shared.settings.write().await;
            settings.owner_user_id = Some(USER_ID);
            settings.allow_all_users = true;
        }
        discord::rebind_channel_session(
            &shared,
            &CLAUDE,
            h.channel_id,
            h.root.path().to_str().unwrap(),
            super::super::SESSION_UUID,
        )
        .await;
        shared.restart.reconcile_done.store(true, Ordering::SeqCst);
        h.shared = shared;
        h.data.shared = h.shared.clone();
        h.register_channel_in_role_map();
        h.cache_relay_transport();
        let base = recent_snowflake_base();
        let (active, queued) = (base | 91, base | 92);
        let (held, token) = hold_a_turn(&h, active).await;
        let message_id = MessageId::new(queued);
        let input = crate::services::turn_orchestrator::Intervention {
            author_id: UserId::new(USER_ID),
            author_is_bot: false,
            message_id,
            queued_generation: discord::runtime_store::process_generation(),
            source_message_ids: vec![message_id],
            source_message_queued_generations: Vec::new(),
            source_text_segments: Vec::new(),
            text: Q.into(),
            mode: crate::services::turn_orchestrator::InterventionMode::Soft,
            created_at: std::time::Instant::now(),
            reply_context: None,
            has_reply_boundary: false,
            merge_consecutive: false,
            pending_uploads: Vec::new(),
            voice_announcement: None,
        };
        assert!(
            queue_io::with_post_enqueue_idle_queue_kick_suppressed(
                discord::mailbox_enqueue_intervention(&h.shared, &CLAUDE, h.channel_id, input)
            )
            .await
            .enqueued
        );
        abort_held_intake(held).await;
        assert!(!token.cancelled.load(Ordering::Acquire));
        assert!(Arc::ptr_eq(
            h.mailbox().await.cancel_token.as_ref().unwrap(),
            &token
        ));
        h.answer_placeholders_immediately();
        let body_id = h.channel_id.say(&h.ctx.http, "...").await.unwrap().id.get();
        let panel_id = h
            .channel_id
            .say(&h.ctx.http, "A panel still processing")
            .await
            .unwrap()
            .id
            .get();
        let tmux = CLAUDE.build_tmux_session_name("p5951-cold-incident-fixture");
        let pane = ProbePane::new(&h, &tmux);
        let source = h.root.path().join("cold-source.jsonl");
        std::fs::write(&source, "").unwrap();
        let generation = crate::services::tmux_common::session_temp_path(&tmux, "generation");
        std::fs::create_dir_all(Path::new(&generation).parent().unwrap()).unwrap();
        std::fs::write(generation, "cold-incident-generation").unwrap();
        let mut row = seed_row(&h, active, &token, Some(&tmux), Some(&source));
        row.current_msg_id = body_id;
        row.status_message_id = Some(panel_id);
        row.set_relay_owner_kind(inflight::RelayOwnerKind::Watcher);
        write_row(&row);
        let observed = inflight::o_seed_observation::Guard::new(&CLAUDE, h.channel_id.get());
        observed.register_source(&source);
        let tasks = WatcherTasks(discord::task_supervisor::n4d_spawn_observation::Scope::new(
            &h.shared,
        ));
        incident_attach(&h.shared, h.ctx.http.clone(), &mut row, false).await;
        pane.pane("busy");
        assert!(
            wait_until(WAIT, {
                let shared = h.shared.clone();
                let channel = h.channel_id;
                move || {
                    let shared = shared.clone();
                    Box::pin(async move { shared.tmux_watchers.contains_key(&channel) })
                }
            })
            .await,
            "scheduled cold retry must attach the real watcher"
        );
        let pin = discord::tmux_watcher_registry::WatcherClaimIncarnation::capture_for_source(
            &h.shared.tmux_watchers,
            &tmux,
            &source,
        )
        .unwrap();
        assert!(pin.paused.load(Ordering::Acquire));
        assert!(
            discord::turn_bridge::resume_pin_tests::resume_incident_watcher(&h.shared, &row, &pin)
        );
        incident_attach(&h.shared, h.ctx.http.clone(), &mut row, true).await;
        assert_eq!(queued_ids(&h).await, vec![queued]);
        assert_eq!(h.durable_queue().len(), 1);
        assert!(h.provider_inputs().is_empty());
        assert!(
            !h.shared
                .restart
                .deferred_hook_channels
                .contains_key(&h.channel_id),
            "preserved Q has no preexisting recovery task"
        );
        let events = h.subscribe_completions();
        Self {
            _tasks: tasks,
            pane,
            observed,
            h,
            row,
            token,
            source,
            body_id,
            panel_id,
            queued,
            events,
        }
    }

    fn hold_edit(&self, id: u64, text: &str) {
        *self.h.mock.hold_edit.lock().unwrap() = Some((id, text.into()));
    }

    async fn held(&self) {
        tokio::time::timeout(WAIT, self.h.mock.edit_held.notified())
            .await
            .unwrap_or_else(|_| {
                let trace = String::from_utf8_lossy(&LOG.lock().unwrap()).into_owned();
                panic!(
                    "expected real Discord PATCH; messages={:?}\n{trace}",
                    self.h.messages()
                )
            });
    }

    fn complete(&self, legacy: bool) -> String {
        let terminal = if legacy {
            result()
        } else {
            "{\"type\":\"system\",\"subtype\":\"stop_hook_summary\"}\n".into()
        };
        let bytes = format!("{}{}{terminal}", user("A cold start input"), said(BODY));
        self.pane.pane("idle");
        std::fs::write(&self.source, &bytes).unwrap();
        bytes
    }

    async fn event(&mut self, id: u64, phase: TurnCompletionPhase) {
        tokio::time::timeout(WAIT, async {
            loop {
                let event = self.events.recv().await.expect("completion bus");
                if event.turn_id == Some(id) && event.phase == phase {
                    assert_eq!(event.channel_id, self.h.channel_id);
                    return;
                }
            }
        })
        .await
        .expect("exact lifecycle completion event");
    }

    async fn consumed(&self, bytes: &str) {
        assert!(
            wait_until(WAIT, {
                let channel = self.h.channel_id.get();
                move || {
                    Box::pin(async move {
                        inflight::o_seed_observation::key_totals(&CLAUDE, channel)
                            .event_count("outer_eof")
                            > 0
                    })
                }
            })
            .await,
            "watcher returns to source EOF after terminal effects"
        );
        assert_eq!(self.observed.snapshot().decoded_chunks.concat(), bytes);
        assert_eq!(std::fs::read_to_string(&self.source).unwrap(), bytes);
        assert_eq!(
            self.h
                .messages()
                .iter()
                .filter(|(_, text)| text == BODY)
                .count(),
            1,
            "the exact provider body is visible once, independently of lifecycle state"
        );
    }
}

impl Drop for Incident {
    fn drop(&mut self) {
        if std::thread::panicking() {
            eprintln!("{}", String::from_utf8_lossy(&LOG.lock().unwrap()));
        }
        self.h.mock.release_held_edit.notify_one();
    }
}

async fn completes_before_panel(legacy: bool) {
    let mut run = Incident::start().await;
    run.hold_edit(run.panel_id, "✅ 완료");
    let bytes = run.complete(legacy);
    run.held().await;
    assert!(
        run.token.cancelled.load(Ordering::Acquire),
        "A exact lease must release before the slow panel edit"
    );
    assert!(
        run.h.messages().iter().any(|(_, text)| text.contains(BODY)),
        "source body is independently visible"
    );
    assert!(
        !run.h.mock.messages.lock().unwrap()[&run.panel_id]
            .1
            .contains("✅ 완료")
    );
    run.event(run.row.user_msg_id, TurnCompletionPhase::MailboxReleased)
        .await;
    let slot = run
        .h
        .shared
        .restart
        .deferred_hook_channels
        .get(&run.h.channel_id)
        .expect("pre-panel release leaves a production queue backstop")
        .value()
        .clone();
    slot.wake.notify_one();
    answered_once(&run.h).await;
    assert_eq!(
        dequeues(&run.h),
        1,
        "Q was actually dequeued once while A panel is held"
    );
    assert_eq!(run.h.provider_starts(), 1);
    run.h.mock.release_held_edit.notify_one();
    run.event(run.row.user_msg_id, TurnCompletionPhase::QueueEligible)
        .await;
    run.consumed(&bytes).await;
    assert!(!super::super::registered_bootstrap_e2e::kick_off(&run.h).await);
    assert_eq!(dequeues(&run.h), 1);
    assert_eq!(run.h.provider_inputs().concat().matches(Q).count(), 1);
    eprintln!(
        "COLD_INCIDENT source_bytes={} legacy={legacy} Q_dequeues=1 provider_inputs=1 body_visible=1 A_mailbox_released=1 A_queue_eligible=1 PG=not-used",
        bytes.len()
    );
}

async fn preserves_successor(same_message: bool) {
    let mut run = Incident::start().await;
    if same_message {
        run.hold_edit(run.panel_id, "✅ 완료");
    } else {
        run.hold_edit(run.body_id, BODY);
    }
    let bytes = run.complete(true);
    run.held().await;
    let mailbox = run.h.shared.mailbox(run.h.channel_id);
    if same_message {
        assert!(run.token.cancelled.load(Ordering::Acquire));
        run.event(run.row.user_msg_id, TurnCompletionPhase::MailboxReleased)
            .await;
    } else {
        let finished = discord::mailbox_finish_turn_if_matches(
            &run.h.shared,
            &CLAUDE,
            run.h.channel_id,
            MessageId::new(run.row.user_msg_id),
        )
        .await;
        assert!(Arc::ptr_eq(
            finished.removed_token.as_ref().unwrap(),
            &run.token
        ));
        discord::saturating_decrement_global_active(&run.h.shared);
    }
    let successor = Arc::new(CancelToken::new());
    let id = if same_message {
        run.row.user_msg_id
    } else {
        run.row.user_msg_id + 2
    };
    assert!(
        mailbox
            .try_start_turn(successor.clone(), UserId::new(USER_ID), MessageId::new(id))
            .await
    );
    discord::increment_global_active(&run.h.shared, "cold_incident_successor");
    let mut row = run.row.clone();
    row.user_msg_id = id;
    row.finalizer_turn_id = id;
    row.turn_nonce = successor.turn_nonce().map(str::to_owned);
    if !same_message {
        // This successor owns the next source range; the same-message case isolates the nonce guard.
        row.turn_start_offset = Some(bytes.len() as u64);
        row.last_offset = bytes.len() as u64;
    }
    assert_ne!(row.turn_nonce, run.row.turn_nonce);
    write_row(&row);
    let path = inflight::inflight_state_path(
        &inflight::inflight_runtime_root().unwrap(),
        &CLAUDE,
        run.h.channel_id.get(),
    );
    let before = std::fs::read(&path).unwrap();
    while run.events.try_recv().is_ok() {}
    let eof_before = run.observed.snapshot().event_count("outer_eof");
    run.h.mock.release_held_edit.notify_one();
    assert!(
        wait_until(WAIT, {
            let channel = run.h.channel_id.get();
            move || {
                Box::pin(async move {
                    inflight::o_seed_observation::key_totals(&CLAUDE, channel)
                        .event_count("outer_eof")
                        > eof_before
                })
            }
        })
        .await,
        "old A terminal pipeline returned to EOF"
    );
    run.consumed(&bytes).await;
    assert!(
        run.h
            .mailbox()
            .await
            .cancel_token
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, &successor)),
        "A must preserve B's exact lease"
    );
    assert!(!successor.cancelled.load(Ordering::Acquire));
    assert_eq!(
        String::from_utf8(std::fs::read(path).unwrap()).unwrap(),
        String::from_utf8(before).unwrap(),
        "B durable row stays byte-identical"
    );
    assert_eq!(queued_ids(&run.h).await, vec![run.queued]);
    assert_eq!(run.h.durable_queue().len(), 1);
    assert_eq!(dequeues(&run.h), 0);
    assert!(run.h.provider_inputs().is_empty());
    assert!(!super::super::registered_bootstrap_e2e::kick_off(&run.h).await);
    while let Ok(event) = run.events.try_recv() {
        assert!(
            same_message && event.phase == TurnCompletionPhase::QueueEligible,
            "B must not publish a mailbox release: {event:?}"
        );
        assert_eq!(event.turn_id, Some(run.row.user_msg_id));
    }
    assert!(run.h.unhandled_requests().is_empty());
    eprintln!(
        "COLD_COUNTER same_message={same_message} source_bytes={} B_lease=preserved B_row=identical Q=preserved Q_dequeues=0 provider_inputs=0",
        bytes.len()
    );
}

#[test]
fn cold_start_terminal_releases_before_panel_and_runs_q_once() {
    if child("cold_start_terminal_releases_before_panel_and_runs_q_once") {
        run_with_large_stack(|| completes_before_panel(false));
    }
}

#[test]
fn cold_start_legacy_result_without_native_turn_runs_q_once() {
    if child("cold_start_legacy_result_without_native_turn_runs_q_once") {
        run_with_large_stack(|| completes_before_panel(true));
    }
}

#[test]
fn cold_start_stale_terminal_preserves_successor() {
    if child("cold_start_stale_terminal_preserves_successor") {
        run_with_large_stack(|| preserves_successor(false));
    }
}

#[test]
fn cold_start_stale_terminal_preserves_same_message_new_nonce() {
    if child("cold_start_stale_terminal_preserves_same_message_new_nonce") {
        run_with_large_stack(|| preserves_successor(true));
    }
}
