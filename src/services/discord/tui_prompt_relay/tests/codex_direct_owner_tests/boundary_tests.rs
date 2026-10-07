//! Each Codex direct input's claim starts at its own prompt's end in the rollout, however
//! far the idle loop's scan cursor moved before the observer claimed.
use super::*;
use crate::services::tui_prompt_dedupe::ObservedTuiPrompt;

const PROMPT2: &str = "codex direct prompt 5704 second";
const RESPONSE2: &str = "DIRECT_5704_SECOND";
const PROMPT3: &str = "codex direct prompt 5704 third";
const RESPONSE3: &str = "DIRECT_5704_THIRD";

fn user_line_for(text: &str) -> String {
    rollout_line(serde_json::json!({"type": "response_item", "payload": {
        "type": "message", "role": "user", "content": [{"type": "input_text", "text": text}]}}))
}

/// The record Codex writes right after the prompt; the scanner reads it as a non-prompt line.
fn item_completed_line_for(text: &str) -> String {
    rollout_line(
        serde_json::json!({"type": "event_msg", "payload": {"type": "item_completed",
        "item": {"type": "UserMessage", "content": [{"type": "text", "text": text}]}}}),
    )
}

/// A record the parser ignores, `len` bytes long with its newline.
fn padding_line(len: usize) -> String {
    let empty = rollout_line(serde_json::json!({"type": "padding", "pad": ""}));
    let pad = "x".repeat(len.checked_sub(empty.len()).expect("room for padding"));
    rollout_line(serde_json::json!({"type": "padding", "pad": pad}))
}

fn token_count_line() -> String {
    rollout_line(serde_json::json!({"type": "event_msg", "payload": {"type": "token_count"}}))
}

/// One Codex channel under the real idle rollout loop and observer, Discord recorded.
struct Fixture {
    shared: Arc<SharedData>,
    codex: CodexChannel,
    requests: Requests,
    frames: Arc<AtomicUsize>,
    finalized: tokio::sync::broadcast::Receiver<inflight::InflightSignal>,
    observed: tokio::sync::broadcast::Receiver<ObservedTuiPrompt>,
    _relay: crate::services::cluster::stream_relay::StreamRelayHandle,
    _rest: crate::services::discord::shared_state::test_rest::Guard,
    _server: AbortOnDrop,
    _registry: Arc<crate::services::discord::health::HealthRegistry>,
}

impl Fixture {
    async fn start(root: &Path, channel: u64, tmux: &str) -> Self {
        Self::start_with(root, channel, tmux, false).await
    }

    /// `resume`: a restarted process — the rollout stays and the binding cursor sits at its end.
    async fn start_with(root: &Path, channel: u64, tmux: &str, resume: bool) -> Self {
        enable_session_bound_delivery();
        let mut shared = crate::services::discord::make_shared_data_for_tests();
        // The observer announces direct input through the notify utility bot.
        let registry = Arc::new(crate::services::discord::health::HealthRegistry::new());
        Arc::get_mut(&mut shared)
            .expect("fresh shared")
            .health_registry = Arc::downgrade(&registry);
        let codex = if resume {
            resumed_channel(&shared, root, channel, tmux)
        } else {
            CodexChannel::new(&shared, root, channel, tmux)
        };
        let frames = Arc::new(AtomicUsize::new(0));
        let relay = crate::services::cluster::stream_relay::spawn_stream_relay(
            crate::services::cluster::session_matcher::MatchedChannel {
                channel_id: codex.channel.get().to_string(),
                agent_id: "agent-5704".to_string(),
                provider: ProviderKind::Codex,
                expected_session_name: codex.tmux.clone(),
                expected_rollout_path: codex.rollout.to_str().unwrap().to_string(),
            },
            Arc::new(CountingSink(frames.clone())),
        );
        crate::services::cluster::relay_producer_registry::global_relay_producer_registry()
            .register(codex.tmux.clone(), relay.producer());
        let finalized = shared.inflight_signals.subscribe();
        let (requests, http, server) = recording_discord(codex.channel.get()).await;
        registry
            .set_utility_bot_http_for_tests(
                crate::services::discord::bot_role::UtilityBotRole::Notify,
                http.clone(),
            )
            .await;
        let rest = crate::services::discord::shared_state::test_rest::install(http);
        let observed = crate::services::tui_prompt_dedupe::subscribe_observed_prompts();
        super::super::super::CODEX_IDLE_ROLLOUT_RELAY_STARTED.store(false, Ordering::Release);
        super::super::super::spawn_codex_idle_rollout_relay(shared.clone());
        Self {
            shared,
            codex,
            requests,
            frames,
            finalized,
            observed,
            _relay: relay,
            _rest: rest,
            _server: server,
            _registry: registry,
        }
    }

    /// The Codex UserPromptSubmit hook for `prompt`, and the event it publishes.
    async fn hook(&mut self, prompt: &str) -> ObservedTuiPrompt {
        crate::services::tui_prompt_dedupe::observe_hook_prompt_by_tmux_with_prompt_id_at(
            "codex",
            &self.codex.tmux,
            prompt,
            None,
            chrono::Utc::now(),
        );
        loop {
            let event = tokio::time::timeout(Duration::from_secs(5), self.observed.recv())
                .await
                .expect("the hook publishes its observation")
                .expect("observation channel open");
            if event.tmux_session_name == self.codex.tmux && event.prompt == prompt {
                return event;
            }
        }
    }

    fn append(&self, text: &str) -> u64 {
        append(&self.codex.rollout, text);
        std::fs::metadata(&self.codex.rollout).unwrap().len()
    }

    /// Waits until the idle loop scanned through `end` and polled twice more, as the
    /// observer's announcement and placeholder take in production.
    async fn scanned_past(&self, end: u64) {
        let tmux = self.codex.tmux.clone();
        assert!(
            wait_for(Duration::from_secs(15), || {
                crate::services::tui_prompt_dedupe::runtime_binding_for_tmux_session(&tmux)
                    .is_some_and(|binding| binding.last_offset >= end)
            })
            .await,
            "the idle loop never scanned through {end}"
        );
        let visits = poll_notes(&tmux, PollNote::Visit);
        assert!(
            wait_for(Duration::from_secs(5), || poll_notes(
                &tmux,
                PollNote::Visit
            ) >= visits + 2)
            .await,
            "polls stopped visiting the session"
        );
    }

    /// `(method, path, body)` of Discord requests whose body carries `text`.
    fn sent(&self, text: &str) -> Vec<(String, String, String)> {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, _, body)| body.contains(text))
            .cloned()
            .collect()
    }

    async fn delivered(&self, text: &str) -> bool {
        wait_for(Duration::from_secs(15), || !self.sent(text).is_empty()).await
    }

    fn completed_turns(&mut self) -> usize {
        let mut turns = 0;
        while let Ok(signal) = self.finalized.try_recv() {
            if matches!(signal, inflight::InflightSignal::Completed { channel_id, .. } if channel_id == self.codex.channel.get())
            {
                turns += 1;
            }
        }
        turns
    }

    /// The answer only edits `anchor` (streaming edits included) and its last edit's JSON
    /// content is exactly `text`.
    fn assert_answer(&self, text: &str, anchor: u64) {
        let sent = self.sent(text);
        let anchor = format!("/channels/{}/messages/{anchor}", self.codex.channel.get());
        assert!(
            !sent.is_empty()
                && sent
                    .iter()
                    .all(|(method, path, _)| method == "PATCH" && path.ends_with(&anchor)),
            "{sent:?}"
        );
        let body = &sent.last().unwrap().2;
        let content = serde_json::from_str::<serde_json::Value>(body).unwrap()["content"].clone();
        assert_eq!(content, text);
    }

    /// The turn ended cleanly: row, mailbox and prompt anchor released, no sink frame.
    async fn assert_released(&self) {
        assert!(self.codex.row().is_none(), "row cleared");
        let mailbox =
            crate::services::discord::mailbox_snapshot(&self.shared, self.codex.channel).await;
        assert_eq!(mailbox.active_user_message_id, None, "mailbox released");
        assert!(
            crate::services::tui_prompt_dedupe::prompt_anchor_for_response(
                "codex",
                &self.codex.tmux,
                self.codex.channel.get(),
            )
            .is_none(),
            "prompt anchor consumed"
        );
        assert_eq!(
            self.frames.load(Ordering::SeqCst),
            0,
            "no wrapper sink frame"
        );
    }

    /// After the turn, a non-prompt record moves the scan cursor to the new end.
    async fn assert_cursor_follows_eof(&self) {
        let end = self.append(&token_count_line());
        let tmux = self.codex.tmux.clone();
        assert!(
            wait_for(Duration::from_secs(5), || {
                crate::services::tui_prompt_dedupe::runtime_binding_for_tmux_session(&tmux)
                    .is_some_and(|binding| binding.last_offset == end)
            })
            .await,
            "the scan cursor stays behind the rollout end"
        );
    }

    /// Parks this channel's next tail just before bridge capture, its answer already read.
    fn park_capture(&self) -> (Arc<Notify>, Arc<Notify>) {
        let (entered, resume) = (Arc::new(Notify::new()), Arc::new(Notify::new()));
        *super::super::super::codex_idle_rollout::CAPTURE_PAUSE
            .lock()
            .unwrap() = Some((self.codex.channel.get(), entered.clone(), resume.clone()));
        (entered, resume)
    }

    /// The anchor `text`'s answer edits.
    fn anchor_of(&self, text: &str) -> u64 {
        let sent = self.sent(text);
        let path = &sent.last().expect("answer sent").1;
        path.rsplit('/').next().unwrap().parse().expect("anchor id")
    }

    /// Durable deferred starts on this channel: `(prompt, anchor)`.
    fn pending_starts(&self) -> Vec<(String, u64)> {
        crate::services::discord::tui_direct_pending_start::load_all()
            .into_iter()
            .filter(|record| record.channel_id == self.codex.channel.get())
            .map(|record| (record.prompt_text, record.anchor_message_id))
            .collect()
    }

    /// Each answer reached its own anchor, exactly, in input order, one turn and tail each.
    async fn assert_answers_in_order(&mut self, answers: &[&str], tails: usize) {
        for text in answers {
            assert!(
                self.delivered(text).await,
                "{text} never reached Discord: {:?}",
                self.requests.lock().unwrap()
            );
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
        let mut anchors = Vec::new();
        for text in answers {
            let anchor = self.anchor_of(text);
            self.assert_answer(text, anchor);
            assert!(!anchors.contains(&anchor), "{text} shares an anchor");
            anchors.push(anchor);
        }
        let first_edit = |text: &str| {
            let requests = self.requests.lock().unwrap();
            requests.iter().position(|(_, _, body)| body.contains(text))
        };
        for pair in answers.windows(2) {
            assert!(
                first_edit(pair[0]) < first_edit(pair[1]),
                "answers keep input order"
            );
        }
        assert_eq!(self.completed_turns(), answers.len());
        assert_eq!(tail_starts(self.codex.channel), tails, "one tail per input");
        self.assert_released().await;
    }

    fn finish(self) {
        crate::services::cluster::relay_producer_registry::global_relay_producer_registry()
            .deregister(&self.codex.tmux);
        crate::services::tmux_diagnostics::set_pane_liveness_override_for_tests(
            &self.codex.tmux,
            None,
        );
        crate::services::tui_prompt_dedupe::clear_tmux_runtime_binding(&self.codex.tmux);
    }
}

/// The channel a restarted process rebuilds over the rollout already on disk.
fn resumed_channel(
    shared: &Arc<SharedData>,
    root: &Path,
    channel: u64,
    tmux: &str,
) -> CodexChannel {
    let rollout = root.join(format!("{tmux}-rollout.jsonl"));
    let relay = crate::services::tmux_common::session_temp_path(tmux, "jsonl");
    crate::services::tui_prompt_dedupe::register_tmux_runtime_binding(
        tmux,
        crate::services::tui_prompt_dedupe::TuiRuntimeBinding {
            runtime_kind: RuntimeHandoffKind::CodexTui,
            output_path: rollout.to_str().unwrap().to_string(),
            relay_output_path: Some(relay.clone()),
            input_fifo_path: None,
            session_id: Some("s-5704".to_string()),
            last_offset: std::fs::metadata(&rollout).unwrap().len(),
            relay_last_offset: Some(0),
        },
    );
    let channel = ChannelId::new(channel);
    shared
        .tmux_watchers
        .insert(channel, live_watcher(tmux, &relay));
    crate::services::tmux_diagnostics::set_pane_liveness_override_for_tests(
        tmux,
        Some(crate::services::platform::tmux::PaneLiveness::Live),
    );
    CodexChannel {
        channel,
        tmux: tmux.to_string(),
        rollout,
    }
}

fn run(scenario: impl FnOnce(PathBuf) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()>>>) {
    let _env_lock = crate::config::shared_test_env_lock()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let root = tempfile::tempdir().expect("isolated root");
    let _root = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "AGENTDESK_ROOT_DIR",
        root.path(),
    );
    let _dedupe = crate::services::tui_prompt_dedupe::TEST_LOCK
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let _boot = crate::services::tui_o::cutover::test_override::force_channels(&[]);
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime")
        .block_on(scenario(root.path().to_path_buf()));
}

/// Production's order: the hook fires, Codex writes the prompt and its `item_completed`
/// record, the loop polls past both, then the observer claims; the answer may land on
/// either side of the claim.
async fn cursor_past_prompt(root: PathBuf, channel: u64, tmux: &str, answer_first: bool) {
    let mut fx = Fixture::start(&root, channel, tmux).await;
    let event = fx.hook(PROMPT).await;
    let prompt_end = fx.append(&user_line_for(PROMPT));
    let mut after = item_completed_line_for(PROMPT);
    if answer_first {
        after += &answer_lines();
    }
    fx.append(&after);
    fx.scanned_past(prompt_end).await;
    super::super::super::relay_observed_prompt(&fx.shared, event).await;
    let anchor = fx
        .codex
        .row()
        .expect("the observer claims a synthetic row")
        .user_msg_id;
    if !answer_first {
        fx.append(&answer_lines());
    }
    assert!(
        fx.delivered(RESPONSE).await,
        "answer never reached Discord: {:?}",
        fx.requests.lock().unwrap()
    );
    tokio::time::sleep(Duration::from_secs(2)).await;
    fx.assert_answer(RESPONSE, anchor);
    assert_eq!(fx.completed_turns(), 1, "one bridge turn delivers");
    assert_eq!(tail_starts(fx.codex.channel), 1, "one tail");
    fx.assert_released().await;
    fx.assert_cursor_follows_eof().await;
    fx.finish();
}

#[test]
fn codex_direct_answer_reaches_discord_after_the_cursor_passes_the_prompt_record() {
    run(|root| {
        Box::pin(cursor_past_prompt(
            root,
            5_704_700,
            "AgentDesk-codex-5704-booked",
            false,
        ))
    });
}

#[test]
fn codex_direct_answer_reaches_discord_when_it_finishes_before_the_claim() {
    run(|root| {
        Box::pin(cursor_past_prompt(
            root,
            5_704_800,
            "AgentDesk-codex-5704-finished",
            true,
        ))
    });
}

/// A second direct input lands and is scanned while the first waits for its claim: each
/// answer reaches its own anchor once, first before second.
#[test]
fn codex_direct_answer_keeps_its_boundary_when_a_later_direct_input_lands_first() {
    run(|root| {
        Box::pin(async move {
            let mut fx = Fixture::start(&root, 5_704_910, "AgentDesk-codex-5704-overlap").await;
            let first = fx.hook(PROMPT).await;
            fx.append(
                &(user_line_for(PROMPT) + &item_completed_line_for(PROMPT) + &answer_lines()),
            );
            let second = fx.hook(PROMPT2).await;
            let second_end = fx.append(&user_line_for(PROMPT2));
            fx.append(&(item_completed_line_for(PROMPT2) + &answer_lines_with(RESPONSE2)));
            fx.scanned_past(second_end).await;
            let (entered, resume) = fx.park_capture();
            super::super::super::relay_observed_prompt(&fx.shared, first).await;
            let first_anchor = fx.codex.row().expect("first claim").user_msg_id;
            tokio::time::timeout(Duration::from_secs(15), entered.notified())
                .await
                .expect("the first answer's tail reaches bridge capture");
            super::super::super::relay_observed_prompt(&fx.shared, second).await;
            let pending = fx.pending_starts();
            assert!(
                pending.len() == 1 && pending[0].0 == PROMPT2,
                "the second input defers behind the unfinished first: {pending:?}"
            );
            resume.notify_one();
            for text in [RESPONSE, RESPONSE2] {
                assert!(
                    fx.delivered(text).await,
                    "{text} never reached Discord: {:?}",
                    fx.requests.lock().unwrap()
                );
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
            fx.assert_answer(RESPONSE, first_anchor);
            let second_anchor = fx.sent(RESPONSE2)[0].1.rsplit('/').next().unwrap().parse();
            let second_anchor = second_anchor.expect("anchor id");
            assert_ne!(
                second_anchor, first_anchor,
                "the second input has its own anchor"
            );
            fx.assert_answer(RESPONSE2, second_anchor);
            let first_edit = |text: &str| {
                let requests = fx.requests.lock().unwrap();
                requests.iter().position(|(_, _, body)| body.contains(text))
            };
            assert!(
                first_edit(RESPONSE) < first_edit(RESPONSE2),
                "answers keep input order"
            );
            assert_eq!(fx.completed_turns(), 2);
            assert_eq!(tail_starts(fx.codex.channel), 2, "one tail per input");
            fx.assert_released().await;
            fx.finish();
        })
    });
}

/// A Discord-origin prompt lands and is scanned while a direct input waits for its claim.
#[test]
fn codex_direct_answer_keeps_its_boundary_when_a_discord_prompt_lands_first() {
    run(|root| {
        Box::pin(async move {
            let mut fx = Fixture::start(&root, 5_704_920, "AgentDesk-codex-5704-discord").await;
            let first = fx.hook(PROMPT).await;
            fx.append(
                &(user_line_for(PROMPT) + &item_completed_line_for(PROMPT) + &answer_lines()),
            );
            crate::services::tui_prompt_dedupe::record_suppressed_discord_origin_prompt(
                "codex",
                &fx.codex.tmux,
                PROMPT2,
            );
            let discord_end = fx.append(&user_line_for(PROMPT2));
            fx.append(&item_completed_line_for(PROMPT2));
            fx.scanned_past(discord_end).await;
            super::super::super::relay_observed_prompt(&fx.shared, first).await;
            let anchor = fx.codex.row().expect("direct claim").user_msg_id;
            assert!(
                fx.delivered(RESPONSE).await,
                "answer never reached Discord: {:?}",
                fx.requests.lock().unwrap()
            );
            tokio::time::sleep(Duration::from_secs(2)).await;
            fx.assert_answer(RESPONSE, anchor);
            assert_eq!(fx.completed_turns(), 1);
            assert_eq!(
                tail_starts(fx.codex.channel),
                1,
                "no tail for the Discord prompt"
            );
            fx.assert_released().await;
            fx.finish();
        })
    });
}

/// How the next input's records arrive after the first answer's tail started.
#[derive(Clone, Copy)]
enum NextInputWrite {
    /// One append: first answer and terminal, next prompt, next answer.
    Whole,
    /// The tail's 8192-byte read ends inside the next prompt record.
    ReadBoundary,
    /// The next prompt record arrives without its newline first.
    TornNewline,
}

/// The tail stops before the next input's prompt record, however its bytes arrive.
async fn next_input_after_tail_start(
    root: PathBuf,
    channel: u64,
    tmux: &str,
    write: NextInputWrite,
) {
    let mut fx = Fixture::start(&root, channel, tmux).await;
    let first = fx.hook(PROMPT).await;
    let first_end = fx.append(&user_line_for(PROMPT));
    let mut read_from = fx.append(&item_completed_line_for(PROMPT));
    fx.scanned_past(first_end).await;
    super::super::super::relay_observed_prompt(&fx.shared, first).await;
    let anchor = fx.codex.row().expect("first claim").user_msg_id;
    assert!(
        wait_for(Duration::from_secs(5), || tail_starts(fx.codex.channel)
            == 1)
        .await,
        "the first answer's tail never started"
    );
    // The reader now waits at the rollout end.
    tokio::time::sleep(Duration::from_secs(1)).await;
    let second = fx.hook(PROMPT2).await;
    let first_turn = answer_lines();
    let second_prompt = user_line_for(PROMPT2);
    let second_turn = item_completed_line_for(PROMPT2) + &answer_lines_with(RESPONSE2);
    match write {
        NextInputWrite::Whole => {
            fx.append(&(first_turn.clone() + &second_prompt + &second_turn));
        }
        NextInputWrite::ReadBoundary => {
            let pad = padding_line(8192 - first_turn.len() - second_prompt.len() / 2);
            read_from += pad.len() as u64;
            fx.append(&(pad + &first_turn + &second_prompt + &second_turn));
        }
        NextInputWrite::TornNewline => {
            fx.append(&(first_turn.clone() + second_prompt.trim_end_matches('\n')));
            tokio::time::sleep(Duration::from_secs(1)).await;
            fx.append(&("\n".to_string() + &second_turn));
        }
    }
    let second_end = read_from + (first_turn.len() + second_prompt.len()) as u64;
    assert!(
        fx.delivered(RESPONSE).await,
        "answer never reached Discord: {:?}",
        fx.requests.lock().unwrap()
    );
    tokio::time::sleep(Duration::from_secs(2)).await;
    fx.assert_answer(RESPONSE, anchor);
    fx.scanned_past(second_end).await;
    super::super::super::relay_observed_prompt(&fx.shared, second).await;
    fx.assert_answers_in_order(&[RESPONSE, RESPONSE2], 2).await;
    fx.finish();
}

#[test]
fn codex_direct_answer_stops_before_a_next_input_written_in_the_same_append() {
    run(|root| {
        Box::pin(next_input_after_tail_start(
            root,
            5_704_930,
            "AgentDesk-codex-5704-append",
            NextInputWrite::Whole,
        ))
    });
}

#[test]
fn codex_direct_answer_stops_before_a_next_input_split_by_the_read_buffer() {
    run(|root| {
        Box::pin(next_input_after_tail_start(
            root,
            5_704_931,
            "AgentDesk-codex-5704-split",
            NextInputWrite::ReadBoundary,
        ))
    });
}

#[test]
fn codex_direct_answer_stops_before_a_next_input_torn_before_its_newline() {
    run(|root| {
        Box::pin(next_input_after_tail_start(
            root,
            5_704_932,
            "AgentDesk-codex-5704-torn",
            NextInputWrite::TornNewline,
        ))
    });
}

/// The same prompt typed twice, more than the dedupe window apart: each answer stays on
/// the input whose record it follows.
#[test]
fn codex_direct_same_prompt_twice_keeps_each_answer_on_its_own_input() {
    run(|root| {
        Box::pin(async move {
            let mut fx = Fixture::start(&root, 5_704_940, "AgentDesk-codex-5704-same").await;
            let first = fx.hook(PROMPT).await;
            fx.append(&user_line_for(PROMPT));
            let first_turn_end = fx.append(&(item_completed_line_for(PROMPT) + &answer_lines()));
            fx.scanned_past(first_turn_end).await;
            crate::services::tui_prompt_dedupe::age_observed_prompt_records_for_tests(
                "codex",
                &fx.codex.tmux,
                Duration::from_secs(31),
            );
            let second = fx.hook(PROMPT).await;
            let second_end = fx.append(&user_line_for(PROMPT));
            fx.append(&(item_completed_line_for(PROMPT) + &answer_lines_with(RESPONSE2)));
            fx.scanned_past(second_end).await;
            super::super::super::relay_observed_prompt(&fx.shared, first).await;
            super::super::super::relay_observed_prompt(&fx.shared, second).await;
            fx.assert_answers_in_order(&[RESPONSE, RESPONSE2], 2).await;
            fx.finish();
        })
    });
}

/// Sixteen more inputs land and are scanned while the first waits for its claim.
#[test]
fn codex_direct_answer_keeps_its_boundary_behind_a_burst_of_later_inputs() {
    run(|root| {
        Box::pin(async move {
            let mut fx = Fixture::start(&root, 5_704_950, "AgentDesk-codex-5704-burst").await;
            let first = fx.hook(PROMPT).await;
            fx.append(
                &(user_line_for(PROMPT) + &item_completed_line_for(PROMPT) + &answer_lines()),
            );
            for index in 0..16 {
                let text = format!("codex direct prompt 5704 burst {index}");
                fx.hook(&text).await;
                fx.append(&(user_line_for(&text) + &item_completed_line_for(&text)));
            }
            let end = std::fs::metadata(&fx.codex.rollout).unwrap().len();
            fx.scanned_past(end).await;
            super::super::super::relay_observed_prompt(&fx.shared, first).await;
            let anchor = fx.codex.row().expect("first claim").user_msg_id;
            assert!(
                fx.delivered(RESPONSE).await,
                "answer never reached Discord: {:?}",
                fx.requests.lock().unwrap()
            );
            tokio::time::sleep(Duration::from_secs(2)).await;
            fx.assert_answer(RESPONSE, anchor);
            assert_eq!(fx.completed_turns(), 1);
            assert_eq!(tail_starts(fx.codex.channel), 1);
            fx.assert_released().await;
            fx.finish();
        })
    });
}

/// The same prompt published twice before Codex writes either record, the second past
/// the dedupe window: the older observation still owns the first record.
#[test]
fn codex_direct_same_prompt_published_twice_before_its_records_keeps_input_order() {
    run(|root| {
        Box::pin(async move {
            let mut fx = Fixture::start(&root, 5_704_960, "AgentDesk-codex-5704-slow").await;
            let first = fx.hook(PROMPT).await;
            crate::services::tui_prompt_dedupe::age_observed_prompt_records_for_tests(
                "codex",
                &fx.codex.tmux,
                Duration::from_secs(31),
            );
            let second = fx.hook(PROMPT).await;
            fx.append(
                &(user_line_for(PROMPT) + &item_completed_line_for(PROMPT) + &answer_lines()),
            );
            let second_end = fx.append(&user_line_for(PROMPT));
            fx.append(&(item_completed_line_for(PROMPT) + &answer_lines_with(RESPONSE2)));
            fx.scanned_past(second_end).await;
            super::super::super::relay_observed_prompt(&fx.shared, first).await;
            super::super::super::relay_observed_prompt(&fx.shared, second).await;
            fx.assert_answers_in_order(&[RESPONSE, RESPONSE2], 2).await;
            fx.finish();
        })
    });
}

/// Three inputs scanned before any claim; the first parks before capture, so the second
/// defers with its boundary and the third takes the session's latest lease meanwhile.
async fn three_inputs_behind_a_parked_first(fx: &mut Fixture) -> Arc<Notify> {
    let mut events = Vec::new();
    for (prompt, answer) in [
        (PROMPT, RESPONSE),
        (PROMPT2, RESPONSE2),
        (PROMPT3, RESPONSE3),
    ] {
        events.push(fx.hook(prompt).await);
        fx.append(
            &(user_line_for(prompt)
                + &item_completed_line_for(prompt)
                + &answer_lines_with(answer)),
        );
    }
    let end = std::fs::metadata(&fx.codex.rollout).unwrap().len();
    fx.scanned_past(end).await;
    let (entered, resume) = fx.park_capture();
    let mut events = events.into_iter();
    super::super::super::relay_observed_prompt(&fx.shared, events.next().unwrap()).await;
    tokio::time::timeout(Duration::from_secs(15), entered.notified())
        .await
        .expect("the first answer's tail reaches bridge capture");
    super::super::super::relay_observed_prompt(&fx.shared, events.next().unwrap()).await;
    let pending = fx.pending_starts();
    assert!(
        pending.len() == 1 && pending[0].0 == PROMPT2,
        "the second input defers: {pending:?}"
    );
    super::super::super::relay_observed_prompt(&fx.shared, events.next().unwrap()).await;
    let lease = crate::services::tui_prompt_dedupe::external_input_relay_lease(
        "codex",
        &fx.codex.tmux,
        fx.codex.channel.get(),
    )
    .expect("latest lease");
    let pending = fx.pending_starts();
    assert_eq!(pending.len(), 2, "the third input defers too: {pending:?}");
    let second_turn = crate::services::discord::tui_direct_pending_start::load_all()
        .into_iter()
        .find(|record| record.prompt_text == PROMPT2)
        .expect("second input's record")
        .lease_turn_id;
    assert!(
        lease.turn_id.is_some() && lease.turn_id != second_turn,
        "the third input's observer took the session lease"
    );
    resume
}

#[test]
fn codex_direct_deferred_input_keeps_its_boundary_after_a_later_lease() {
    run(|root| {
        Box::pin(async move {
            let mut fx = Fixture::start(&root, 5_704_970, "AgentDesk-codex-5704-three").await;
            let resume = three_inputs_behind_a_parked_first(&mut fx).await;
            resume.notify_one();
            fx.assert_answers_in_order(&[RESPONSE, RESPONSE2, RESPONSE3], 3)
                .await;
            fx.finish();
        })
    });
}

/// The process restarts while two inputs wait deferred: only durable state carries them.
#[test]
fn codex_direct_deferred_inputs_keep_their_boundaries_across_a_restart() {
    let _env_lock = crate::config::shared_test_env_lock()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let root = tempfile::tempdir().expect("isolated root");
    let _root = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "AGENTDESK_ROOT_DIR",
        root.path(),
    );
    let _dedupe = crate::services::tui_prompt_dedupe::TEST_LOCK
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let _boot = crate::services::tui_o::cutover::test_override::force_channels(&[]);
    let (channel, tmux) = (5_704_980, "AgentDesk-codex-5704-restart");
    let runtime = || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime")
    };
    let before = runtime();
    before.block_on(async {
        let mut fx = Fixture::start(root.path(), channel, tmux).await;
        let _resume = three_inputs_behind_a_parked_first(&mut fx).await;
        drop(fx);
    });
    before.shutdown_background();
    // The restart drops every in-memory record; the first turn's row was settled meanwhile.
    *super::super::super::codex_idle_rollout::CAPTURE_PAUSE
        .lock()
        .unwrap() = None;
    inflight::clear_inflight_state(&ProviderKind::Codex, channel);
    crate::services::tui_prompt_dedupe::reset_state_for_tests();
    runtime().block_on(async {
        let mut fx = Fixture::start_with(root.path(), channel, tmux, true).await;
        synthetic_start::restore_pending_starts(&fx.shared, &ProviderKind::Codex);
        fx.assert_answers_in_order(&[RESPONSE2, RESPONSE3], 3).await;
        fx.finish();
    });
}
