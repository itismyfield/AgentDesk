//! Each Codex direct input's answer starts at its own prompt's end, however far the scan cursor
//! moved first. Records and hooks carry native turn ids as measured on codex-cli 0.160.1.
use super::*;
use crate::services::claude_tui::hook_server::{HookEvent, HookEventKind};
use crate::services::tui_prompt_dedupe::ObservedTuiPrompt;

const PROMPT2: &str = "codex direct prompt 5704 second";
const RESPONSE2: &str = "DIRECT_5704_SECOND";
const PROMPT3: &str = "codex direct prompt 5704 third";
const RESPONSE3: &str = "DIRECT_5704_THIRD";

fn user_line_for(text: &str) -> String {
    rollout_line(serde_json::json!({"type": "response_item", "payload": {
        "type": "message", "role": "user", "content": [{"type": "input_text", "text": text}]}}))
}

/// A turn's opening records through its prompt: `task_started`, `turn_context`, the prompt.
fn opening(turn: &str, text: &str) -> String {
    rollout_line(serde_json::json!({"type": "event_msg", "payload": {
        "type": "task_started", "turn_id": turn}}))
        + &rollout_line(serde_json::json!({"type": "turn_context", "payload": {"turn_id": turn}}))
        + &user_line_for(text)
}

/// The record Codex writes right after a user record; the scanner reads it as a non-prompt line.
fn item_completed_line_for(turn: &str) -> String {
    rollout_line(
        serde_json::json!({"type": "event_msg", "payload": {"type": "item_completed",
        "turn_id": turn, "item": {"type": "UserMessage"}}}),
    )
}

/// A turn's answer and its `task_complete`.
fn answer_for(turn: &str, text: &str) -> String {
    rollout_line(serde_json::json!({"type": "response_item", "payload": {
        "type": "message", "role": "assistant", "content": [{"type": "output_text", "text": text}]}}))
        + &rollout_line(serde_json::json!({"type": "event_msg", "payload": {
            "type": "task_complete", "turn_id": turn, "last_agent_message": text}}))
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
    hooks: tokio::sync::broadcast::Sender<HookEvent>,
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
        // The production hook observer; the test relays each published event itself.
        let (hooks, hook_rx) = tokio::sync::broadcast::channel(64);
        super::super::super::hook_observer::spawn_tui_prompt_relay_observer_inner(
            "codex".to_string(),
            hook_rx,
            |_| Box::pin(async {}),
            None,
        );
        super::super::super::CODEX_IDLE_ROLLOUT_RELAY_STARTED.store(false, Ordering::Release);
        super::super::super::spawn_codex_idle_rollout_relay(shared.clone());
        Self {
            shared,
            codex,
            requests,
            frames,
            finalized,
            observed,
            hooks,
            _relay: relay,
            _rest: rest,
            _server: server,
            _registry: registry,
        }
    }

    /// The Codex UserPromptSubmit hook for `prompt` in native turn `turn`; a steer names the
    /// running turn.
    fn send_hook(&self, prompt: &str, turn: &str) {
        self.hooks
            .send(HookEvent {
                provider: "codex".to_string(),
                session_id: self.codex.tmux.clone(),
                kind: HookEventKind::UserPromptSubmit,
                received_at: chrono::Utc::now(),
                payload: serde_json::json!({"hook_event_name": "UserPromptSubmit",
                    "session_id": "s-5704", "turn_id": turn, "prompt": prompt}),
                fanout: None,
            })
            .expect("hook observer listening");
    }

    /// The hook and the event it publishes.
    async fn hook(&mut self, prompt: &str, turn: &str) -> ObservedTuiPrompt {
        self.send_hook(prompt, turn);
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

    /// A steer's hook joins its running turn: nothing about `prompt` was or is published.
    async fn joined_hook(&mut self, prompt: &str, turn: &str) {
        self.send_hook(prompt, turn);
        let window = tokio::time::Instant::now() + Duration::from_secs(1);
        while let Ok(event) = tokio::time::timeout_at(window, self.observed.recv()).await {
            let event = event.expect("observation channel open");
            assert!(
                event.prompt != prompt,
                "a steering input published its own observation"
            );
        }
    }

    /// Waits for `n` more idle loop polls of this session.
    async fn polls(&self, n: usize) {
        let start = poll_notes(&self.codex.tmux, PollNote::Visit);
        assert!(
            wait_for(Duration::from_secs(10), || poll_notes(
                &self.codex.tmux,
                PollNote::Visit
            ) >= start + n)
            .await,
            "the idle loop stopped polling"
        );
    }

    /// The session's external-input lease id.
    fn lease_turn(&self) -> Option<String> {
        crate::services::tui_prompt_dedupe::external_input_relay_lease(
            "codex",
            &self.codex.tmux,
            self.codex.channel.get(),
        )
        .and_then(|lease| lease.turn_id)
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
    async fn assert_answers_in_order(&mut self, answers: &[&str], tails: Option<usize>) {
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
        if let Some(tails) = tails {
            assert_eq!(tail_starts(self.codex.channel), tails, "one tail per input");
        }
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

/// A `tmux` on PATH that reports every pane live until `kill_panes`; the caller holds the
/// shared env lock.
fn live_pane_tmux(root: &Path) -> crate::config::TestEnvVarGuard {
    use std::os::unix::fs::PermissionsExt;
    let dir = root.join("fake-tmux");
    std::fs::create_dir_all(&dir).expect("fake tmux dir");
    let tmux = dir.join("tmux");
    std::fs::write(
        &tmux,
        "#!/bin/sh\nwhile [ \"${1#-}\" != \"$1\" ]; do shift; done\ncase \"$1\" in list-panes) [ -f \"$(dirname \"$0\")/dead\" ] && echo 1 || echo 0;; esac\nexit 0\n",
    )
    .expect("fake tmux");
    std::fs::set_permissions(&tmux, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    crate::config::TestEnvVarGuard::prepend_path_after_shared_test_env_lock(&dir)
}

/// Every pane `live_pane_tmux` reports turns dead, or live again.
fn kill_panes(root: &Path, dead: bool) {
    let flag = root.join("fake-tmux").join("dead");
    if dead {
        std::fs::write(flag, "").expect("dead flag");
    } else {
        std::fs::remove_file(flag).expect("dead flag");
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

/// A Codex native turn id shaped like the measured ones; `n` tells turns apart.
fn turn(n: u32) -> String {
    format!("019a5704-{n:04}-7000-8000-000000005704")
}

/// Production's order: hook, prompt and `item_completed` records, the loop polls past both, then
/// the observer claims; the answer may land on either side of the claim.
async fn cursor_past_prompt(root: PathBuf, channel: u64, tmux: &str, answer_first: bool) {
    let mut fx = Fixture::start(&root, channel, tmux).await;
    let t1 = turn(1);
    let event = fx.hook(PROMPT, &t1).await;
    let prompt_end = fx.append(&opening(&t1, PROMPT));
    let mut after = item_completed_line_for(&t1);
    if answer_first {
        after += &answer_for(&t1, RESPONSE);
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
        fx.append(&answer_for(&t1, RESPONSE));
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
            let (t1, t2) = (turn(1), turn(2));
            let first = fx.hook(PROMPT, &t1).await;
            fx.append(
                &(opening(&t1, PROMPT)
                    + &item_completed_line_for(&t1)
                    + &answer_for(&t1, RESPONSE)),
            );
            let second = fx.hook(PROMPT2, &t2).await;
            let second_end = fx.append(&opening(&t2, PROMPT2));
            fx.append(&(item_completed_line_for(&t2) + &answer_for(&t2, RESPONSE2)));
            fx.scanned_past(second_end).await;
            super::super::super::relay_observed_prompt(&fx.shared, first).await;
            let first_anchor = fx.codex.row().expect("first claim").user_msg_id;
            super::super::super::relay_observed_prompt(&fx.shared, second).await;
            let pending = fx.pending_starts();
            assert!(
                pending.len() == 1 && pending[0].0 == PROMPT2,
                "the second input defers behind the unfinished first: {pending:?}"
            );
            let second_anchor = pending[0].1;
            assert_ne!(
                second_anchor, first_anchor,
                "the second input has its own anchor"
            );
            fx.assert_answers_in_order(&[RESPONSE, RESPONSE2], Some(2))
                .await;
            assert_eq!(fx.anchor_of(RESPONSE), first_anchor);
            assert_eq!(fx.anchor_of(RESPONSE2), second_anchor);
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
            let (t1, t2) = (turn(1), turn(2));
            let first = fx.hook(PROMPT, &t1).await;
            fx.append(
                &(opening(&t1, PROMPT)
                    + &item_completed_line_for(&t1)
                    + &answer_for(&t1, RESPONSE)),
            );
            crate::services::tui_prompt_dedupe::record_suppressed_discord_origin_prompt(
                "codex",
                &fx.codex.tmux,
                PROMPT2,
            );
            let discord_end = fx.append(&opening(&t2, PROMPT2));
            fx.append(&item_completed_line_for(&t2));
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
    /// One append: first answer and terminal, next opening, next answer.
    Whole,
    /// The tail's 8192-byte read ends inside the next prompt record.
    ReadBoundary,
    /// The next prompt record arrives without its newline first.
    TornNewline,
    /// The first turn is interrupted with its answer partly written; the next turn opens.
    Interrupted,
    /// The first turn writes no terminal at all before the next turn opens.
    Unclosed,
    /// As `Interrupted`, then a late completion of the aborted turn inside the next one.
    LateForeignComplete,
}

/// The tail stops before the next turn's records, however its bytes arrive.
async fn next_input_after_tail_start(
    root: PathBuf,
    channel: u64,
    tmux: &str,
    write: NextInputWrite,
) {
    // A pane that stays live keeps the tail waiting at the rollout end for the next append.
    let _live_pane = live_pane_tmux(&root);
    let mut fx = Fixture::start(&root, channel, tmux).await;
    let (t1, t2) = (turn(1), turn(2));
    let first = fx.hook(PROMPT, &t1).await;
    let first_end = fx.append(&opening(&t1, PROMPT));
    let mut read_from = fx.append(&item_completed_line_for(&t1));
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
    let first_turn = match write {
        // Measured interrupt: a developer notice and `turn_aborted` close the first turn.
        NextInputWrite::Interrupted | NextInputWrite::LateForeignComplete => {
            rollout_line(serde_json::json!({"type": "response_item", "payload": {
                "type": "message", "role": "assistant",
                "content": [{"type": "output_text", "text": RESPONSE}]}}))
                + &token_count_line()
                + &rollout_line(serde_json::json!({"type": "response_item", "payload": {
                    "type": "message", "role": "developer",
                    "content": [{"type": "input_text", "text": "<turn_aborted>\ninterrupted\n</turn_aborted>"}]}}))
                + &rollout_line(serde_json::json!({"type": "event_msg", "payload": {
                    "type": "turn_aborted", "turn_id": t1, "reason": "interrupted"}}))
                + &rollout_line(serde_json::json!({"type": "event_msg", "payload": {
                    "type": "thread_settings_applied"}}))
        }
        NextInputWrite::Unclosed => rollout_line(serde_json::json!({"type": "response_item",
            "payload": {"type": "message", "role": "assistant",
                "content": [{"type": "output_text", "text": RESPONSE}]}})),
        _ => answer_for(&t1, RESPONSE),
    };
    let second_prompt = opening(&t2, PROMPT2);
    let mut second_turn = item_completed_line_for(&t2);
    if matches!(write, NextInputWrite::LateForeignComplete) {
        second_turn += &rollout_line(serde_json::json!({"type": "event_msg", "payload": {
            "type": "task_complete", "turn_id": t1, "last_agent_message": OLD_RESPONSE}}));
    }
    second_turn += &answer_for(&t2, RESPONSE2);
    if matches!(write, NextInputWrite::Interrupted) {
        // Measured: the aborted turn's command completes after the next turn's completion.
        second_turn += &rollout_line(serde_json::json!({"type": "event_msg", "payload": {
            "type": "item_completed", "turn_id": t1, "item": {"type": "CommandExecution"}}}));
    }
    match write {
        NextInputWrite::Whole
        | NextInputWrite::Interrupted
        | NextInputWrite::Unclosed
        | NextInputWrite::LateForeignComplete => {
            fx.append(&(first_turn.clone() + &second_prompt + &second_turn));
        }
        NextInputWrite::ReadBoundary => {
            // Split halfway into the prompt record, past the new turn's opening events.
            let split = second_prompt.len() - user_line_for(PROMPT2).len() / 2;
            let pad = padding_line(8192 - first_turn.len() - split);
            read_from += pad.len() as u64;
            fx.append(&(pad + &first_turn + &second_prompt + &second_turn));
        }
        NextInputWrite::TornNewline => {
            fx.append(&(first_turn.clone() + second_prompt.trim_end_matches('\n')));
            // Long enough for the tail's 100 ms end-of-file polls, shorter than the first
            // turn's release, so only the tail sees the torn record.
            tokio::time::sleep(Duration::from_millis(300)).await;
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
    // The second input's hook lands after the loop scanned its record, so the event the
    // loop published is the one the observer takes.
    fx.scanned_past(second_end).await;
    let second = fx.hook(PROMPT2, &t2).await;
    super::super::super::relay_observed_prompt(&fx.shared, second).await;
    fx.assert_answers_in_order(&[RESPONSE, RESPONSE2], Some(2))
        .await;
    assert!(
        fx.sent(OLD_RESPONSE).is_empty(),
        "a foreign completion's text leaked"
    );
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

#[test]
fn codex_direct_answer_never_carries_a_next_turn_when_its_own_turn_never_closed() {
    run(|root| {
        Box::pin(next_input_after_tail_start(
            root,
            5_704_934,
            "AgentDesk-codex-5704-unclosed",
            NextInputWrite::Unclosed,
        ))
    });
}

#[test]
fn codex_direct_answer_ends_at_its_own_interrupted_turn() {
    run(|root| {
        Box::pin(next_input_after_tail_start(
            root,
            5_704_933,
            "AgentDesk-codex-5704-interrupted",
            NextInputWrite::Interrupted,
        ))
    });
}

#[test]
fn codex_direct_answer_ignores_a_late_completion_of_an_aborted_turn() {
    run(|root| {
        Box::pin(next_input_after_tail_start(
            root,
            5_704_935,
            "AgentDesk-codex-5704-late-complete",
            NextInputWrite::LateForeignComplete,
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
            let (t1, t2) = (turn(1), turn(2));
            let first = fx.hook(PROMPT, &t1).await;
            fx.append(&opening(&t1, PROMPT));
            let first_turn_end =
                fx.append(&(item_completed_line_for(&t1) + &answer_for(&t1, RESPONSE)));
            fx.scanned_past(first_turn_end).await;
            crate::services::tui_prompt_dedupe::age_observed_prompt_records_for_tests(
                "codex",
                &fx.codex.tmux,
                Duration::from_secs(31),
            );
            let second = fx.hook(PROMPT, &t2).await;
            let second_end = fx.append(&opening(&t2, PROMPT));
            fx.append(&(item_completed_line_for(&t2) + &answer_for(&t2, RESPONSE2)));
            fx.scanned_past(second_end).await;
            super::super::super::relay_observed_prompt(&fx.shared, first).await;
            let first_anchor = fx.codex.row().expect("first claim").user_msg_id;
            super::super::super::relay_observed_prompt(&fx.shared, second).await;
            let second_anchor = fx.pending_starts()[0].1;
            fx.assert_answers_in_order(&[RESPONSE, RESPONSE2], Some(2))
                .await;
            assert_eq!(fx.anchor_of(RESPONSE), first_anchor);
            assert_eq!(fx.anchor_of(RESPONSE2), second_anchor);
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
            let t1 = turn(1);
            let first = fx.hook(PROMPT, &t1).await;
            fx.append(
                &(opening(&t1, PROMPT)
                    + &item_completed_line_for(&t1)
                    + &answer_for(&t1, RESPONSE)),
            );
            for index in 0..16 {
                let (text, id) = (
                    format!("codex direct prompt 5704 burst {index}"),
                    turn(10 + index),
                );
                fx.hook(&text, &id).await;
                fx.append(&(opening(&id, &text) + &item_completed_line_for(&id)));
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
            let (t1, t2) = (turn(1), turn(2));
            let first = fx.hook(PROMPT, &t1).await;
            crate::services::tui_prompt_dedupe::age_observed_prompt_records_for_tests(
                "codex",
                &fx.codex.tmux,
                Duration::from_secs(31),
            );
            let second = fx.hook(PROMPT, &t2).await;
            fx.append(
                &(opening(&t1, PROMPT)
                    + &item_completed_line_for(&t1)
                    + &answer_for(&t1, RESPONSE)),
            );
            let second_end = fx.append(&opening(&t2, PROMPT));
            fx.append(&(item_completed_line_for(&t2) + &answer_for(&t2, RESPONSE2)));
            fx.scanned_past(second_end).await;
            super::super::super::relay_observed_prompt(&fx.shared, first).await;
            let first_anchor = fx.codex.row().expect("first claim").user_msg_id;
            super::super::super::relay_observed_prompt(&fx.shared, second).await;
            let second_anchor = fx.pending_starts()[0].1;
            fx.assert_answers_in_order(&[RESPONSE, RESPONSE2], Some(2))
                .await;
            assert_eq!(fx.anchor_of(RESPONSE), first_anchor);
            assert_eq!(fx.anchor_of(RESPONSE2), second_anchor);
            fx.finish();
        })
    });
}

/// An input claimed before its record keeps its claim start; the same prompt again past the
/// dedupe window and scanned first still answers on the second anchor.
#[test]
fn codex_direct_same_prompt_after_a_claim_first_input_keeps_its_own_boundary() {
    run(|root| {
        Box::pin(async move {
            let _live_pane = live_pane_tmux(&root);
            let mut fx = Fixture::start(&root, 5_704_980, "AgentDesk-codex-5704-claimfirst").await;
            let (t1, t2) = (turn(1), turn(2));
            let first = fx.hook(PROMPT, &t1).await;
            super::super::super::relay_observed_prompt(&fx.shared, first).await;
            let claimed = fx.codex.row().expect("first claim");
            let (first_anchor, claim_start) = (claimed.user_msg_id, claimed.turn_start_offset);
            let prompt_end = fx.append(&opening(&t1, PROMPT));
            fx.append(&item_completed_line_for(&t1));
            assert!(
                wait_for(Duration::from_secs(10), || tail_starts(fx.codex.channel)
                    == 1)
                .await,
                "the claimed input's tail never started"
            );
            // The bridge witness pins the claim start, so the tail reads from the prompt end
            // while the row keeps that start.
            let repaired = fx.codex.row().expect("repaired row");
            assert!(claim_start.is_some_and(|start| start < prompt_end));
            assert_eq!(repaired.turn_start_offset, claim_start);
            fx.append(&answer_for(&t1, RESPONSE));
            assert!(
                fx.delivered(RESPONSE).await,
                "first answer never reached Discord"
            );
            assert!(
                wait_for(Duration::from_secs(10), || fx.codex.row().is_none()).await,
                "first turn never released"
            );
            crate::services::tui_prompt_dedupe::age_observed_prompt_records_for_tests(
                "codex",
                &fx.codex.tmux,
                Duration::from_secs(31),
            );
            let second = fx.hook(PROMPT, &t2).await;
            fx.append(&opening(&t2, PROMPT));
            let end = fx.append(&(item_completed_line_for(&t2) + &answer_for(&t2, RESPONSE2)));
            fx.scanned_past(end).await;
            super::super::super::relay_observed_prompt(&fx.shared, second).await;
            let second_anchor = fx.codex.row().map(|row| row.user_msg_id);
            fx.assert_answers_in_order(&[RESPONSE, RESPONSE2], None)
                .await;
            assert_eq!(fx.anchor_of(RESPONSE), first_anchor);
            assert_ne!(fx.anchor_of(RESPONSE2), first_anchor);
            if let Some(second_anchor) = second_anchor {
                assert_eq!(fx.anchor_of(RESPONSE2), second_anchor);
            }
            fx.finish();
        })
    });
}

/// The first turn ends empty while the same prompt waits deferred and is claimed before its
/// record: the second answer still reaches the second anchor.
#[test]
fn codex_direct_same_prompt_deferred_behind_an_empty_turn_keeps_its_own_boundary() {
    run(|root| {
        Box::pin(async move {
            let _live_pane = live_pane_tmux(&root);
            let mut fx = Fixture::start(&root, 5_704_990, "AgentDesk-codex-5704-empty").await;
            let (t1, t2) = (turn(1), turn(2));
            let first = fx.hook(PROMPT, &t1).await;
            let first_end = fx.append(&(opening(&t1, PROMPT) + &item_completed_line_for(&t1)));
            fx.scanned_past(first_end).await;
            super::super::super::relay_observed_prompt(&fx.shared, first).await;
            let first_anchor = fx.codex.row().expect("first claim").user_msg_id;
            crate::services::tui_prompt_dedupe::age_observed_prompt_records_for_tests(
                "codex",
                &fx.codex.tmux,
                Duration::from_secs(31),
            );
            let second = fx.hook(PROMPT, &t2).await;
            super::super::super::relay_observed_prompt(&fx.shared, second).await;
            let pending = fx.pending_starts();
            assert_eq!(pending.len(), 1, "the second input defers: {pending:?}");
            let second_anchor = pending[0].1;
            // The first turn's pane dies with no answer; the pane is back before the next claim.
            kill_panes(&root, true);
            assert!(
                wait_for(Duration::from_secs(10), || fx
                    .codex
                    .row()
                    .is_none_or(|row| row.user_msg_id != first_anchor))
                .await,
                "the first turn never ended"
            );
            kill_panes(&root, false);
            assert!(
                wait_for(Duration::from_secs(20), || fx
                    .codex
                    .row()
                    .is_some_and(|row| row.user_msg_id == second_anchor))
                .await,
                "the deferred input was never claimed"
            );
            fx.append(
                &(opening(&t2, PROMPT)
                    + &item_completed_line_for(&t2)
                    + &answer_for(&t2, RESPONSE2)),
            );
            assert!(
                fx.delivered(RESPONSE2).await,
                "second answer never reached Discord: {:?}",
                fx.requests.lock().unwrap()
            );
            tokio::time::sleep(Duration::from_secs(2)).await;
            fx.assert_answer(RESPONSE2, second_anchor);
            assert_ne!(second_anchor, first_anchor);
            fx.assert_released().await;
            fx.finish();
        })
    });
}

/// A deferred input's durable record from before a restart, while a later input already
/// holds the session lease: the restored worker starts that input at its own prompt end.
#[test]
fn codex_direct_deferred_input_restored_under_a_later_lease_keeps_its_boundary() {
    run(|root| {
        Box::pin(async move {
            let (channel, tmux) = (5_704_970_u64, "AgentDesk-codex-5704-restored");
            let (t1, t2, t3) = (turn(1), turn(2), turn(3));
            // The earlier process delivered the first input; the second and third waited.
            let rollout = root.join(format!("{tmux}-rollout.jsonl"));
            let mut body = rollout_line(
                serde_json::json!({"type": "session_meta", "payload": {"id": "s-5704"}}),
            ) + &opening(&t1, PROMPT)
                + &item_completed_line_for(&t1)
                + &answer_for(&t1, RESPONSE)
                + &opening(&t2, PROMPT2);
            let second_end = body.len() as u64;
            body += &(item_completed_line_for(&t2)
                + &answer_for(&t2, RESPONSE2)
                + &opening(&t3, PROMPT3)
                + &item_completed_line_for(&t3)
                + &answer_for(&t3, RESPONSE3));
            std::fs::write(&rollout, &body).expect("rollout");
            let fx = Fixture::start_with(&root, channel, tmux, true).await;
            let lease_turn = |n: u8| format!("external:codex:{channel}:{tmux}:{n}");
            let anchor = 5_704_970_002_u64;
            let record = serde_json::from_value::<
                crate::services::discord::tui_direct_pending_start::TuiDirectPendingStart,
            >(serde_json::json!({
                "provider": "codex",
                "channel_id": channel,
                "tmux_session_name": tmux,
                "prompt_text": PROMPT2,
                "anchor_message_id": anchor,
                "lease_relay_owner": "bridge_adapter",
                "lease_runtime_kind": "codex_tui",
                "lease_turn_id": lease_turn(2),
                "lease_session_key": null,
                "generation": fx.shared.restart.current_generation,
                "created_at_ms": 1,
                "observed_at_ms": 1,
                "state": "Waiting",
                "attempt_count": 0,
                "captured_source": null,
                "native_turn_id": t2,
            }))
            .expect("pending record");
            crate::services::discord::tui_direct_pending_start::persist(&record).expect("persist");
            let mut later = ExternalInputRelayLease::unassigned(Some(channel));
            later.turn_id = Some(lease_turn(3));
            later.relay_owner =
                crate::services::tui_prompt_dedupe::ExternalInputRelayOwner::BridgeAdapter;
            later.runtime_kind = Some(RuntimeHandoffKind::CodexTui);
            crate::services::tui_prompt_dedupe::record_external_input_turn_lease(
                "codex", tmux, later,
            );
            synthetic_start::restore_pending_starts(&fx.shared, &ProviderKind::Codex);
            // The restored claim starts at the second prompt's end. Discord delivery on the
            // restore path stays unresolved in this fixture, so the boundary is the check.
            assert!(
                wait_for(Duration::from_secs(15), || fx.codex.row().is_some()).await,
                "the restored input was never claimed"
            );
            let row = fx.codex.row().expect("restored row");
            assert_eq!(row.user_msg_id, anchor);
            assert_eq!(row.turn_start_offset, Some(second_end));
            tokio::time::sleep(Duration::from_secs(2)).await;
            for other in [RESPONSE, RESPONSE3] {
                assert!(
                    fx.sent(other).is_empty(),
                    "{other} belongs to no restored input"
                );
            }
            fx.finish();
        })
    });
}

/// Native turn identity: fallback, steering joins, durable deferral.
mod native_turn_tests;
