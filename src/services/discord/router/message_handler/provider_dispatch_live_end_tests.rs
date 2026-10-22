//! The row end pass over a live Herdr turn its executor still reads: a runtime of its own on the
//! rig's registry, the channel's actor and this process's row, a Discord fake and a queued input.
use std::path::Path;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use poise::serenity_prelude::{ChannelId, MessageId, UserId};

use crate::services::agent_protocol::RuntimeHandoffKind;
use crate::services::discord::tui_prompt_relay::relay_e2e::discord_mock::{self, DiscordMockState};
use crate::services::discord::{self, SharedData, inflight};
use crate::services::provider::{CancelToken, ProviderKind};
use crate::services::session_host::herdr_socket_rig_tests::{PANE, SHELL};

use super::{HostedRecord, HostedState, PathBuf, StreamMessage, json};

const GENERATION: u64 = 9;
const USER_MSG: u64 = 7_145_000_001;
const PLACEHOLDER: u64 = 7_145_000_002;
const SENTINEL: u64 = 7_145_000_003;

/// The process a live turn's actor runs in, on the calling thread.
pub(in crate::services::discord) struct LiveEnd {
    pub(in crate::services::discord) shared: Arc<SharedData>,
    pub(in crate::services::discord) mock: DiscordMockState,
    pub(in crate::services::discord) provider: ProviderKind,
    pub(in crate::services::discord) channel: ChannelId,
    pub(in crate::services::discord) rt: tokio::runtime::Runtime,
    /// Whether the row was still on disk each time the idle kickoff took the queued input.
    pub(in crate::services::discord) taken: Arc<Mutex<Vec<bool>>>,
    _kick: crate::services::discord::queue_io::IdleQueueKickHookResetForTests,
    _server: tokio::task::JoinHandle<()>,
    /// No channel is O's, so the notice takes the legacy relay.
    _o: crate::services::tui_o::cutover::test_override::ChannelsGuard,
}

impl LiveEnd {
    /// `token` holds the mailbox, finalizer entry and current row of `logical` as intake leaves a
    /// Herdr turn, with one more input queued; Discord refuses the channel's writes until `route`.
    pub(in crate::services::discord) fn start(
        database_url: &str,
        token_hash: &str,
        (provider, channel, logical): (ProviderKind, u64, &str),
        token: &Arc<CancelToken>,
    ) -> Self {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let id = ChannelId::new(channel);
        let nonce = token.turn_nonce().unwrap().to_owned();
        let generation = discord::runtime_store::generation_path().unwrap();
        std::fs::create_dir_all(generation.parent().unwrap()).unwrap();
        std::fs::write(&generation, GENERATION.to_string()).unwrap();
        let (shared, mock, server) = rt.block_on(async {
            let pool = crate::db::postgres::connect_test_pool_with_max_connections(
                database_url,
                "live end",
                4,
            )
            .await
            .unwrap();
            let mock = DiscordMockState::new();
            let (proxy, gateway, server) = discord_mock::start(mock.clone()).await;
            let ctx = discord_mock::serenity_context(proxy, gateway).await;
            let mut shared = discord::make_shared_data_for_tests_with_storage(Some(pool));
            let fresh = Arc::get_mut(&mut shared).unwrap();
            fresh.restart.current_generation = GENERATION;
            fresh.token_hash = token_hash.into();
            {
                let mut settings = shared.settings.write().await;
                settings.owner_user_id = Some(7);
                settings.allow_all_users = true;
            }
            shared.restart.reconcile_done.store(true, Ordering::Release);
            shared.http.cached_serenity_ctx.set(ctx).unwrap();
            shared
                .mailbox(id)
                .restore_active_turn(token.clone(), UserId::new(7), MessageId::new(USER_MSG))
                .await;
            shared.restart.global_active.fetch_add(1, Ordering::Relaxed);
            let key = discord::turn_finalizer::TurnKey::new(id, USER_MSG, GENERATION)
                .with_episode_nonce(Some(&nonce));
            shared.turn_finalizer.register_start_with_completion_admission(
                key,
                provider.clone(),
                inflight::RelayOwnerKind::Watcher,
                discord::turn_finalizer::CompletionAdmissionPlan::AfterTerminalProjectionAndDispositionSettled,
                &shared,
            );
            (shared, mock, server)
        });
        let mut row = inflight::InflightTurnState::new(
            provider.clone(),
            channel,
            Some("live-end".into()),
            7,
            USER_MSG,
            PLACEHOLDER,
            "질문".into(),
            None,
            Some(logical.into()),
            None,
            None,
            0,
        );
        row.turn_nonce = Some(nonce);
        row.born_generation = GENERATION;
        row.runtime_kind = Some(match provider {
            ProviderKind::Codex => RuntimeHandoffKind::CodexTui,
            _ => RuntimeHandoffKind::ClaudeTui,
        });
        inflight::save_inflight_state(&row).unwrap();
        let (taken, kick) = rt.block_on(queued(&shared, &provider, id));
        rt.block_on(async {
            use crate::services::discord::queue_io::cancel_backstop_test_support as observed;
            let listener =
                crate::services::discord::queue_io::spawn_turn_completion_idle_queue_listener;
            listener(shared.clone(), provider.clone());
            let deadline = Instant::now() + Duration::from_secs(15);
            while observed::listener_completed_reconciles(id) == 0 && Instant::now() < deadline {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        });
        Self {
            shared,
            mock,
            provider,
            channel: id,
            rt,
            taken,
            _kick: kick,
            _server: server,
            _o: crate::services::tui_o::cutover::test_override::force_channels(&[]),
        }
    }

    /// One end pass, returning once the task it started settled the turn or `limit` passed.
    pub(in crate::services::discord) fn pass(&self, limit: Duration) {
        self.rt.block_on(async {
            crate::services::discord::recovery_engine::herdr_row_end::row_end_pass(
                &self.shared,
                &self.provider,
            )
            .await;
            let deadline = Instant::now() + limit;
            while Instant::now() < deadline && self.held().await {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        });
    }

    /// Whether any actor still holds the channel's mailbox.
    pub(in crate::services::discord) async fn held(&self) -> bool {
        let snapshot = discord::mailbox_snapshot(&self.shared, self.channel).await;
        snapshot.cancel_token.is_some()
    }

    pub(in crate::services::discord) fn row(&self) -> Option<inflight::InflightTurnState> {
        inflight::load_inflight_state_read_only(&self.provider, self.channel.get())
    }

    pub(in crate::services::discord) fn texts(&self) -> Vec<String> {
        self.mock.written_texts()
    }

    /// The intake QUEUE-GUARD over the row aged past its stale limit: whether it reads the turn
    /// active, why it may not release it, and whether the row stayed as it was.
    pub(in crate::services::discord) fn queue_guard(&self) -> (bool, Option<String>, bool) {
        let root = inflight::inflight_runtime_root().unwrap();
        let path = inflight::inflight_state_path(&root, &self.provider, self.channel.get());
        let mut row = self.row().unwrap();
        let aged = chrono::Local::now() - chrono::Duration::minutes(10);
        row.updated_at = aged.format("%Y-%m-%d %H:%M:%S").to_string();
        std::fs::write(&path, serde_json::to_string_pretty(&row).unwrap()).unwrap();
        let before = std::fs::read(&path).unwrap();
        let (shared, provider, channel) = (&self.shared, &self.provider, self.channel);
        let (active, refusal) = self.rt.block_on(async {
            use crate::services::discord::router::thread_guard_test_api::queue_guard_active;
            let refusal =
                discord::admin_host_guard::turn_release_refusal(shared, provider, channel);
            let refusal = refusal.await;
            (queue_guard_active(shared, provider, channel).await, refusal)
        });
        (active, refusal, std::fs::read(&path).unwrap() == before)
    }

    /// Lets Discord take the channel's writes.
    pub(in crate::services::discord) fn route(&self) {
        self.mock.allow_channel(self.channel.get());
    }

    /// The one body-free notice a live row's end shows.
    pub(in crate::services::discord) fn notice() -> String {
        crate::services::discord::turn_bridge::stale_inflight_message("")
    }

    /// The turn's own bridge over every frame its executor sent, from this process's row, and what
    /// it reported.
    pub(in crate::services::discord) fn bridge(
        &self,
        token: &Arc<CancelToken>,
        frames: Vec<StreamMessage>,
    ) -> Option<discord::turn_bridge::BridgeCompletionSignal> {
        let (tx, rx) = std::sync::mpsc::channel();
        frames.into_iter().for_each(|frame| tx.send(frame).unwrap());
        drop(tx);
        let row = self.row().expect("the row the bridge starts from");
        self.rt.block_on(async {
            let http = self
                .shared
                .http
                .cached_serenity_ctx
                .get()
                .unwrap()
                .http
                .clone();
            let (shared, provider) = (self.shared.clone(), self.provider.clone());
            let gateway = discord::gateway::DiscordGateway::new(http, shared, provider, None);
            let (done, report) = tokio::sync::oneshot::channel();
            let bridge = discord::TurnBridgeContext {
                provider: self.provider.clone(),
                gateway: Arc::new(gateway),
                channel_id: self.channel,
                user_msg_id: Some(MessageId::new(USER_MSG)),
                user_text_owned: "질문".into(),
                request_owner_name: "live-end".into(),
                role_binding: None,
                adk_session_key: None,
                adk_session_name: None,
                adk_session_info: None,
                adk_cwd: None,
                dispatch_id: None,
                dispatch_kind: None,
                memory_recall_usage: Default::default(),
                context_window_tokens: 0,
                context_compact_percent: 0,
                current_msg_id: Some(MessageId::new(PLACEHOLDER)),
                response_sent_offset: 0,
                full_response: String::new(),
                tmux_last_offset: None,
                new_session_id: None,
                defer_watcher_resume: false,
                reuse_status_panel_message: false,
                completion_tx: Some(done),
                is_external_input_tui_direct: false,
                inflight_state: row,
            };
            discord::spawn_turn_bridge(self.shared.clone(), token.clone(), rx, bridge);
            let reported = tokio::time::timeout(Duration::from_secs(30), report).await;
            reported.ok()?.ok()
        })
    }

    /// Waits for the idle kickoff to take the queued input.
    pub(in crate::services::discord) fn wait_taken(&self) -> Vec<bool> {
        self.rt.block_on(async {
            let deadline = Instant::now() + Duration::from_secs(20);
            while Instant::now() < deadline && self.taken.lock().unwrap().is_empty() {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        });
        self.taken.lock().unwrap().clone()
    }
}

/// The next input waiting in the queue, and each time the idle kickoff takes it with whether the
/// row was still on disk then.
async fn queued(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel: ChannelId,
) -> (
    Arc<Mutex<Vec<bool>>>,
    crate::services::discord::queue_io::IdleQueueKickHookResetForTests,
) {
    let intervention = crate::services::discord::Intervention {
        author_id: UserId::new(7),
        author_is_bot: false,
        message_id: MessageId::new(SENTINEL),
        queued_generation: GENERATION,
        source_message_ids: vec![MessageId::new(SENTINEL)],
        source_message_queued_generations: Vec::new(),
        source_text_segments: Vec::new(),
        text: "sentinel".into(),
        mode: crate::services::discord::InterventionMode::Soft,
        created_at: Instant::now(),
        reply_context: None,
        has_reply_boundary: false,
        merge_consecutive: false,
        pending_uploads: Vec::new(),
        voice_announcement: None,
    };
    let persistence =
        crate::services::discord::queue_persistence_context(shared, provider, channel);
    shared
        .mailbox(channel)
        .replace_queue(vec![intervention], persistence)
        .await;
    let taken = Arc::new(Mutex::new(Vec::new()));
    let seen = taken.clone();
    let reset = crate::services::discord::queue_io::set_idle_queue_kick_hook_for_tests(Arc::new(
        move |shared, provider, kicked, _reason| {
            let seen = seen.clone();
            Box::pin(async move {
                if kicked != channel {
                    return None;
                }
                let present =
                    inflight::load_inflight_state_read_only(&provider, kicked.get()).is_some();
                let next = crate::services::discord::mailbox_take_next_automatic_intervention(
                    &shared, &provider, kicked,
                )
                .await;
                if let Some(item) = next.intervention.as_ref() {
                    seen.lock().unwrap().push(present);
                    crate::services::discord::mailbox_abandon_unclaimed_dispatch_after_success(
                        &shared,
                        &provider,
                        kicked,
                        item.message_id,
                        next.dispatch_lease.as_ref().expect("dequeue lease").clone(),
                    )
                    .await;
                }
                Some(crate::services::discord::IdleQueueKickoffChannelOutcome {
                    started: next.intervention.is_some(),
                })
            })
        },
    ));
    (taken, reset)
}

/// The executor's turn reads `path` from a record already on disk; after its prompt it records its
/// input and a part of an answer, then `end` plays the provider's death and the end passes.
fn claude_live_turn<R>(
    fx: &super::Fixture,
    end: impl FnOnce(LiveEnd, &Arc<CancelToken>) -> R,
) -> (Result<(), String>, Vec<StreamMessage>, Option<R>) {
    let record = fx.store_bound();
    let launcher = Arc::new(super::Launcher::default());
    let binding =
        crate::services::tui_prompt_dedupe::runtime_binding_for_tmux_session(fx.logical()).unwrap();
    let (session, path) = (
        binding.session_id.unwrap(),
        PathBuf::from(binding.output_path),
    );
    super::append(
        &path,
        &[json!({"type": "system", "subtype": "init", "sessionId": session})],
    );
    let url = fx.db.as_ref().unwrap().database_url.clone();
    let mut seen = None;
    let (result, messages) = fx.turn(&record, &fx.ports(&launcher), || {
        if !super::wait_for(&fx.finished, "the prompt", || fx.rig.sends().len() == 2) {
            return;
        }
        let at = chrono::Utc::now().to_rfc3339();
        let user = json!({"type": "user", "sessionId": session, "timestamp": at,
            "message": {"role": "user", "content": "질문"}});
        let part = json!({"type": "assistant", "sessionId": session, "timestamp": at,
            "message": {"role": "assistant", "content": [{"type": "text", "text": "부분"}]}});
        super::append(&path, &[user, part]);
        let token = fx.cancel.lock().unwrap().clone();
        let _registry = fx.rig.registry_on_this_thread();
        let live = LiveEnd::start(
            &url,
            super::TOKEN,
            (ProviderKind::Claude, super::CHANNEL, fx.logical()),
            &token,
        );
        seen = Some(end(live, &token));
    });
    (result, messages, seen)
}

/// The pane back at its shell and the provider process gone.
fn kill(fx: &super::Fixture) {
    let root = crate::config::runtime_root().unwrap();
    let context = root.join(format!(
        "runtime/binding_contexts/claude/{}.json",
        super::NONCE
    ));
    fx.rig.show_panes(&[PANE]);
    fx.rig.foreground(&[SHELL]);
    fx.rig.provider_seen(&context, false);
}

/// The bound execution's SessionStart record of its transcript in the pane's log, as its launch
/// left it; admission takes the turn's source from it.
pub(super) fn log_source(fx: &super::Fixture) {
    use crate::services::tui_prompt_dedupe::binding_events as events;
    use std::os::unix::fs::MetadataExt;
    let binding =
        crate::services::tui_prompt_dedupe::runtime_binding_for_tmux_session(fx.logical()).unwrap();
    let meta = std::fs::metadata(&binding.output_path).unwrap();
    let logged = events::binding_events_since(super::CHANNEL, 0).unwrap();
    let now = chrono::Utc::now();
    let event = events::BindingEvent {
        seq: logged.last().map_or(1, |event| event.seq + 1),
        channel_id: super::CHANNEL,
        provider: "claude".into(),
        tmux_session: fx.logical().into(),
        execution_nonce: Some(super::NONCE.into()),
        old: None,
        new: events::BindingTarget::Source(events::SourceId {
            session_id: binding.session_id.unwrap(),
            path: binding.output_path.into(),
            dev: meta.dev(),
            ino: meta.ino(),
        }),
        cause: events::BindingCause::Startup,
        parent_hint: None,
        evidence: events::BindingEvidence {
            hook_event: Some("SessionStart".into()),
            received_at: now,
        },
        committed_at: now,
    };
    let log = events::test_root()
        .unwrap()
        .join(events::BINDING_EVENTS_DIR);
    std::fs::create_dir_all(&log).unwrap();
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log.join(format!("{}.log", super::CHANNEL)))
        .unwrap();
    let line = format!("{}\n", serde_json::to_string(&event).unwrap());
    std::io::Write::write_all(&mut file, line.as_bytes()).unwrap();
}

/// The runtime binding the turn's reader hands off through, as a watcher reads it.
fn handoff(fx: &super::Fixture) -> (String, u64) {
    let binding =
        crate::services::tui_prompt_dedupe::runtime_binding_for_tmux_session(fx.logical()).unwrap();
    (binding.output_path, binding.last_offset)
}

/// A stale live Claude turn whose provider died mid-record: the queue guard and a refused notice keep
/// it; the next pass settles it once, wakes the reader without a hand-off, starts the queued input.
#[test]
fn a_live_claude_turn_whose_provider_died_ends_once_and_frees_its_channel_pg() {
    let fx = super::Fixture::new("liveend", None);
    let (result, messages, seen) = claude_live_turn(&fx, |live, token| {
        let before = handoff(&fx);
        let record = json!({"type": "assistant", "message": {"role": "assistant",
            "content": [{"type": "text", "text": "끊긴"}]}})
        .to_string();
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&before.0)
            .unwrap();
        std::io::Write::write_all(&mut file, &record.as_bytes()[..record.len() / 2]).unwrap();
        kill(&fx);
        let (active, refusal, kept) = live.queue_guard();
        assert!(
            active && kept && refusal.is_some(),
            "the queue guard keeps a Herdr turn: {refusal:?}"
        );
        assert!(!token.cancelled.load(Ordering::SeqCst));
        live.pass(Duration::from_secs(5));
        let refused = (
            live.texts(),
            live.row().is_some(),
            live.rt.block_on(live.held()),
        );
        assert_eq!(
            refused,
            (Vec::<String>::new(), true, true),
            "the refused notice keeps the turn"
        );
        assert_eq!(fx.row(), Some(HostedState::Retired));
        assert!(!token.is_completion_cleanup() && !token.cancelled.load(Ordering::SeqCst));
        live.route();
        live.pass(Duration::from_secs(15));
        let ended = (
            live.texts(),
            live.row().is_none(),
            live.rt.block_on(live.held()),
        );
        (
            ended,
            token.is_completion_cleanup(),
            live.wait_taken(),
            before,
        )
    });
    let (ended, woken, taken, before) = seen.expect("the turn reached its prompt");
    assert_eq!(ended, (vec![LiveEnd::notice()], true, false));
    assert!(woken, "the settled turn's reader was woken");
    assert_eq!(
        taken,
        vec![false],
        "the queued input started once, after the row"
    );
    assert_eq!(result, Ok(()), "the woken reader joined");
    assert!(
        !messages
            .iter()
            .any(|m| matches!(m, StreamMessage::RuntimeReady { .. })),
        "{messages:?}"
    );
    assert_eq!(handoff(&fx), before, "the woken read hands nothing off");
    assert_eq!(
        fx.rig.sends(),
        super::prompt_sends(),
        "no pane input after the prompt"
    );
    assert_eq!(fx.row(), Some(HostedState::Retired));
}

/// A terminal the dying provider flushed is read after the end is seen, so it stays the reader's:
/// the pass writes nothing and the reader admits it.
#[test]
fn a_terminal_flushed_as_the_provider_died_is_left_to_its_reader_pg() {
    let fx = super::Fixture::new("liveterm", None);
    let (result, messages, seen) = claude_live_turn(&fx, |live, _| {
        live.route();
        let binding =
            crate::services::tui_prompt_dedupe::runtime_binding_for_tmux_session(fx.logical())
                .unwrap();
        let session = binding.session_id.unwrap();
        let answer = json!({"type": "assistant", "sessionId": session, "message": {
            "role": "assistant", "stop_reason": "end_turn",
            "content": [{"type": "text", "text": "답"}]}});
        let done = json!({"type": "system", "subtype": "turn_duration", "sessionId": session});
        let flush = Mutex::new(Some((PathBuf::from(binding.output_path), [answer, done])));
        let _flushed = crate::services::discord::recovery_engine::herdr_row_end::tests::on_while(
            "live_after_end",
            move || {
                if let Some((path, lines)) = flush.lock().unwrap().take() {
                    super::append(&path, &lines);
                }
                Box::pin(async {})
            },
        );
        kill(&fx);
        live.pass(Duration::from_secs(3));
        (live.texts(), live.row().is_some(), fx.row())
    });
    assert_eq!(seen, Some((Vec::new(), true, Some(HostedState::Bound))));
    assert_eq!(result, Ok(()));
    assert!(
        messages
            .iter()
            .any(|m| matches!(m, StreamMessage::RuntimeReady { .. })),
        "{messages:?}"
    );
}

/// A terminal the dying provider wrote without its newline is the reader's: the pass leaves it,
/// the reader reads it to the file's end and its bridge settles the turn once and frees the channel.
#[test]
fn a_terminal_left_without_its_newline_is_settled_once_by_its_own_reader_pg() {
    let fx = super::Fixture::new("liveraw", None);
    let (result, messages, seen) = claude_live_turn(&fx, |live, token| {
        live.route();
        let (path, _) = handoff(&fx);
        let answer = json!({"type": "assistant", "message": {"role": "assistant",
            "stop_reason": "end_turn", "content": [{"type": "text", "text": "답"}]}});
        super::append(Path::new(&path), &[answer]);
        let done = json!({"type": "system", "subtype": "turn_duration"}).to_string();
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        std::io::Write::write_all(&mut file, done.as_bytes()).unwrap();
        kill(&fx);
        live.pass(Duration::from_secs(3));
        let left = (live.texts(), live.row().is_some(), fx.row());
        (
            left,
            std::fs::metadata(&path).unwrap().len(),
            token.clone(),
            live,
        )
    });
    let (left, len, token, live) = seen.expect("the turn reached its prompt");
    assert_eq!(left, (Vec::new(), true, Some(HostedState::Bound)));
    assert_eq!(result, Ok(()), "the reader joined");
    let ends: Vec<u64> = messages
        .iter()
        .filter_map(|m| match m {
            StreamMessage::ClaudeTuiTerminalDone {
                complete_record_end,
                ..
            } => Some(*complete_record_end),
            _ => None,
        })
        .collect();
    assert_eq!(ends, [len], "one terminal, read to the file's end");
    log_source(&fx);
    let _registry = fx.rig.registry_on_this_thread();
    let finalized = crate::services::discord::turn_bridge::BridgeCompletionSignal::Finalized;
    assert_eq!(
        live.bridge(&token, messages),
        Some(finalized),
        "settled once"
    );
    let settled = (live.row().is_none(), live.rt.block_on(live.held()));
    assert_eq!(
        settled,
        (true, false),
        "the bridge freed the row and the channel"
    );
    assert_eq!(live.wait_taken().len(), 1, "the queued input started once");
    let texts = live.texts();
    let delivered = texts.iter().any(|text| text.contains("답"));
    assert!(
        delivered && !texts.contains(&LiveEnd::notice()),
        "{texts:?}"
    );
}

/// After its execution ended, a turn the pass does not own, or whose source it cannot read, is
/// left as it was; the unread source is logged once for the turn.
#[test]
fn a_live_turn_the_pass_cannot_own_or_read_is_left_as_it_was_pg() {
    use crate::services::agent_protocol::NativeTerminalKind;
    let fx = super::Fixture::new("livekeep", None);
    let nonce = |to: &str| {
        let HostedRecord::Known(mut record) = fx.record() else {
            panic!("the stored record")
        };
        record.execution_nonce = to.into();
        record.source_ref.execution_nonce = to.into();
        record.expected.as_mut().unwrap().binding_nonce = to.into();
        fx.store(&record);
    };
    let (_, _, seen) = claude_live_turn(&fx, |live, token| {
        live.route();
        kill(&fx);
        let untouched = |live: &LiveEnd| {
            let kind = live.row().map(|row| row.tui_terminal_kind);
            (
                live.texts().len(),
                kind,
                fx.row(),
                token.is_completion_cleanup(),
            )
        };
        let mut row = live.row().unwrap();
        row.tui_terminal_kind = Some(NativeTerminalKind::Aborted);
        inflight::save_inflight_state(&row).unwrap();
        live.pass(Duration::from_secs(2));
        let admitted = untouched(&live);
        row.tui_terminal_kind = None;
        inflight::save_inflight_state(&row).unwrap();
        nonce("successor-execution");
        live.pass(Duration::from_secs(2));
        let successor = untouched(&live);
        nonce(super::NONCE);
        let (path, _) = handoff(&fx);
        let kept = std::fs::read(&path).unwrap();
        std::fs::remove_file(&path).unwrap();
        std::fs::write(&path, kept).unwrap();
        let logs = Arc::new(Mutex::new(Vec::new()));
        let writer = Logs(logs.clone());
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::INFO)
            .with_ansi(false)
            .with_writer(move || writer.clone())
            .finish();
        crate::logging::test_capture::pin_callsite_interest();
        tracing::subscriber::with_default(subscriber, || {
            live.pass(Duration::from_secs(2));
            live.pass(Duration::from_secs(2));
        });
        let unread = untouched(&live);
        fx.cancel_now();
        let logs = String::from_utf8(logs.lock().unwrap().clone()).unwrap();
        (
            admitted,
            successor,
            unread,
            logs.matches("execution_ended_source_unread").count(),
        )
    });
    let (admitted, successor, unread, logged) = seen.expect("the turn reached its prompt");
    let kept = |kind| (0, Some(kind), Some(HostedState::Bound), false);
    assert_eq!(
        admitted,
        kept(Some(NativeTerminalKind::Aborted)),
        "an admitted terminal owns it"
    );
    assert_eq!(
        successor,
        kept(None),
        "another execution's record ends nothing"
    );
    assert_eq!(unread, kept(None), "a replaced source is not read");
    assert_eq!(logged, 1, "the unread source is logged once for the turn");
}

#[derive(Clone)]
struct Logs(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Logs {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
