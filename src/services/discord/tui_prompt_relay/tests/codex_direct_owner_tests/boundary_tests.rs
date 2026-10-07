//! Each Codex direct input's claim starts at its own prompt's end in the rollout, however
//! far the idle loop's scan cursor moved before the observer claimed.
use super::*;
use crate::services::tui_prompt_dedupe::ObservedTuiPrompt;

const PROMPT2: &str = "codex direct prompt 5704 second";
const RESPONSE2: &str = "DIRECT_5704_SECOND";

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
        enable_session_bound_delivery();
        let mut shared = crate::services::discord::make_shared_data_for_tests();
        // The observer announces direct input through the notify utility bot.
        let registry = Arc::new(crate::services::discord::health::HealthRegistry::new());
        Arc::get_mut(&mut shared)
            .expect("fresh shared")
            .health_registry = Arc::downgrade(&registry);
        let codex = CodexChannel::new(&shared, root, channel, tmux);
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
            wait_for(Duration::from_secs(5), || {
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
            super::super::super::relay_observed_prompt(&fx.shared, first).await;
            let first_anchor = fx.codex.row().expect("first claim").user_msg_id;
            super::super::super::relay_observed_prompt(&fx.shared, second).await;
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
